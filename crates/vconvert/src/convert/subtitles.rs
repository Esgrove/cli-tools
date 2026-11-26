//! External subtitle discovery and matching for video convert movie mode.
//!
//! Finds supported subtitle sidecars, pairs `VobSub` index and data files,
//! and assigns each subtitle to the one video in its directory whose title matches best.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use super::VideoConvert;
use crate::helpers::path_without_extension;
use crate::observer::SubtitleMatchProblem;
use crate::options::ALL_EXTENSIONS;
use crate::types::{SubtitleFile, VideoFile, movie_subtitle_match_score};

impl VideoConvert<'_> {
    /// Gather subtitle sidecars from the directories containing the given video files.
    pub(super) fn gather_subtitle_files_for_video_files(video_files: &[VideoFile]) -> Vec<SubtitleFile> {
        let mut subtitle_files = Vec::new();
        let mut seen_directories = HashSet::new();

        for video_file in video_files {
            let Some(parent) = video_file.path.parent() else {
                continue;
            };
            if seen_directories.insert(parent.to_path_buf()) {
                subtitle_files.extend(Self::gather_subtitle_files_from_root(parent, 1));
            }
        }

        subtitle_files
    }

    /// Gather supported subtitle sidecar files below a root path.
    fn gather_subtitle_files_from_root(root: &Path, max_depth: usize) -> Vec<SubtitleFile> {
        let subtitle_paths: Vec<PathBuf> = WalkDir::new(root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|entry| !cli_tools_common::should_skip_entry(entry))
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(walkdir::DirEntry::into_path)
            .filter(|path| SubtitleFile::is_supported_extension(&cli_tools_common::path_to_file_extension_string(path)))
            .collect();

        let idx_stems: HashSet<PathBuf> = subtitle_paths
            .iter()
            .filter(|path| cli_tools_common::path_to_file_extension_string(path) == "idx")
            .map(|path| path_without_extension(path))
            .collect();

        let sub_by_stem: HashMap<PathBuf, PathBuf> = subtitle_paths
            .iter()
            .filter(|path| cli_tools_common::path_to_file_extension_string(path) == "sub")
            .map(|path| (path_without_extension(path), path.clone()))
            .collect();

        let mut subtitle_files: Vec<SubtitleFile> = subtitle_paths
            .into_iter()
            .filter_map(|path| {
                let extension = cli_tools_common::path_to_file_extension_string(&path);
                if extension == "sub" && idx_stems.contains(&path_without_extension(&path)) {
                    return None;
                }
                let paired_sub_path = if extension == "idx" {
                    sub_by_stem.get(&path_without_extension(&path)).cloned()
                } else {
                    None
                };
                Some(SubtitleFile::new(&path, paired_sub_path))
            })
            .collect();
        subtitle_files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        subtitle_files
    }

    /// Match subtitle sidecars to video files in the same directory using normalized title tokens.
    ///
    /// Scoring considers every known video file present in the same directory, not only the active batch,
    /// so subtitles belonging to already-converted or filtered sibling videos are never stolen.
    /// A subtitle without a matching video, or with several equally good matches, is reported and left out.
    pub(super) fn match_subtitle_files(
        &self,
        video_files: &[VideoFile],
        subtitle_files: Vec<SubtitleFile>,
    ) -> HashMap<PathBuf, Vec<SubtitleFile>> {
        let active_paths: HashSet<PathBuf> = video_files.iter().map(|video| video.path.clone()).collect();
        let all_videos = Self::collect_all_sibling_videos(video_files);
        let mut matches: HashMap<PathBuf, Vec<SubtitleFile>> = HashMap::new();

        for subtitle_file in subtitle_files {
            let subtitle_stem = cli_tools_common::path_to_file_stem_string(&subtitle_file.path);
            let mut candidates: Vec<(usize, &VideoFile)> = all_videos
                .iter()
                .filter(|video_file| video_file.path.parent() == subtitle_file.path.parent())
                .filter_map(|video_file| {
                    movie_subtitle_match_score(&video_file.name, &subtitle_stem).map(|score| (score, video_file))
                })
                .collect();

            candidates.sort_unstable_by_key(|(score, _)| std::cmp::Reverse(*score));
            let Some((best_score, best_video)) = candidates.first() else {
                self.observer
                    .subtitle_not_matched(&subtitle_file.path, SubtitleMatchProblem::NoMatchingVideo);
                continue;
            };

            if candidates.iter().filter(|(score, _)| score == best_score).count() > 1 {
                self.observer
                    .subtitle_not_matched(&subtitle_file.path, SubtitleMatchProblem::Ambiguous);
                continue;
            }

            if active_paths.contains(&best_video.path) {
                matches.entry(best_video.path.clone()).or_default().push(subtitle_file);
            }
        }

        matches
    }

    /// Combine the active batch with all other known video files sitting in the same directories.
    fn collect_all_sibling_videos(active_videos: &[VideoFile]) -> Vec<VideoFile> {
        let mut all_videos: Vec<VideoFile> = active_videos.to_vec();
        let mut seen_paths: HashSet<PathBuf> = active_videos.iter().map(|video| video.path.clone()).collect();
        let mut seen_directories: HashSet<PathBuf> = HashSet::new();

        for video in active_videos {
            let Some(parent) = video.path.parent() else {
                continue;
            };
            if parent.as_os_str().is_empty() || !seen_directories.insert(parent.to_path_buf()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(parent) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
                    continue;
                }
                let path = entry.path();
                let extension = cli_tools_common::path_to_file_extension_string(&path);
                if ALL_EXTENSIONS.contains(&extension.as_str()) && seen_paths.insert(path.clone()) {
                    all_videos.push(VideoFile::new(&path, 0));
                }
            }
        }

        all_videos
    }
}

