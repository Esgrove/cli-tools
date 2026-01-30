use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use cli_tools::path_to_filename_string;

/// Information about a directory used for matching files to move.
#[derive(Debug)]
pub struct DirectoryInfo {
    /// Absolute path to the directory.
    pub(crate) path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    pub(crate) name: String,
}

/// Information about a file for grouping purposes.
/// Uses `Cow` for efficient string handling - avoids cloning when possible.
pub struct FileInfo<'a> {
    /// Path to the file.
    pub(crate) path: Cow<'a, Path>,
    /// Original filename after stripping ignored prefixes (used for contiguity checks).
    pub(crate) original_name: Cow<'a, str>,
    /// Filtered filename with numeric/resolution/glue parts removed (used for prefix matching).
    pub(crate) filtered_name: Cow<'a, str>,
}

#[derive(Debug)]
pub struct MoveInfo {
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
}

/// A pair of directories to merge: source (with prefix) into target (without prefix).
/// Used during the `merge_prefixed_directories` operation.
#[derive(Debug)]
pub struct MergePair {
    /// Directory with the `prefix_ignore` prefix (to be merged from).
    pub(crate) source: PathBuf,
    /// Directory without the prefix (to be merged into).
    pub(crate) target: PathBuf,
}

impl MergePair {
    /// Create a new `MergePair`.
    pub(crate) const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

/// A candidate prefix for grouping files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixCandidate<'a> {
    /// The prefix string (e.g., "Studio.Name" or "`StudioName`").
    pub(crate) prefix: Cow<'a, str>,
    /// Number of files matching this prefix.
    pub(crate) match_count: usize,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    pub(crate) part_count: usize,
    /// Position in the filename where this prefix starts (0 = beginning).
    /// Lower values indicate prefixes closer to the start of the filename.
    pub(crate) start_position: usize,
}

/// Information about what needs to be moved during an unpack operation.
#[derive(Debug, Default)]
pub struct UnpackInfo {
    /// Files to move.
    pub(crate) file_moves: Vec<MoveInfo>,
    /// Directories to move directly.
    pub(crate) directory_moves: Vec<MoveInfo>,
}

impl FileInfo<'_> {
    /// Create a new `FileInfo` with owned strings.
    pub(crate) const fn new(path: PathBuf, original_name: String, filtered_name: String) -> Self {
        Self {
            path: Cow::Owned(path),
            original_name: Cow::Owned(original_name),
            filtered_name: Cow::Owned(filtered_name),
        }
    }

    /// Get the path as a `PathBuf`.
    pub(crate) fn path_buf(&self) -> PathBuf {
        self.path.to_path_buf()
    }
}

impl fmt::Debug for FileInfo<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileInfo")
            .field("path", &self.path)
            .field("original_name", &self.original_name)
            .field("filtered_name", &self.filtered_name)
            .finish()
    }
}

impl DirectoryInfo {
    pub(crate) fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
}

impl MoveInfo {
    pub(crate) const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

impl<'a> PrefixCandidate<'a> {
    /// Create a new `PrefixCandidate`.
    pub(crate) const fn new(
        prefix: Cow<'a, str>,
        match_count: usize,
        part_count: usize,
        start_position: usize,
    ) -> Self {
        Self {
            prefix,
            match_count,
            part_count,
            start_position,
        }
    }
}

/// A group of files that share a common prefix.
/// Used for organizing files into directories based on their name prefixes.
#[derive(Debug, Clone)]
pub struct PrefixGroup {
    /// Files belonging to this prefix group.
    pub(crate) files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    /// Higher values indicate more specific prefixes.
    pub(crate) part_count: usize,
    /// Minimum start position where this prefix appears across all files.
    /// Lower values indicate prefixes closer to the start of filenames.
    pub(crate) min_start_position: usize,
}

impl PrefixGroup {
    /// Create a new `PrefixGroup`.
    pub(crate) const fn new(files: Vec<PathBuf>, part_count: usize, min_start_position: usize) -> Self {
        Self {
            files,
            part_count,
            min_start_position,
        }
    }
}

/// Intermediate data structure for building prefix groups.
/// Used during the collection phase before converting to final `PrefixGroup`.
#[derive(Debug)]
pub struct PrefixGroupBuilder {
    /// The original prefix string (may contain dots).
    pub(crate) original_prefix: String,
    /// Files belonging to this prefix group.
    pub(crate) files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix.
    pub(crate) part_count: usize,
    /// Whether the prefix has a concatenated (no-dot) form.
    pub(crate) has_concatenated_form: bool,
    /// Minimum start position where this prefix appears across all files.
    pub(crate) min_start_position: usize,
}

impl PrefixGroupBuilder {
    /// Create a new `PrefixGroupBuilder`.
    pub(crate) fn new(
        original_prefix: String,
        file: PathBuf,
        part_count: usize,
        has_concatenated_form: bool,
        start_position: usize,
    ) -> Self {
        Self {
            original_prefix,
            files: vec![file],
            part_count,
            has_concatenated_form,
            min_start_position: start_position,
        }
    }

