//! Pure duplicate matching and group filtering.
//!
//! This module groups `DupeFileInfo` values by normalized names, exact filenames,
//! configured identifier patterns, and additional index groups supplied by callers.

use std::collections::HashMap;
use std::hash::BuildHasher;

use itertools::Itertools;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use regex::Regex;

use super::{DupeFileInfo, DuplicateGroup, MatchRange, normalize_stem, strip_ignored_prefixes};

/// Options controlling duplicate matching.
#[derive(Debug, Default)]
pub struct DuplicateMatchOptions<'a> {
    /// Identifier patterns used to merge files with the same matched text.
    pub patterns: &'a [Regex],
    /// Dot-delimited prefixes removed before filename comparisons.
    pub prefix_ignores: &'a [String],
    /// Additional file-index groups to merge, such as exact-content hash matches.
    pub additional_match_groups: &'a [Vec<usize>],
}

/// Find duplicate groups using filename, pattern, normalization, and matches supplied by the caller.
#[must_use]
pub fn find_duplicates(files: &[DupeFileInfo], options: &DuplicateMatchOptions<'_>) -> Vec<DuplicateGroup> {
    let normalized_keys: Vec<String> = files
        .par_iter()
        .map(|file| {
            let stem = strip_ignored_prefixes(&file.stem, options.prefix_ignores);
            normalize_stem(&stem)
        })
        .collect();

    let mut file_to_group: HashMap<usize, String> = HashMap::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();

    for (index, key) in normalized_keys.into_iter().enumerate() {
        file_to_group.insert(index, key.clone());
        groups.entry(key).or_default().push(index);
    }

    let mut filename_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        let filename = strip_ignored_prefixes(&file.filename, options.prefix_ignores).to_lowercase();
        filename_to_indices.entry(filename).or_default().push(index);
    }
    merge_matching_indices(filename_to_indices.values(), &mut file_to_group, &mut groups);

    let mut pattern_matches: HashMap<usize, MatchRange> = HashMap::new();
    let mut pattern_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        for pattern in options.patterns {
            if let Some(pattern_match) = pattern.find(&file.filename) {
                pattern_to_indices
                    .entry(pattern_match.as_str().to_lowercase())
                    .or_default()
                    .push(index);
                pattern_matches.insert(
                    index,
                    MatchRange {
                        start: pattern_match.start(),
                        end: pattern_match.end(),
                    },
                );
                break;
            }
        }
    }
    merge_matching_indices(pattern_to_indices.values(), &mut file_to_group, &mut groups);

    for indices in options.additional_match_groups {
        let valid_indices: Vec<usize> = indices.iter().copied().filter(|index| *index < files.len()).collect();
        merge_indices_into_groups(&valid_indices, &mut file_to_group, &mut groups);
    }

    groups
        .into_iter()
        .filter(|(_, indices)| indices.len() > 1)
        .map(|(key, indices)| {
            let group_files = indices
                .into_iter()
                .map(|index| {
                    let mut file = files[index].clone();
                    file.pattern_match = pattern_matches.get(&index).copied();
                    file
                })
                .collect();
            DuplicateGroup::new(key, group_files)
        })
        .sorted_by(|left, right| left.key.cmp(&right.key))
        .collect()
}

/// Return whether a duplicate group matches one of the configured ignore values.
#[must_use]
pub fn group_matches_ignore(group: &DuplicateGroup, ignore_matches: &[String]) -> bool {
    let display_name = group.display_name().to_lowercase();
    ignore_matches
        .iter()
        .any(|ignored| display_name == ignored.to_lowercase())
}

/// Remove duplicate groups whose display names match configured ignore values.
#[must_use]
pub fn filter_ignored_groups(groups: Vec<DuplicateGroup>, ignore_matches: &[String]) -> Vec<DuplicateGroup> {
    if ignore_matches.is_empty() {
        return groups;
    }

    groups
        .into_iter()
        .filter(|group| !group_matches_ignore(group, ignore_matches))
        .collect()
}

