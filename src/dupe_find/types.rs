//! Core duplicate file data types.

use std::path::PathBuf;

use crate::MatchRange;

/// All video extensions considered by default.
pub const FILE_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// A group of duplicate files that share a common key.
#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    /// The normalized key that identifies this group.
    pub key: String,
    /// Files belonging to this duplicate group.
    pub files: Vec<DupeFileInfo>,
}

/// Information about a duplicate file candidate.
#[derive(Debug, Clone)]
pub struct DupeFileInfo {
    /// Full path to the file.
    pub path: PathBuf,
    /// Complete filename including extension.
    pub filename: String,
    /// Filename stem without extension.
    pub stem: String,
    /// Lowercase file extension.
    pub extension: String,
    /// Pattern match range if matched by a pattern.
    pub pattern_match: Option<MatchRange>,
}

impl DuplicateGroup {
    /// Create a new duplicate group.
    #[must_use]
    pub const fn new(key: String, files: Vec<DupeFileInfo>) -> Self {
        Self { key, files }
    }

    /// Get the display name for this group.
    ///
    /// If pattern matches exist and all matched texts are equal, uses that text as the identifier.
    /// Files without a pattern match do not affect the result.
    /// Otherwise falls back to the normalized key.
    #[must_use]
    pub fn display_name(&self) -> String {
        let mut pattern_texts = self
            .files
            .iter()
            .filter_map(|file| file.pattern_match.map(|range| range.extract_from(&file.filename)));

        if let Some(first) = pattern_texts.next() {
            let normalized_first = first.to_lowercase();
            if pattern_texts.all(|text| text.to_lowercase() == normalized_first) {
                return first.to_string();
            }
        }

        self.key.clone()
    }
}

impl DupeFileInfo {
    /// Create a new `DupeFileInfo` from a path and extension.
    #[must_use]
    pub fn new(path: PathBuf, extension: String) -> Self {
        let filename = crate::path_to_filename_string(&path);
        let stem = crate::path_to_file_stem_string(&path);
        Self {
            path,
            filename,
            stem,
            extension,
            pattern_match: None,
        }
    }
}

#[cfg(test)]
mod test_dupe_file_info {
    use super::*;

    #[test]
    fn derives_filename_and_stem_from_path() {
        let path = PathBuf::from("videos/example.movie.mp4");

        let file = DupeFileInfo::new(path.clone(), "mp4".to_string());

        assert_eq!(file.path, path);
        assert_eq!(file.filename, "example.movie.mp4");
        assert_eq!(file.stem, "example.movie");
        assert_eq!(file.extension, "mp4");
        assert!(file.pattern_match.is_none());
    }
}

#[cfg(test)]
mod test_duplicate_group {
    use super::*;

    #[test]
    fn uses_shared_pattern_text_as_display_name() {
        let mut first = DupeFileInfo::new(PathBuf::from("Show.S01E05.mp4"), "mp4".to_string());
        first.pattern_match = Some(MatchRange { start: 5, end: 11 });
        let mut second = DupeFileInfo::new(PathBuf::from("show.s01e05.mkv"), "mkv".to_string());
        second.pattern_match = Some(MatchRange { start: 5, end: 11 });
        let group = DuplicateGroup::new("show.s01e05".to_string(), vec![first, second]);

        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn compares_pattern_text_using_unicode_case_folding() {
        let mut first = DupeFileInfo::new(PathBuf::from("Épisode.mp4"), "mp4".to_string());
        first.pattern_match = Some(MatchRange { start: 0, end: 8 });
        let mut second = DupeFileInfo::new(PathBuf::from("épisode.mkv"), "mkv".to_string());
        second.pattern_match = Some(MatchRange { start: 0, end: 8 });
        let group = DuplicateGroup::new("episode".to_string(), vec![first, second]);

        assert_eq!(group.display_name(), "Épisode");
    }

    #[test]
    fn falls_back_to_group_key_without_pattern_matches() {
        let group = DuplicateGroup::new(
            "example.movie".to_string(),
            vec![DupeFileInfo::new(PathBuf::from("example.movie.mp4"), "mp4".to_string())],
        );

        assert_eq!(group.display_name(), "example.movie");
    }
}

#[cfg(test)]
mod test_display_name {
    use super::{DupeFileInfo, DuplicateGroup, MatchRange};
    use std::path::PathBuf;

    /// Helper to create a `DupeFileInfo` with an optional pattern match.
    fn make_file_with_match(filename: &str, extension: &str, pattern_match: Option<MatchRange>) -> DupeFileInfo {
        let path = PathBuf::from(filename);
        let mut file = DupeFileInfo::new(path, extension.to_string());
        file.pattern_match = pattern_match;
        file
    }

    #[test]
    fn falls_back_to_key_when_no_pattern_matches() {
        let group = DuplicateGroup::new(
            "some.movie".to_string(),
            vec![
                make_file_with_match("some.movie.mkv", "mkv", None),
                make_file_with_match("some.movie.mp4", "mp4", None),
            ],
        );
        assert_eq!(group.display_name(), "some.movie");
    }

