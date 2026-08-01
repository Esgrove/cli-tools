//! Duplicate file types, filename normalization, matching, hashing, and display helpers.

pub mod hash;
mod matcher;
mod normalization;
mod types;

pub use crate::{MatchRange, RE_RESOLUTION};
pub use matcher::{
    DuplicateMatchOptions, filter_ignored_groups, find_duplicates, group_matches_ignore, merge_indices_into_groups,
};
pub use normalization::{CODEC_PATTERNS, normalize_stem, strip_ignored_prefixes};
pub use types::{DupeFileInfo, DuplicateGroup, FILE_EXTENSIONS};

/// Format a filename with optional pattern match highlighting.
///
/// When a `MatchRange` is provided the matched portion is rendered in green.
/// Otherwise the filename is returned unchanged.
#[must_use]
pub fn format_filename_with_highlight(filename: &str, pattern_match: Option<MatchRange>) -> String {
    crate::format_text_with_highlight(filename, pattern_match)
}

#[cfg(test)]
mod test_format_filename_with_highlight {
    use super::*;

    #[test]
    fn leaves_filename_unchanged_without_match() {
        assert_eq!(format_filename_with_highlight("example.mp4", None), "example.mp4");
    }

    #[test]
    fn preserves_all_text_when_highlighting_match() {
        let formatted = format_filename_with_highlight("example.ABC123.mp4", Some(MatchRange { start: 8, end: 14 }));

        assert!(formatted.contains("example."));
        assert!(formatted.contains("ABC123"));
        assert!(formatted.contains(".mp4"));
    }
}