/// Merge file indices into existing groups, unifying groups containing the supplied files.
pub fn merge_indices_into_groups<S: BuildHasher>(
    indices: &[usize],
    file_to_group: &mut HashMap<usize, String, S>,
    groups: &mut HashMap<String, Vec<usize>, S>,
) {
    if indices.len() < 2 {
        return;
    }

    let Some(canonical_group) = file_to_group.get(&indices[0]).cloned() else {
        return;
    };
    for &index in &indices[1..] {
        let Some(current_group) = file_to_group.get(&index).cloned() else {
            continue;
        };
        if current_group != canonical_group
            && let Some(to_move) = groups.remove(&current_group)
        {
            for moved_index in &to_move {
                file_to_group.insert(*moved_index, canonical_group.clone());
            }
            groups.entry(canonical_group.clone()).or_default().extend(to_move);
        }
    }
}

/// Merge every index collection containing at least two files.
fn merge_matching_indices<'a>(
    index_groups: impl Iterator<Item = &'a Vec<usize>>,
    file_to_group: &mut HashMap<usize, String>,
    groups: &mut HashMap<String, Vec<usize>>,
) {
    for indices in index_groups {
        if indices.len() > 1 {
            merge_indices_into_groups(indices, file_to_group, groups);
        }
    }
}

#[cfg(test)]
mod test_merge_indices_into_groups {
    use super::*;

    #[test]
    fn ignores_indices_missing_from_group_map() {
        let mut file_to_group = HashMap::from([(0, "first".to_string()), (1, "second".to_string())]);
        let mut groups = HashMap::from([("first".to_string(), vec![0]), ("second".to_string(), vec![1])]);

        merge_indices_into_groups(&[0, 99, 1], &mut file_to_group, &mut groups);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups["first"], vec![0, 1]);
    }

    #[test]
    fn ignores_missing_canonical_index() {
        let mut file_to_group = HashMap::from([(0, "first".to_string())]);
        let mut groups = HashMap::from([("first".to_string(), vec![0])]);

        merge_indices_into_groups(&[99, 0], &mut file_to_group, &mut groups);

        assert_eq!(groups["first"], vec![0]);
    }
}

#[cfg(test)]
mod test_find_duplicates {
    use std::path::PathBuf;

    use super::*;

    fn make_file(path: &str, extension: &str) -> DupeFileInfo {
        DupeFileInfo::new(PathBuf::from(path), extension.to_string())
    }

    fn find_with_patterns(files: &[DupeFileInfo], patterns: &[&str]) -> Vec<DuplicateGroup> {
        let patterns: Vec<Regex> = patterns
            .iter()
            .map(|pattern| Regex::new(pattern).expect("valid test regex"))
            .collect();
        find_duplicates(
            files,
            &DuplicateMatchOptions {
                patterns: &patterns,
                ..Default::default()
            },
        )
    }