    /// Add a file to this group and update metadata.
    /// Prefers concatenated (no-dot) forms for directory names.
    /// Among concatenated forms, prefers proper casing (starts with uppercase).
    /// For determinism across platforms, uses alphabetical ordering as tiebreaker.
    pub(crate) fn add_file(
        &mut self,
        file: PathBuf,
        part_count: usize,
        is_concatenated: bool,
        start_position: usize,
        prefix: String,
    ) {
        self.files.push(file);
        self.part_count = self.part_count.max(part_count);
        self.min_start_position = self.min_start_position.min(start_position);

        // Prefer concatenated (no-dot) form for directory names
        if is_concatenated {
            if self.has_concatenated_form {
                // Already have a concatenated form - prefer better casing or alphabetical order
                let should_replace = Self::is_better_prefix(&prefix, &self.original_prefix);
                if should_replace {
                    self.original_prefix = prefix;
                }
            } else {
                // First concatenated form - use it
                self.original_prefix = prefix;
                self.has_concatenated_form = true;
            }
        }
    }

    /// Determine if `new_prefix` is better than `current_prefix` for display.
    /// Prefers CamelCase (mixed case) over all-uppercase or all-lowercase.
    /// Uses alphabetical order as final tiebreaker for determinism.
    fn is_better_prefix(new_prefix: &str, current_prefix: &str) -> bool {
        let new_score = Self::casing_score(new_prefix);
        let current_score = Self::casing_score(current_prefix);

        if new_score == current_score {
            // Same casing quality - use alphabetical order for determinism
            new_prefix < current_prefix
        } else {
            // Higher score is better
            new_score > current_score
        }
    }

    /// Score the casing quality of a prefix.
    /// Higher scores are better.
    /// CamelCase (mixed case starting with uppercase) = 3
    /// Starts with uppercase but all same case = 2
    /// Mixed case starting with lowercase = 1
    /// All lowercase or all uppercase = 0
    fn casing_score(prefix: &str) -> u8 {
        let mut chars = prefix.chars();
        let Some(first) = chars.next() else {
            return 0;
        };

        let starts_upper = first.is_ascii_uppercase();
        let has_upper = starts_upper || chars.clone().any(|c| c.is_ascii_uppercase());
        let has_lower = first.is_ascii_lowercase() || chars.any(|c| c.is_ascii_lowercase());

        match (starts_upper, has_upper, has_lower) {
            // CamelCase: starts uppercase, has both upper and lower (e.g., "PhotoLab")
            (true, true, true) => 3,
            // Starts uppercase, single case (e.g., "PHOTOLAB")
            (true, true, false) => 2,
            // Mixed case but starts lowercase (e.g., "photoLab")
            (false, true, true) => 1,
            // All lowercase (e.g., "photolab") or edge cases
            _ => 0,
        }
    }

    /// Convert this builder into a final `PrefixGroup`.
    pub(crate) fn into_prefix_group(self) -> (String, PrefixGroup) {
        (
            self.original_prefix,
            PrefixGroup::new(self.files, self.part_count, self.min_start_position),
        )
    }
}

#[cfg(test)]
mod test_casing_score {
    use super::*;

    #[test]
    fn camel_case_scores_highest() {
        // CamelCase (starts uppercase, has both upper and lower) = 3
        assert_eq!(PrefixGroupBuilder::casing_score("PhotoLab"), 3);
        assert_eq!(PrefixGroupBuilder::casing_score("NeonLight"), 3);
        assert_eq!(PrefixGroupBuilder::casing_score("MyApp"), 3);
    }

    #[test]
    fn all_uppercase_scores_second() {
        // Starts uppercase, all same case = 2
        assert_eq!(PrefixGroupBuilder::casing_score("PHOTOLAB"), 2);
        assert_eq!(PrefixGroupBuilder::casing_score("NEONLIGHT"), 2);
        assert_eq!(PrefixGroupBuilder::casing_score("ABC"), 2);
    }

    #[test]
    fn mixed_case_starting_lowercase_scores_third() {
        // Mixed case but starts lowercase = 1
        assert_eq!(PrefixGroupBuilder::casing_score("photoLab"), 1);
        assert_eq!(PrefixGroupBuilder::casing_score("neonLight"), 1);
    }

    #[test]
    fn all_lowercase_scores_zero() {
        // All lowercase = 0
        assert_eq!(PrefixGroupBuilder::casing_score("photolab"), 0);
        assert_eq!(PrefixGroupBuilder::casing_score("neonlight"), 0);
    }

    #[test]
    fn empty_string_scores_zero() {
        assert_eq!(PrefixGroupBuilder::casing_score(""), 0);
    }
}

#[cfg(test)]
mod test_is_better_prefix {
    use super::*;

    #[test]
    fn camel_case_preferred_over_all_uppercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PhotoLab", "PHOTOLAB"));
        assert!(!PrefixGroupBuilder::is_better_prefix("PHOTOLAB", "PhotoLab"));
    }

    #[test]
    fn camel_case_preferred_over_all_lowercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PhotoLab", "photolab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("photolab", "PhotoLab"));
    }

    #[test]
    fn all_uppercase_preferred_over_all_lowercase() {
        assert!(PrefixGroupBuilder::is_better_prefix("PHOTOLAB", "photolab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("photolab", "PHOTOLAB"));
    }

    #[test]
    fn alphabetical_order_breaks_ties() {
        // Both CamelCase - alphabetical order wins
        assert!(PrefixGroupBuilder::is_better_prefix("NeonLight", "PhotoLab"));
        assert!(!PrefixGroupBuilder::is_better_prefix("PhotoLab", "NeonLight"));

        // Both all lowercase - alphabetical order wins
        assert!(PrefixGroupBuilder::is_better_prefix("abc", "xyz"));
        assert!(!PrefixGroupBuilder::is_better_prefix("xyz", "abc"));
    }

    #[test]
    fn same_prefix_not_better() {
        assert!(!PrefixGroupBuilder::is_better_prefix("PhotoLab", "PhotoLab"));
    }
}