#[cfg(test)]
mod test_subtitle_discovery {
    use super::*;

    #[test]
    fn gathers_supported_files_and_combines_vobsub_pairs() {
        let temp_directory = tempfile::TempDir::new().expect("Failed to create temp directory");
        let root = temp_directory.path().join("root");
        std::fs::create_dir(&root).expect("Failed to create subtitle root directory");
        let idx_path = root.join("movie.idx");
        let paired_sub_path = root.join("movie.sub");
        let srt_path = root.join("movie.srt");
        let standalone_sub_path = root.join("standalone.sub");

        for path in [&idx_path, &paired_sub_path, &srt_path, &standalone_sub_path] {
            std::fs::write(path, "subtitle").expect("Failed to create subtitle fixture");
        }
        std::fs::write(root.join("ignored.ass"), "subtitle").expect("Failed to create unsupported fixture");

        let subtitle_files = VideoConvert::gather_subtitle_files_from_root(&root, 1);

        assert_eq!(subtitle_files.len(), 3);
        assert_eq!(subtitle_files[0], SubtitleFile::new(&idx_path, Some(paired_sub_path)));
        assert_eq!(subtitle_files[1], SubtitleFile::new(&srt_path, None));
        assert_eq!(subtitle_files[2], SubtitleFile::new(&standalone_sub_path, None));
    }

    #[test]
    fn pairs_uppercase_idx_and_sub_sidecars_preserving_actual_case() {
        let temp_directory = tempfile::TempDir::new().expect("temp dir");
        let idx_path = temp_directory.path().join("Movie.IDX");
        let sub_path = temp_directory.path().join("Movie.SUB");
        std::fs::write(&idx_path, "idx").expect("write idx");
        std::fs::write(&sub_path, "sub").expect("write sub");

        let subtitle_files = VideoConvert::gather_subtitle_files_from_root(temp_directory.path(), 1);
        assert_eq!(subtitle_files.len(), 1);
        assert_eq!(subtitle_files[0], SubtitleFile::new(&idx_path, Some(sub_path)));
    }

    #[test]
    fn respects_maximum_walk_depth() {
        let temp_directory = tempfile::TempDir::new().expect("Failed to create temp directory");
        let root = temp_directory.path().join("root");
        let nested_directory = root.join("nested");
        std::fs::create_dir_all(&nested_directory).expect("Failed to create nested directory");
        let root_subtitle = root.join("root.srt");
        let nested_subtitle = nested_directory.join("nested.srt");
        std::fs::write(&root_subtitle, "subtitle").expect("Failed to create root subtitle fixture");
        std::fs::write(&nested_subtitle, "subtitle").expect("Failed to create nested subtitle fixture");

        let shallow_files = VideoConvert::gather_subtitle_files_from_root(&root, 1);
        let recursive_files = VideoConvert::gather_subtitle_files_from_root(&root, usize::MAX);

        assert_eq!(shallow_files, vec![SubtitleFile::new(&root_subtitle, None)]);
        assert_eq!(
            recursive_files,
            vec![
                SubtitleFile::new(&nested_subtitle, None),
                SubtitleFile::new(&root_subtitle, None),
            ]
        );
    }