    #[test]
    fn groups_normalized_resolution_and_codec_variants() {
        let files = vec![
            make_file("/one/movie.1080p.x265.mp4", "mp4"),
            make_file("/two/movie.720p.x264.mkv", "mkv"),
            make_file("/three/other.mp4", "mp4"),
        ];

        let duplicates = find_duplicates(&files, &DuplicateMatchOptions::default());

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].key, "movie");
        assert_eq!(duplicates[0].files.len(), 2);
    }

    #[test]
    fn strips_ignored_prefixes_before_matching() {
        let files = vec![
            make_file("/one/prefix.some.file.name.123.mp4", "mp4"),
            make_file("/two/Some.File.Name.123.mp4", "mp4"),
        ];
        let prefix_ignores = vec!["prefix".to_string()];

        let duplicates = find_duplicates(
            &files,
            &DuplicateMatchOptions {
                prefix_ignores: &prefix_ignores,
                ..Default::default()
            },
        );

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].key, "some.file.name.123");
    }

    #[test]
    fn groups_exact_filenames_case_insensitively() {
        let files = vec![
            make_file("/one/Video.mp4", "mp4"),
            make_file("/two/video.mp4", "mp4"),
            make_file("/three/other.mp4", "mp4"),
        ];

        let duplicates = find_duplicates(&files, &DuplicateMatchOptions::default());

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 2);
    }

    #[test]
    fn groups_case_insensitive_identifier_patterns_and_records_ranges() {
        let files = vec![
            make_file("/one/video.ABC123.mp4", "mp4"),
            make_file("/two/movie.abc123.mkv", "mkv"),
            make_file("/three/other.XYZ999.mp4", "mp4"),
        ];

        let duplicates = find_with_patterns(&files, &[r"(?i)ABC\d+"]);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 2);
        assert!(duplicates[0].files.iter().all(|file| file.pattern_match.is_some()));
        assert_eq!(duplicates[0].display_name(), "ABC123");
    }

    #[test]
    fn first_matching_pattern_takes_priority() {
        let files = vec![
            make_file("/one/video.FIRST001.mp4", "mp4"),
            make_file("/two/movie.FIRST001.mkv", "mkv"),
            make_file("/three/clip.FIRST001.SECOND002.mp4", "mp4"),
            make_file("/four/other.SECOND002.mp4", "mp4"),
            make_file("/five/another.SECOND002.mkv", "mkv"),
        ];

        let duplicates = find_with_patterns(&files, &[r"FIRST\d+", r"SECOND\d+"]);

        assert_eq!(duplicates.len(), 2);
        assert!(duplicates.iter().any(|group| group.files.len() == 3));
        assert!(duplicates.iter().any(|group| group.files.len() == 2));
    }

    #[test]
    fn merges_caller_supplied_match_groups() {
        let files = vec![
            make_file("/one/first.mp4", "mp4"),
            make_file("/two/different-name.mp4", "mp4"),
            make_file("/three/unique.mp4", "mp4"),
        ];
        let additional_match_groups = vec![vec![0, 1], vec![99, 100]];

        let duplicates = find_duplicates(
            &files,
            &DuplicateMatchOptions {
                additional_match_groups: &additional_match_groups,
                ..Default::default()
            },
        );

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 2);
    }

    #[test]
    fn merges_groups_across_multiple_match_criteria() {
        let files = vec![
            make_file("/one/movie.1080p.mp4", "mp4"),
            make_file("/two/movie.720p.mkv", "mkv"),
            make_file("/three/video.ID123.mp4", "mp4"),
            make_file("/four/other.ID123.mp4", "mp4"),
        ];

        let duplicates = find_with_patterns(&files, &[r"ID\d+"]);

        assert_eq!(duplicates.len(), 2);
        assert!(duplicates.iter().all(|group| group.files.len() == 2));
    }

    #[test]
    fn empty_and_single_inputs_have_no_duplicates() {
        assert!(find_duplicates(&[], &DuplicateMatchOptions::default()).is_empty());

        let files = vec![make_file("/one/video.mp4", "mp4")];
        assert!(find_duplicates(&files, &DuplicateMatchOptions::default()).is_empty());
    }
}

#[cfg(test)]
mod test_filter_ignored_groups {
    use std::path::PathBuf;

    use super::*;

    fn make_file(filename: &str, pattern_match: Option<MatchRange>) -> DupeFileInfo {
        let mut file = DupeFileInfo::new(PathBuf::from(filename), "mp4".to_string());
        file.pattern_match = pattern_match;
        file
    }

    #[test]
    fn keeps_everything_when_ignore_list_is_empty() {
        let groups = vec![DuplicateGroup::new(
            "movie.one".to_string(),
            vec![make_file("movie.one.mp4", None), make_file("movie.one.mkv", None)],
        )];

        assert_eq!(filter_ignored_groups(groups, &[]).len(), 1);
    }

    #[test]
    fn filters_key_matches_case_insensitively() {
        let groups = vec![DuplicateGroup::new(
            "movie.one".to_string(),
            vec![make_file("movie.one.mp4", None), make_file("movie.one.mkv", None)],
        )];

        assert!(filter_ignored_groups(groups, &["Movie.One".to_string()]).is_empty());
    }

    #[test]
    fn filters_unicode_key_matches_case_insensitively() {
        let groups = vec![DuplicateGroup::new(
            "Épisode".to_string(),
            vec![make_file("Épisode.mp4", None), make_file("Épisode.mkv", None)],
        )];

        assert!(filter_ignored_groups(groups, &["épisode".to_string()]).is_empty());
    }