    #[test]
    fn uses_common_pattern_text_when_all_files_match() {
        // Files: "Show.S01E05.720p.mkv" and "Show.S01E05.1080p.mp4"
        // Pattern matched "S01E05" in both
        let group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("Show.S01E05.1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn uses_pattern_text_with_different_casing() {
        // Same pattern matched with different casing across files
        let group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("show.s01e05.1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        // "S01E05" and "s01e05" match case-insensitively, uses first file's text
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn falls_back_to_key_when_pattern_texts_differ() {
        // Merged group where files matched different pattern texts
        let group = DuplicateGroup::new(
            "normalized.key".to_string(),
            vec![
                make_file_with_match("Show.S01E05.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("Show.S02E10.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        // "S01E05" != "S02E10", so falls back to key
        assert_eq!(group.display_name(), "normalized.key");
    }

    #[test]
    fn uses_pattern_text_when_only_some_files_match() {
        // Group merged from a pattern match and a normalized name match.
        let group = DuplicateGroup::new(
            "some.show".to_string(),
            vec![
                make_file_with_match("Some.Show.S01E05.mkv", "mkv", Some(MatchRange { start: 10, end: 16 })),
                make_file_with_match("Some.Show.mkv", "mkv", None),
            ],
        );
        // Files without a pattern range are ignored when selecting a shared pattern text.
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn uses_pattern_text_for_single_file_with_match() {
        let group = DuplicateGroup::new(
            "key".to_string(),
            vec![make_file_with_match(
                "Show.S01E05.mkv",
                "mkv",
                Some(MatchRange { start: 5, end: 11 }),
            )],
        );
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn falls_back_to_key_for_empty_group() {
        let group = DuplicateGroup::new("empty.key".to_string(), vec![]);
        assert_eq!(group.display_name(), "empty.key");
    }

    #[test]
    fn matches_episode_pattern_with_letters_and_numbers() {
        // Pattern like (?i)S\d+E\d+ matching "S01E05" in filenames
        let group = DuplicateGroup::new(
            "show.s01e05".to_string(),
            vec![
                make_file_with_match("Show.S01E05.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match(
                    "Show.S01E05.REPACK.1080p.mp4",
                    "mp4",
                    Some(MatchRange { start: 5, end: 11 }),
                ),
                make_file_with_match("show.s01e05.web.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert_eq!(group.display_name(), "S01E05");
    }

    #[test]
    fn matches_numeric_only_identifier() {
        // Pattern like \d{6} matching a numeric ID in filenames
        let group = DuplicateGroup::new(
            "clip.483621".to_string(),
            vec![
                make_file_with_match("clip.483621.720p.mkv", "mkv", Some(MatchRange { start: 5, end: 11 })),
                make_file_with_match("clip_483621_1080p.mp4", "mp4", Some(MatchRange { start: 5, end: 11 })),
            ],
        );
        assert_eq!(group.display_name(), "483621");
    }

    #[test]
    fn matches_alphabetic_only_identifier() {
        // Pattern like [A-Za-z]+ matching a word identifier
        let group = DuplicateGroup::new(
            "documentary.wildlife".to_string(),
            vec![
                make_file_with_match(
                    "Documentary.Wildlife.720p.mkv",
                    "mkv",
                    Some(MatchRange { start: 12, end: 20 }),
                ),
                make_file_with_match(
                    "documentary.wildlife.1080p.mp4",
                    "mp4",
                    Some(MatchRange { start: 12, end: 20 }),
                ),
            ],
        );
        // "Wildlife" and "wildlife" match case-insensitively
        assert_eq!(group.display_name(), "Wildlife");
    }

    #[test]
    fn matches_pattern_at_different_positions_in_filenames() {
        // Same matched text but at different character positions in each filename
        // Pattern like (?i)ABC-\d+ matching "ABC-123"
        let group = DuplicateGroup::new(
            "normalized.key".to_string(),
            vec![
                make_file_with_match("ABC-123.720p.mkv", "mkv", Some(MatchRange { start: 0, end: 7 })),
                make_file_with_match(
                    "prefix.ABC-123.1080p.mp4",
                    "mp4",
                    Some(MatchRange { start: 7, end: 14 }),
                ),
            ],
        );
        assert_eq!(group.display_name(), "ABC-123");
    }

    #[test]
    fn matches_pattern_with_mixed_separators() {
        // Pattern matching an identifier like "2024.05.01" across files with different formatting
        let group = DuplicateGroup::new(
            "show.2024.05.01".to_string(),
            vec![
                make_file_with_match(
                    "Show.2024.05.01.Episode.mkv",
                    "mkv",
                    Some(MatchRange { start: 5, end: 15 }),
                ),
                make_file_with_match(
                    "show.2024.05.01.rerun.mp4",
                    "mp4",
                    Some(MatchRange { start: 5, end: 15 }),
                ),
            ],
        );
        assert_eq!(group.display_name(), "2024.05.01");
    }

    #[test]
    fn matches_short_alphanumeric_code() {
        // Pattern like [A-Z]\d{3} matching codes like "A001"
        let group = DuplicateGroup::new(
            "batch.a001".to_string(),
            vec![
                make_file_with_match("Batch.A001.v1.mkv", "mkv", Some(MatchRange { start: 6, end: 10 })),
                make_file_with_match("batch.a001.v2.mp4", "mp4", Some(MatchRange { start: 6, end: 10 })),
            ],
        );
        assert_eq!(group.display_name(), "A001");
    }
}
