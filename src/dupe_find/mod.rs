use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::PathBuf;
use std::sync::LazyLock;

use colored::Colorize;
use regex::Regex;

pub use crate::RE_RESOLUTION;

/// Regex to match codec patterns.
static RE_CODEC: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\b({})\b", CODEC_PATTERNS.join("|"));
    Regex::new(&pattern).expect("Invalid codec regex")
});

/// Regex to match two or more consecutive dots.
static RE_MULTI_DOTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{2,}").expect("Invalid dots regex"));

/// Regex to match two or more consecutive whitespace characters.
static RE_MULTI_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").expect("Invalid spaces regex"));

/// Common codec patterns to remove when normalizing.
pub const CODEC_PATTERNS: &[&str] = &["x264", "x265", "h264", "h265"];

/// All video extensions.
pub const FILE_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// Range of a pattern match in a filename.
#[derive(Debug, Clone, Copy)]
pub struct MatchRange {
    /// Start position of the match (inclusive).
    pub start: usize,
    /// End position of the match (exclusive).
    pub end: usize,
}

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

impl MatchRange {
    /// Extract the matched substring from the given text.
    #[must_use]
    pub fn extract_from<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }
}

impl DuplicateGroup {
    /// Create a new duplicate group.
    #[must_use]
    pub const fn new(key: String, files: Vec<DupeFileInfo>) -> Self {
        Self { key, files }
    }

    /// Get the display name for this group.
    ///
    /// If all files share the same pattern match text, uses that as the identifier.
    /// Otherwise falls back to the normalized key.
    #[must_use]
    pub fn display_name(&self) -> String {
        let mut pattern_texts = self
            .files
            .iter()
            .filter_map(|file| file.pattern_match.map(|range| range.extract_from(&file.filename)));

        if let Some(first) = pattern_texts.next()
            && pattern_texts.all(|text| text.eq_ignore_ascii_case(first))
        {
            return first.to_string();
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

/// Normalize a file stem by removing resolution and codec patterns.
///
/// Converts to lowercase and strips resolution tags (e.g. `1080p`), codec tags
/// (e.g. `x265`), then cleans up leftover consecutive dots, spaces, and
/// trailing separators. Falls back to the lowercased original stem if
/// normalization would produce an empty string.
pub fn normalize_stem(stem: &str) -> String {
    let mut normalized = stem.to_lowercase();

    // Remove resolutions
    normalized = RE_RESOLUTION.replace_all(&normalized, "").to_string();

    // Remove codec patterns
    normalized = RE_CODEC.replace_all(&normalized, "").to_string();

    // Clean up multiple dots and spaces
    normalized = RE_MULTI_DOTS.replace_all(&normalized, ".").to_string();
    normalized = RE_MULTI_SPACES.replace_all(&normalized, " ").to_string();

    let result = normalized
        .trim_matches(|character| character == '.' || character == ' ' || character == '_' || character == '-')
        .to_string();

    // Fallback to lowercase stem if normalization removed everything
    if result.is_empty() { stem.to_lowercase() } else { result }
}

/// Merge file indices into existing groups, unifying any separate groups that
/// contain files from the same set of indices.
///
/// Uses the group of the first index as the canonical group and moves all other
/// indices into it.
pub fn merge_indices_into_groups<S: BuildHasher>(
    indices: &[usize],
    file_to_group: &mut HashMap<usize, String, S>,
    groups: &mut HashMap<String, Vec<usize>, S>,
) {
    if indices.len() < 2 {
        return;
    }

    // Find the canonical group (use the first one as canonical)
    let canonical_group = file_to_group[&indices[0]].clone();

    for &index in &indices[1..] {
        let current_group = file_to_group[&index].clone();
        if current_group != canonical_group {
            // Move all files from current_group to canonical_group
            if let Some(to_move) = groups.remove(&current_group) {
                for moved_index in &to_move {
                    file_to_group.insert(*moved_index, canonical_group.clone());
                }
                groups.entry(canonical_group.clone()).or_default().extend(to_move);
            }
        }
    }
}

/// Format a filename with optional pattern match highlighting.
///
/// When a `MatchRange` is provided the matched portion is rendered in green.
/// Otherwise the filename is returned as-is.
#[must_use]
pub fn format_filename_with_highlight(filename: &str, pattern_match: Option<MatchRange>) -> String {
    pattern_match.map_or_else(
        || filename.to_string(),
        |range| {
            let before = &filename[..range.start];
            let matched = range.extract_from(filename).green().to_string();
            let after = &filename[range.end..];
            format!("{before}{matched}{after}")
        },
    )
}