    #[test]
    fn scans_each_video_directory_only_once() {
        let temp_directory = tempfile::TempDir::new().expect("Failed to create temp directory");
        let first_directory = temp_directory.path().join("first");
        let second_directory = temp_directory.path().join("second");
        std::fs::create_dir(&first_directory).expect("Failed to create first directory");
        std::fs::create_dir(&second_directory).expect("Failed to create second directory");
        let first_subtitle = first_directory.join("first.srt");
        let second_subtitle = second_directory.join("second.srt");
        std::fs::write(&first_subtitle, "subtitle").expect("Failed to create first subtitle fixture");
        std::fs::write(&second_subtitle, "subtitle").expect("Failed to create second subtitle fixture");
        let video_files = vec![
            VideoFile::new(&first_directory.join("first.mp4"), 0),
            VideoFile::new(&first_directory.join("alternate.mp4"), 0),
            VideoFile::new(&second_directory.join("second.mkv"), 0),
        ];

        let subtitle_files = VideoConvert::gather_subtitle_files_for_video_files(&video_files);
        let paths: HashSet<&Path> = subtitle_files.iter().map(|file| file.path.as_path()).collect();

        assert_eq!(subtitle_files.len(), 2);
        assert_eq!(
            paths,
            HashSet::from([first_subtitle.as_path(), second_subtitle.as_path()])
        );
    }
}

#[cfg(test)]
mod test_subtitle_matching {
    use super::super::test_helpers::converter;
    use super::*;
    use crate::options::ConvertOptions;

    #[test]
    fn assigns_multiple_language_subtitles_to_the_same_video() {
        let video = VideoFile::new(Path::new("movies/Movie.Title.2024.1080p.mp4"), 0);
        let english = SubtitleFile::new(Path::new("movies/Movie.Title.2024.English.srt"), None);
        let finnish = SubtitleFile::new(Path::new("movies/Movie.Title.2024.Finnish.srt"), None);

        let matches = converter(ConvertOptions::default())
            .match_subtitle_files(std::slice::from_ref(&video), vec![english.clone(), finnish.clone()]);

        assert_eq!(matches.get(&video.path), Some(&vec![english, finnish]));
    }

    #[test]
    fn selects_the_unique_highest_scoring_video() {
        let exact_video = VideoFile::new(Path::new("movies/Movie.Title.2024.mp4"), 0);
        let extended_video = VideoFile::new(Path::new("movies/Movie.Title.2024.Extended.mkv"), 0);
        let subtitle = SubtitleFile::new(Path::new("movies/Movie.Title.2024.srt"), None);

        let matches = converter(ConvertOptions::default())
            .match_subtitle_files(&[extended_video, exact_video.clone()], vec![subtitle.clone()]);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches.get(&exact_video.path), Some(&vec![subtitle]));
    }

    #[test]
    fn skips_ambiguous_best_matches() {
        let high_resolution = VideoFile::new(Path::new("movies/Movie.Title.2024.2160p.mkv"), 0);
        let standard_resolution = VideoFile::new(Path::new("movies/Movie.Title.2024.1080p.mp4"), 0);
        let subtitle = SubtitleFile::new(Path::new("movies/Movie.Title.2024.English.srt"), None);

        let matches = converter(ConvertOptions::default())
            .match_subtitle_files(&[high_resolution, standard_resolution], vec![subtitle]);

        assert!(matches.is_empty());
    }

    #[test]
    fn requires_a_related_video_in_the_same_directory() {
        let video = VideoFile::new(Path::new("movies/Movie.Title.2024.mp4"), 0);
        let unrelated = SubtitleFile::new(Path::new("movies/Other.Title.2024.srt"), None);
        let other_directory = SubtitleFile::new(Path::new("subtitles/Movie.Title.2024.srt"), None);

        let matches =
            converter(ConvertOptions::default()).match_subtitle_files(&[video], vec![unrelated, other_directory]);

        assert!(matches.is_empty());
    }

    #[test]
    fn does_not_steal_subtitle_belonging_to_sibling_video_outside_batch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let active_path = dir.path().join("Alien.1979.mkv");
        let sibling_path = dir.path().join("Alien.1979.Directors.Cut.x265.mkv");
        let sibling_subtitle_path = dir.path().join("Alien.1979.Directors.Cut.srt");
        std::fs::write(&active_path, b"video").expect("write active");
        std::fs::write(&sibling_path, b"video").expect("write sibling");
        std::fs::write(&sibling_subtitle_path, b"srt").expect("write srt");

        let active_video = VideoFile::new_with_metadata(&active_path);
        let subtitle = SubtitleFile::new(&sibling_subtitle_path, None);
        let matches = converter(ConvertOptions::default()).match_subtitle_files(&[active_video], vec![subtitle]);

        assert!(
            matches.is_empty(),
            "subtitle belonging to sibling video outside the active batch must not be stolen: {matches:?}"
        );
    }
}