    #[test]
    fn filters_pattern_display_names_without_partial_matching() {
        let pattern_group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file("Show.S01E05.mp4", Some(MatchRange { start: 5, end: 11 })),
                make_file("Show.S01E05.mkv", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert!(filter_ignored_groups(vec![pattern_group], &["s01e05".to_string()]).is_empty());

        let partial_group = DuplicateGroup::new(
            "abc-123".to_string(),
            vec![make_file("ABC-123.mp4", None), make_file("ABC-123.mkv", None)],
        );
        assert_eq!(
            filter_ignored_groups(vec![partial_group], &["abc".to_string()]).len(),
            1
        );
    }
}

#[cfg(test)]
mod test_duplicate_matching_regressions {
    use super::{DupeFileInfo, DuplicateGroup, DuplicateMatchOptions, find_duplicates};
    use regex::Regex;
    use std::path::PathBuf;

    /// Test matcher that owns pattern and prefix configuration.
    struct TestConfig {
        prefix_ignores: Vec<String>,
    }

    struct TestMatcher {
        patterns: Vec<Regex>,
        config: TestConfig,
    }

    impl TestMatcher {
        fn find_all_duplicates(&self, files: &[DupeFileInfo]) -> Vec<DuplicateGroup> {
            find_duplicates(
                files,
                &DuplicateMatchOptions {
                    patterns: &self.patterns,
                    prefix_ignores: &self.config.prefix_ignores,
                    additional_match_groups: &[],
                },
            )
        }
    }

    /// Helper to create a `DupeFileInfo` for testing.
    fn make_file(path: &str, extension: &str) -> DupeFileInfo {
        DupeFileInfo::new(PathBuf::from(path), extension.to_string())
    }

    /// Create a test matcher with specific patterns.
    fn make_dupe_finder(patterns: Vec<&str>) -> TestMatcher {
        TestMatcher {
            patterns: patterns
                .into_iter()
                .map(|pattern| Regex::new(pattern).expect("Invalid test pattern"))
                .collect(),
            config: TestConfig { prefix_ignores: vec![] },
        }
    }

