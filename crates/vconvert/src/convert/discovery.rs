//! Input file discovery for video convert.
//!
//! Walks the configured input paths and collects the video files that pass the extension and name filters.

use std::collections::HashSet;
use std::time::Instant;

use anyhow::Result;
use walkdir::WalkDir;

use super::VideoConvert;
use crate::classification::should_include_file;
use crate::types::VideoFile;

impl VideoConvert {
    /// Gather video files based on the config settings.
    pub(super) fn gather_files_to_process(&self) -> Result<Vec<VideoFile>> {
        let start = Instant::now();
        let max_depth = if self.config.recurse { usize::MAX } else { 1 };
        let mut files = Vec::new();
        let mut seen_paths = HashSet::new();

        for path in &self.config.paths {
            if path.is_file() {
                let file = VideoFile::new_with_metadata(path);
                if should_include_file(&self.config, &file) && seen_paths.insert(file.path.clone()) {
                    files.push(file);
                }
                continue;
            }

            if !path.is_dir() {
                anyhow::bail!("Input path '{}' does not exist or is not accessible", path.display());
            }

            files.extend(
                WalkDir::new(path)
                    .max_depth(max_depth)
                    .into_iter()
                    .filter_entry(|entry| !cli_tools_common::should_skip_entry(entry))
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| entry.file_type().is_file())
                    .map(VideoFile::from)
                    .filter(|file| should_include_file(&self.config, file))
                    .filter(|file| seen_paths.insert(file.path.clone())),
            );
        }

        files.sort_unstable();

        self.log_gathered_files(files.len(), start.elapsed());

        Ok(files)
    }
}

#[cfg(test)]
mod test_gather_files {
    use std::path::{Path, PathBuf};

    use super::super::test_helpers::converter;
    use super::*;
    use crate::config::Config;

    fn config_for(path: &Path, recurse: bool) -> Config {
        Config {
            paths: vec![path.to_path_buf()],
            recurse,
            extensions: vec!["mkv".to_string(), "mp4".to_string()],
            ..Config::default()
        }
    }

    #[test]
    fn gathers_matching_extensions_and_respects_recursion() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let root = directory.path().join("videos");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("Failed to create nested directory");
        std::fs::write(root.join("top.mkv"), b"video").expect("Failed to write video file");
        std::fs::write(root.join("notes.txt"), b"notes").expect("Failed to write text file");
        std::fs::write(nested.join("deep.mp4"), b"video").expect("Failed to write nested video file");

        let (_log, shallow) = converter(config_for(&root, false));
        let (_log_recursive, recursive) = converter(config_for(&root, true));

        let shallow_names: Vec<String> = shallow
            .gather_files_to_process()
            .expect("Failed to gather files")
            .into_iter()
            .map(|file| file.name)
            .collect();
        let recursive_count = recursive
            .gather_files_to_process()
            .expect("Failed to gather files")
            .len();

        assert_eq!(shallow_names, vec!["top"]);
        assert_eq!(recursive_count, 2);
    }

    #[test]
    fn gathers_multiple_files_and_directories_without_duplicates() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let first_root = directory.path().join("first");
        let second_root = directory.path().join("second");
        let nested_root = first_root.join("nested");
        std::fs::create_dir_all(&nested_root).expect("Failed to create nested directory");
        std::fs::create_dir(&second_root).expect("Failed to create second directory");
        let first_video = first_root.join("first.mkv");
        let second_video = second_root.join("second.mp4");
        let nested_video = nested_root.join("nested.mkv");
        let standalone_video = directory.path().join("standalone.mp4");
        let ignored_file = directory.path().join("notes.txt");
        for path in [
            &first_video,
            &second_video,
            &nested_video,
            &standalone_video,
            &ignored_file,
        ] {
            std::fs::write(path, b"fixture").expect("Failed to write fixture");
        }
        let first_subtitle = first_root.join("first.srt");
        let standalone_subtitle = directory.path().join("standalone.srt");
        for path in [&first_subtitle, &standalone_subtitle] {
            std::fs::write(path, b"subtitle").expect("Failed to write subtitle fixture");
        }

        let mut config = config_for(&first_root, true);
        config.paths.extend([
            second_root,
            standalone_video.clone(),
            first_video.clone(),
            nested_root,
            ignored_file,
            first_root,
        ]);
        let (_log, converter) = converter(config);
        let files = converter.gather_files_to_process().expect("Failed to gather files");
        let paths: HashSet<PathBuf> = files.iter().map(|file| file.path.clone()).collect();

        assert_eq!(files.len(), 4);
        assert_eq!(
            paths,
            HashSet::from([first_video, second_video, nested_video, standalone_video])
        );
        let subtitles = VideoConvert::gather_subtitle_files_for_video_files(&files);
        assert_eq!(subtitles.len(), 2);
        assert_eq!(
            subtitles.into_iter().map(|file| file.path).collect::<HashSet<_>>(),
            HashSet::from([first_subtitle, standalone_subtitle])
        );
    }

    #[test]
    fn multiple_directories_respect_recursion_and_filters() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let roots = [directory.path().join("first"), directory.path().join("second")];
        for root in &roots {
            let nested = root.join("nested");
            std::fs::create_dir_all(&nested).expect("Failed to create nested directory");
            for path in [
                root.join("keep.mkv"),
                root.join("skip.mp4"),
                nested.join("keep.deep.mkv"),
            ] {
                std::fs::write(path, b"video").expect("Failed to write video fixture");
            }
        }
        let mut config = config_for(&roots[0], false);
        config.paths.push(roots[1].clone());
        config.include = vec!["keep".to_string()];
        let (_log, converter) = converter(config);

        let files = converter.gather_files_to_process().expect("Failed to gather files");

        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.name == "keep"));
    }

    #[test]
    fn a_single_file_path_is_gathered_when_it_matches() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let video = directory.path().join("clip.mkv");
        let text = directory.path().join("clip.txt");
        std::fs::write(&video, b"video").expect("Failed to write video file");
        std::fs::write(&text, b"notes").expect("Failed to write text file");

        let (_log, video_converter) = converter(config_for(&video, false));
        let (_log_text, text_converter) = converter(config_for(&text, false));

        assert_eq!(
            video_converter
                .gather_files_to_process()
                .expect("Failed to gather files")
                .len(),
            1
        );
        assert_eq!(
            text_converter
                .gather_files_to_process()
                .expect("Failed to gather files"),
            [] as [VideoFile; 0]
        );
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let (_log, converter) = converter(config_for(&directory.path().join("missing"), false));

        assert!(converter.gather_files_to_process().is_err());
    }
}