    #[test]
    fn test_find_duplicates_by_normalized_name() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/path1/movie.1080p.mp4", "mp4"),
            make_file("/path2/movie.720p.mkv", "mkv"),
            make_file("/path3/other.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // movie.1080p and movie.720p should be grouped (both normalize to "movie")
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].key, "movie");
        assert_eq!(duplicates[0].files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_after_stripping_ignored_prefix() {
        let mut finder = make_dupe_finder(vec![]);
        finder.config.prefix_ignores = vec!["prefix".to_string()];
        let files = vec![
            make_file("/path1/prefix.some.file.name.123.mp4", "mp4"),
            make_file("/path2/Some.File.Name.123.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].key, "some.file.name.123");
        assert_eq!(duplicates[0].files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_by_exact_filename() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/path1/video.mp4", "mp4"),
            make_file("/path2/video.mp4", "mp4"),
            make_file("/path3/other.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_by_pattern() {
        let finder = make_dupe_finder(vec![r"ABC\d+"]);
        let files = vec![
            make_file("/path1/video.ABC123.mp4", "mp4"),
            make_file("/path2/movie.ABC123.mkv", "mkv"),
            make_file("/path3/other.XYZ999.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // video.ABC123 and movie.ABC123 should be grouped by pattern match
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 2);
        // Verify pattern match positions are stored.
        assert!(duplicates[0].files.iter().all(|file| file.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_pattern_same_root() {
        // Files with same pattern in the same root directory
        let finder = make_dupe_finder(vec![r"[A-Z]{2}\d{4}"]);
        let files = vec![
            make_file("/videos/Holiday.AB1234.mp4", "mp4"),
            make_file("/videos/Vacation.AB1234.mkv", "mkv"),
            make_file("/videos/Trip.AB1234.1080p.mp4", "mp4"),
            make_file("/videos/Trip.ABC1234.1080p.mp4", "mp4"),
            make_file("/videos/Trip.DDD123.1080p.mp4", "mp4"),
            make_file("/videos/Trip.1234.1080p.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 3);
        assert!(duplicates[0].files.iter().all(|file| file.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_pattern_nested_subdirs() {
        // Files with same pattern scattered in nested subdirectories
        let finder = make_dupe_finder(vec![r"ID-[a-z0-9]+"]);
        let files = vec![
            make_file("/root/2024/january/clip.ID-abc123.mp4", "mp4"),
            make_file("/root/2024/february/video.ID-abc123.mkv", "mkv"),
            make_file("/root/2023/archive/old.ID-abc123.mp4", "mp4"),
            make_file("/root/downloads/new.ID-abc123.720p.mp4", "mp4"),
            make_file("/other/backup/copy.ID-abc123.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 5);
    }

    #[test]
    fn test_find_duplicates_pattern_varied_naming_styles() {
        // Different naming conventions but same pattern identifier
        let finder = make_dupe_finder(vec![r"tt\d{7}"]);
        let files = vec![
            // Various naming styles used in media files
            make_file("/movies/The.Movie.2024.tt1234567.1080p.mp4", "mp4"),
            make_file("/movies/The Movie (2024) tt1234567.mkv", "mkv"),
            make_file("/movies/the-movie-2024-tt1234567-720p.mp4", "mp4"),
            make_file("/downloads/The_Movie_2024_tt1234567.mp4", "mp4"),
            make_file("/backup/movie.tt1234567.x265.mp4", "mp4"),
            // Different identifier - should not be grouped
            make_file("/movies/Other.Film.tt9999999.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have one group for tt1234567 (5 files)
        // tt9999999 is alone, so no group for it
        let tt1234567_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("tt1234567")))
            .expect("expected tt1234567 duplicate group");
        assert_eq!(tt1234567_group.files.len(), 5);
    }

    #[test]
    fn test_find_duplicates_pattern_multiple_roots() {
        // Same pattern appearing across completely different root directories
        let finder = make_dupe_finder(vec![r"S\d{2}E\d{2}"]);
        let files = vec![
            make_file("/nas/tv/Show/Season1/show.S01E05.mp4", "mp4"),
            make_file("/local/downloads/show.S01E05.720p.mkv", "mkv"),
            make_file("/external/backup/tv/show.S01E05.1080p.mp4", "mp4"),
            make_file("/cloud/media/show.S01E05.x265.mp4", "mp4"),
            // Different episode
            make_file("/nas/tv/Show/Season1/show.S01E06.mp4", "mp4"),
            make_file("/local/downloads/show.S01E06.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have 2 groups: S01E05 (4 files) and S01E06 (2 files)
        assert_eq!(duplicates.len(), 2);

        let episode_five_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("S01E05")))
            .expect("expected S01E05 duplicate group");
        assert_eq!(episode_five_group.files.len(), 4);

        let episode_six_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("S01E06")))
            .expect("expected S01E06 duplicate group");
        assert_eq!(episode_six_group.files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_pattern_with_special_chars_in_path() {
        // Paths with spaces, unicode, and special characters
        let finder = make_dupe_finder(vec![r"REF\d+"]);
        let files = vec![
            make_file("/My Videos/2024 Clips/video.REF001.mp4", "mp4"),
            make_file("/Media Library/Downloads (New)/clip.REF001.mkv", "mkv"),
            make_file("/Données/Vidéos/fichier.REF001.mp4", "mp4"),
            make_file("/path/with spaces/and-dashes/file.REF001.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 4);
    }

    #[test]
    fn test_find_duplicates_pattern_same_identifier_different_case_in_name() {
        // Files with same pattern identifier but different casing elsewhere in filename
        // Pattern groups by the matched string itself, so same identifier = same group
        let finder = make_dupe_finder(vec![r"ID\d+"]);
        let files = vec![
            make_file("/path1/VIDEO.ID123.mp4", "mp4"),
            make_file("/path2/video.ID123.mkv", "mkv"),
            make_file("/path3/Video.ID123.mp4", "mp4"),
            make_file("/path4/CLIP.ID123.mp4", "mp4"),
            make_file("/path4/CLIP.CD123.mp4", "mp4"),
            make_file("/path4/CLIP.123.mp4", "mp4"),
            make_file("/path4/CLIP.ID.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // All have same pattern match "ID123", so grouped together
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 4);
    }

    #[test]
    fn test_find_duplicates_pattern_at_different_positions() {
        // Pattern appearing at start, middle, and end of filename
        let finder = make_dupe_finder(vec![r"KEY\d{3}"]);
        let files = vec![
            make_file("/videos/KEY001.video.mp4", "mp4"),
            make_file("/videos/video.KEY001.mp4", "mp4"),
            make_file("/videos/video.KEY001.1080p.mp4", "mp4"),
            make_file("/videos/some.long.name.KEY001.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 4);
        // Verify all have pattern matches recorded.
        assert!(duplicates[0].files.iter().all(|file| file.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_pattern_overlapping_with_resolution() {
        // Pattern that could be confused with resolution patterns
        let finder = make_dupe_finder(vec![r"V\d{4}"]);
        let files = vec![
            make_file("/videos/movie.V1080.mp4", "mp4"),
            make_file("/videos/movie.V1080.720p.mkv", "mkv"),
            make_file("/videos/clip.V1080.1080p.x265.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should group by pattern V1080, resolution should be stripped separately
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].files.len(), 3);
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn test_find_duplicates_many_patterns_many_files() {
        // Stress test with multiple patterns and many files
        let finder = make_dupe_finder(vec![r"GRP[A-Z]", r"NUM\d+", r"TAG_\w+"]);
        let files = vec![
            // GRPA group
            make_file("/dir1/a.GRPA.mp4", "mp4"),
            make_file("/dir2/b.GRPA.mkv", "mkv"),
            make_file("/dir3/c.GRPA.mp4", "mp4"),
            // GRPB group
            make_file("/dir1/x.GRPB.mp4", "mp4"),
            make_file("/dir2/y.GRPB.mkv", "mkv"),
            // NUM001 group
            make_file("/dir1/video.NUM001.mp4", "mp4"),
            make_file("/dir2/movie.NUM001.mkv", "mkv"),
            make_file("/dir3/clip.NUM001.mp4", "mp4"),
            make_file("/dir4/film.NUM001.mp4", "mp4"),
            // TAG_test group
            make_file("/dir1/file.TAG_test.mp4", "mp4"),
            make_file("/dir2/other.TAG_test.mkv", "mkv"),
            // Unique files (no duplicates)
            make_file("/dir1/unique1.mp4", "mp4"),
            make_file("/dir2/unique2.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have 4 groups: GRPA(3), GRPB(2), NUM001(4), TAG_test(2)
        assert_eq!(duplicates.len(), 4);

        let group_a = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("GRPA")))
            .expect("expected GRPA duplicate group");
        assert_eq!(group_a.files.len(), 3);

        let group_b = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("GRPB")))
            .expect("expected GRPB duplicate group");
        assert_eq!(group_b.files.len(), 2);

        let numeric_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("NUM001")))
            .expect("expected NUM001 duplicate group");
        assert_eq!(numeric_group.files.len(), 4);

        let tag_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("TAG_test")))
            .expect("expected TAG_test duplicate group");
        assert_eq!(tag_group.files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_no_duplicates() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/path1/video1.mp4", "mp4"),
            make_file("/path2/video2.mp4", "mp4"),
            make_file("/path3/video3.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        assert!(duplicates.is_empty());
    }

    #[test]
    fn test_find_duplicates_merges_groups() {
        // Test that files matching by pattern are merged into one group
        let finder = make_dupe_finder(vec![r"ID\d+"]);
        let files = vec![
            // These two match by normalized name (both -> "movie")
            make_file("/path1/movie.1080p.mp4", "mp4"),
            make_file("/path2/movie.720p.mkv", "mkv"),
            // These two match by pattern ID123
            make_file("/path3/video.ID123.mp4", "mp4"),
            make_file("/path4/other.ID123.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have two groups:
        // - "movie" group (movie.1080p and movie.720p normalize to same name)
        // - merged ID123 group (video.ID123 and other.ID123 match by pattern)
        assert_eq!(duplicates.len(), 2);

        // Find the movie group and verify it has 2 files
        let movie_group = duplicates
            .iter()
            .find(|group| group.key == "movie")
            .expect("expected normalized movie group");
        assert_eq!(movie_group.files.len(), 2);

        // Find the pattern-matched group, which is keyed by the first file's normalized name.
        let pattern_group = duplicates
            .iter()
            .find(|group| group.key != "movie")
            .expect("expected identifier pattern group");
        assert_eq!(pattern_group.files.len(), 2);
        assert!(pattern_group.files.iter().all(|file| file.pattern_match.is_some()));
    }

    #[test]
    fn test_find_duplicates_with_resolution_variants() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![
            make_file("/videos/show.1080p.x265.mp4", "mp4"),
            make_file("/videos/show.720p.x264.mkv", "mkv"),
            make_file("/videos/show.1920x1080.mp4", "mp4"),
            make_file("/videos/show.2160p.mp4", "mp4"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // All should normalize to "show"
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].key, "show");
        assert_eq!(duplicates[0].files.len(), 4);
    }

    #[test]
    fn test_find_duplicates_multiple_patterns() {
        let finder = make_dupe_finder(vec![r"ABC\d+", r"XYZ\d+"]);
        let files = vec![
            make_file("/path1/video.ABC123.mp4", "mp4"),
            make_file("/path2/movie.ABC123.mkv", "mkv"),
            make_file("/path3/clip.XYZ456.mp4", "mp4"),
            make_file("/path4/film.XYZ456.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // Should have two groups: ABC123 and XYZ456
        assert_eq!(duplicates.len(), 2);
    }

    #[test]
    fn test_find_duplicates_first_pattern_takes_priority() {
        // When a file matches multiple patterns, first pattern wins
        let finder = make_dupe_finder(vec![r"FIRST\d+", r"SECOND\d+"]);
        let files = vec![
            // These have FIRST pattern
            make_file("/path1/video.FIRST001.mp4", "mp4"),
            make_file("/path2/movie.FIRST001.mkv", "mkv"),
            // This has both patterns - FIRST should win
            make_file("/path3/clip.FIRST001.SECOND002.mp4", "mp4"),
            // These only have SECOND
            make_file("/path4/other.SECOND002.mp4", "mp4"),
            make_file("/path5/another.SECOND002.mkv", "mkv"),
        ];

        let duplicates = finder.find_all_duplicates(&files);

        // FIRST001 group should have 3 files (including the one with both patterns)
        let first_group = duplicates
            .iter()
            .find(|group| group.files.iter().any(|file| file.filename.contains("FIRST001")))
            .expect("expected FIRST001 duplicate group");
        assert_eq!(first_group.files.len(), 3);

        // The SECOND002 group excludes the file already claimed by the first pattern.
        let second_group = duplicates
            .iter()
            .find(|group| {
                group.files.iter().all(|file| !file.filename.contains("FIRST001"))
                    && group.files.iter().any(|file| file.filename.contains("SECOND002"))
            })
            .expect("expected SECOND002 duplicate group");
        assert_eq!(second_group.files.len(), 2);
    }

    #[test]
    fn test_find_duplicates_empty_input() {
        let finder = make_dupe_finder(vec![]);
        let files: Vec<DupeFileInfo> = vec![];

        let duplicates = finder.find_all_duplicates(&files);

        assert!(duplicates.is_empty());
    }

    #[test]
    fn test_find_duplicates_single_file() {
        let finder = make_dupe_finder(vec![]);
        let files = vec![make_file("/path/video.mp4", "mp4")];

        let duplicates = finder.find_all_duplicates(&files);

        assert!(duplicates.is_empty());
    }
}

#[cfg(test)]
mod test_filter_ignored_groups_regressions {
    use super::{DupeFileInfo, DuplicateGroup, MatchRange, filter_ignored_groups};

    use std::path::PathBuf;

    /// Test filter that owns ignore configuration.
    struct TestFilter {
        ignore_matches: Vec<String>,
    }

    impl TestFilter {
        fn filter_ignored_groups(&self, groups: Vec<DuplicateGroup>) -> Vec<DuplicateGroup> {
            filter_ignored_groups(groups, &self.ignore_matches)
        }
    }

    /// Helper to create a `DupeFileInfo` with an optional pattern match.
    fn make_file_with_match(filename: &str, extension: &str, pattern_match: Option<MatchRange>) -> DupeFileInfo {
        let path = PathBuf::from(filename);
        let mut file = DupeFileInfo::new(path, extension.to_string());
        file.pattern_match = pattern_match;
        file
    }

    /// Create a test filter with specific ignore values.
    fn make_dupe_finder(ignore_matches: Vec<&str>) -> TestFilter {
        TestFilter {
            ignore_matches: ignore_matches.into_iter().map(str::to_lowercase).collect(),
        }
    }

    #[test]
    fn keeps_all_groups_when_no_ignores_configured() {
        let finder = make_dupe_finder(vec![]);
        let groups = vec![
            DuplicateGroup::new(
                "movie.one".to_string(),
                vec![
                    make_file_with_match("movie.one.mkv", "mkv", None),
                    make_file_with_match("movie.one.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.two".to_string(),
                vec![
                    make_file_with_match("movie.two.mkv", "mkv", None),
                    make_file_with_match("movie.two.mp4", "mp4", None),
                ],
            ),
        ];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filters_group_by_key_match() {
        let finder = make_dupe_finder(vec!["movie.one"]);
        let groups = vec![
            DuplicateGroup::new(
                "movie.one".to_string(),
                vec![
                    make_file_with_match("movie.one.mkv", "mkv", None),
                    make_file_with_match("movie.one.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.two".to_string(),
                vec![
                    make_file_with_match("movie.two.mkv", "mkv", None),
                    make_file_with_match("movie.two.mp4", "mp4", None),
                ],
            ),
        ];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].key, "movie.two");
    }

    #[test]
    fn filters_group_by_pattern_match_display_name() {
        let finder = make_dupe_finder(vec!["S01E05"]);
        let groups = vec![DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("Show.S01E05.1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        )];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn comparison_is_case_insensitive() {
        let finder = make_dupe_finder(vec!["ABC-123"]);
        let groups = vec![DuplicateGroup::new(
            "normalized.key".to_string(),
            vec![
                make_file_with_match("prefix.Abc-123.mkv", "mkv", Some(MatchRange { start: 7, end: 14 })),
                make_file_with_match("prefix.abc-123.mp4", "mp4", Some(MatchRange { start: 7, end: 14 })),
            ],
        )];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn does_not_filter_partial_matches() {
        let finder = make_dupe_finder(vec!["ABC"]);
        let groups = vec![DuplicateGroup::new(
            "abc-123".to_string(),
            vec![
                make_file_with_match("ABC-123.mkv", "mkv", None),
                make_file_with_match("ABC-123.mp4", "mp4", None),
            ],
        )];
        // "abc-123" != "abc", so the group should be kept
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filters_multiple_ignored_patterns() {
        let finder = make_dupe_finder(vec!["movie.one", "movie.three"]);
        let groups = vec![
            DuplicateGroup::new(
                "movie.one".to_string(),
                vec![
                    make_file_with_match("movie.one.mkv", "mkv", None),
                    make_file_with_match("movie.one.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.two".to_string(),
                vec![
                    make_file_with_match("movie.two.mkv", "mkv", None),
                    make_file_with_match("movie.two.mp4", "mp4", None),
                ],
            ),
            DuplicateGroup::new(
                "movie.three".to_string(),
                vec![
                    make_file_with_match("movie.three.mkv", "mkv", None),
                    make_file_with_match("movie.three.mp4", "mp4", None),
                ],
            ),
        ];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].key, "movie.two");
    }

    #[test]
    fn filters_by_numeric_pattern_display_name() {
        let finder = make_dupe_finder(vec!["483621"]);
        let groups = vec![DuplicateGroup::new(
            "clip.483621".to_string(),
            vec![
                make_file_with_match("clip.483621.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("clip_483621_1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        )];
        let filtered = finder.filter_ignored_groups(groups);
        assert_eq!(filtered.len(), 0);
    }
}
