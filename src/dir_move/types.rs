use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::path_to_filename_string;

/// Result of prompting the user for a directory name in create mode.
#[derive(Debug)]
pub enum PromptResult {
    /// User confirmed: move files to this directory path.
    Confirmed(PathBuf),
    /// User skipped this group.
    Skipped,
    /// User chose to skip and save the group name to `ignored_group_names` in the config file.
    SaveToIgnored,
}

/// Information about a directory used for matching files to move.
#[derive(Debug)]
pub struct DirectoryInfo {
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    pub name: String,
}

/// Information about a file for grouping purposes.
/// Uses `Cow` for efficient string handling - avoids cloning when possible.
///
/// Includes pre-computed split and lowercased parts to avoid redundant work
/// in hot loops where `prefix_matches_normalized` and `parts_are_contiguous_in_original`
/// are called O(N × K × N) times.
pub struct FileInfo<'a> {
    /// Path to the file.
    pub path: Cow<'a, Path>,
    /// Original filename after stripping ignored prefixes (used for contiguity checks).
    pub original_name: Cow<'a, str>,
    /// Filtered filename with numeric/resolution/glue parts removed (used for prefix matching).
    pub filtered_name: Cow<'a, str>,
    /// Pre-computed: `original_name` split by `'.'` (owned, for thread safety).
    pub original_parts: Vec<String>,
    /// Pre-computed filtered name parts and their combinations for efficient prefix matching.
    pub filtered_parts: FilteredParts,
}

/// Pre-computed part combinations from a filtered filename for efficient prefix matching.
///
/// Stores single, 2-part, and 3-part combinations in both lowercased and original-cased
/// forms. This eliminates redundant `split('.')`, `to_lowercase()`, and `format!()` calls
/// in the O(N × K × N) hot loops inside `find_prefix_candidates` and
/// `prefix_matches_normalized`.
pub struct FilteredParts {
    /// Single parts, lowercased (e.g., `["photo", "lab", "image"]`).
    pub parts_lower: Vec<String>,
    /// Contiguous 2-part combinations, lowercased (e.g., `["photolab", "labimage"]`).
    pub two_parts_lower: Vec<String>,
    /// Contiguous 3-part combinations, lowercased (e.g., `["photolabimage"]`).
    pub three_parts_lower: Vec<String>,
    /// Single parts, original casing (e.g., `["Photo", "Lab", "Image"]`).
    pub parts_original: Vec<String>,
    /// Contiguous 2-part combinations, original casing (e.g., `["PhotoLab", "LabImage"]`).
    pub two_parts_original: Vec<String>,
    /// Contiguous 3-part combinations, original casing (e.g., `["PhotoLabImage"]`).
    pub three_parts_original: Vec<String>,
}

/// Information about a file move operation.
#[derive(Debug)]
pub struct MoveInfo {
    /// Source path of the file or directory.
    pub source: PathBuf,
    /// Target path of the file or directory.
    pub target: PathBuf,
}

/// A pair of directories to merge: source (with prefix) into target (without prefix).
/// Used during the `merge_prefixed_directories` operation.
#[derive(Debug)]
pub struct MergePair {
    /// Directory with the `prefix_ignore` prefix (to be merged from).
    pub source: PathBuf,
    /// Directory without the prefix (to be merged into).
    pub target: PathBuf,
}

/// A candidate prefix for grouping files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixCandidate<'a> {
    /// The prefix string (e.g., "Studio.Name" or "`StudioName`").
    pub prefix: Cow<'a, str>,
    /// Number of files matching this prefix.
    pub match_count: usize,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    pub part_count: usize,
    /// Position in the filename where this prefix starts (0 = beginning).
    /// Lower values indicate prefixes closer to the start of the filename.
    pub start_position: usize,
}

/// Information about what needs to be moved during an unpack operation.
#[derive(Debug, Default)]
pub struct UnpackInfo {
    /// Files to move.
    pub file_moves: Vec<MoveInfo>,
    /// Directories to move directly.
    pub directory_moves: Vec<MoveInfo>,
}

/// A group of files that share a common prefix.
/// Used for organizing files into directories based on their name prefixes.
#[derive(Debug, Clone)]
pub struct PrefixGroup {
    /// Files belonging to this prefix group.
    pub files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix (1, 2, or 3).
    /// Higher values indicate more specific prefixes.
    pub part_count: usize,
    /// Minimum start position where this prefix appears across all files.
    /// Lower values indicate prefixes closer to the start of filenames.
    pub min_start_position: usize,
}

/// Intermediate data structure for building prefix groups.
/// A validated prefix candidate ready to be merged into prefix groups.
/// Produced during the parallel first pass of `collect_all_prefix_groups`.
pub struct ValidCandidate {
    /// Normalized group key (lowercase, no dots).
    pub key: String,
    /// Path to the file this candidate belongs to.
    pub file_path: PathBuf,
    /// Number of dot-separated parts in the prefix.
    pub part_count: usize,
    /// Whether the prefix is a concatenated (no-dot) form.
    pub is_concatenated: bool,
    /// Position in the filename where this prefix starts.
    pub start_position: usize,
    /// The original prefix string.
    pub prefix: String,
}

/// Used during the collection phase before converting to final `PrefixGroup`.
#[derive(Debug)]
pub struct PrefixGroupBuilder {
    /// The original prefix string (may contain dots).
    pub original_prefix: String,
    /// Files belonging to this prefix group.
    pub files: Vec<PathBuf>,
    /// Number of dot-separated parts in the prefix.
    pub part_count: usize,
    /// Whether the prefix has a concatenated (no-dot) form.
    pub has_concatenated_form: bool,
    /// Minimum start position where this prefix appears across all files.
    pub min_start_position: usize,
}

impl FileInfo<'_> {
    /// Create a new `FileInfo` with owned strings.
    /// Pre-computes split and lowercased parts for efficient matching in hot loops.
    pub fn new(path: PathBuf, original_name: String, filtered_name: String) -> Self {
        let original_parts: Vec<String> = original_name.split('.').map(String::from).collect();
        let filtered_parts = FilteredParts::new(&filtered_name);

        Self {
            path: Cow::Owned(path),
            original_name: Cow::Owned(original_name),
            filtered_name: Cow::Owned(filtered_name),
            original_parts,
            filtered_parts,
        }
    }

    /// Get the path as a `PathBuf`.
    #[must_use]
    pub fn path_buf(&self) -> PathBuf {
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

impl FilteredParts {
    /// Create a new `FilteredParts` by splitting the filtered name on `'.'` and
    /// pre-computing all single, 2-part, and 3-part combinations in both lowercased
    /// and original-cased forms.
    pub fn new(filtered_name: &str) -> Self {
        let parts_original: Vec<String> = filtered_name.split('.').map(String::from).collect();

        let parts_lower: Vec<String> = parts_original.iter().map(|p| p.to_lowercase()).collect();

        let two_parts_lower: Vec<String> = parts_lower
            .windows(2)
            .map(|window| format!("{}{}", window[0], window[1]))
            .collect();

        let three_parts_lower: Vec<String> = parts_lower
            .windows(3)
            .map(|window| format!("{}{}{}", window[0], window[1], window[2]))
            .collect();

        let two_parts_original: Vec<String> = parts_original
            .windows(2)
            .map(|window| format!("{}{}", window[0], window[1]))
            .collect();

        let three_parts_original: Vec<String> = parts_original
            .windows(3)
            .map(|window| format!("{}{}{}", window[0], window[1], window[2]))
            .collect();

        Self {
            parts_lower,
            two_parts_lower,
            three_parts_lower,
            parts_original,
            two_parts_original,
            three_parts_original,
        }
    }

    /// Check if any pre-computed part combination matches the given normalized target.
    ///
    /// Checks single parts, 2-part, and 3-part lowered combinations against
    /// `target_normalized`, requiring a valid word boundary (checked on the
    /// corresponding original-cased parts) for `starts_with` matches.
    #[must_use]
    pub fn prefix_matches_normalized(&self, target_normalized: &str) -> bool {
        if target_normalized.is_empty() {
            return false;
        }

        // Check all single parts (exact match or starts with at word boundary)
        for (part, original) in self.parts_lower.iter().zip(self.parts_original.iter()) {
            if *part == target_normalized
                || (part.starts_with(target_normalized)
                    && Self::has_word_boundary_at(original, target_normalized.len()))
            {
                return true;
            }
        }

        // Check all 2-part combinations (exact match or starts with at word boundary)
        for (combined, original) in self.two_parts_lower.iter().zip(self.two_parts_original.iter()) {
            if *combined == target_normalized
                || (combined.starts_with(target_normalized)
                    && Self::has_word_boundary_at(original, target_normalized.len()))
            {
                return true;
            }
        }

        // Check all 3-part combinations (exact match or starts with at word boundary)
        for (combined, original) in self.three_parts_lower.iter().zip(self.three_parts_original.iter()) {
            if *combined == target_normalized
                || (combined.starts_with(target_normalized)
                    && Self::has_word_boundary_at(original, target_normalized.len()))
            {
                return true;
            }
        }

        false
    }

    /// Check if there is a valid word boundary at byte position `prefix_len`
    /// in the **original-cased** text.
    ///
    /// The position is a byte offset (typically from `str::len()` on a lowercased
    /// counterpart). If it does not fall on a UTF-8 character boundary in
    /// `original_text`, the function conservatively returns `false`.
    ///
    /// Handles Unicode letters (including Scandic characters such as Ä, Ö, Ü, Å)
    /// by inspecting the `char` values on either side of the boundary.
    ///
    /// # Panics
    ///
    /// Panics if `prefix_len` is within `(0, original_text.len())` but falls on a
    /// char boundary where the preceding or following slice is unexpectedly empty.
    /// This cannot happen when the caller respects the boundary and length guards.
    #[must_use]
    pub fn has_word_boundary_at(original_text: &str, prefix_len: usize) -> bool {
        if prefix_len == 0 || prefix_len >= original_text.len() {
            return true;
        }

        // If the byte offset lands in the middle of a multi-byte character,
        // it is not a valid boundary.
        if !original_text.is_char_boundary(prefix_len) {
            return false;
        }

        // Safe to split: both slices are valid UTF-8.
        let prev = original_text[..prefix_len].chars().next_back().expect("prefix_len > 0");
        let next = original_text[prefix_len..].chars().next().expect("prefix_len < len");

        if next.is_uppercase() {
            true
        } else if next.is_ascii_digit() {
            !prev.is_ascii_digit()
        } else if !next.is_alphanumeric() {
            true
        } else {
            // next is a lowercase letter (ASCII or Unicode) — only a boundary
            // if the previous character was a digit.
            prev.is_ascii_digit()
        }
    }
}

impl DirectoryInfo {
    /// Create a new `DirectoryInfo` from a directory path.
    /// Normalizes the directory name to lowercase with dots replaced by spaces.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
}

impl MoveInfo {
    /// Create a new `MoveInfo`.
    #[must_use]
    pub const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

impl MergePair {
    /// Create a new `MergePair`.
    #[must_use]
    pub const fn new(source: PathBuf, target: PathBuf) -> Self {
        Self { source, target }
    }
}

impl<'a> PrefixCandidate<'a> {
    /// Create a new `PrefixCandidate`.
    #[must_use]
    pub const fn new(prefix: Cow<'a, str>, match_count: usize, part_count: usize, start_position: usize) -> Self {
        Self {
            prefix,
            match_count,
            part_count,
            start_position,
        }
    }
}

impl PrefixGroup {
    /// Create a new `PrefixGroup`.
    #[must_use]
    pub const fn new(files: Vec<PathBuf>, part_count: usize, min_start_position: usize) -> Self {
        Self {
            files,
            part_count,
            min_start_position,
        }
    }
}

impl PrefixGroupBuilder {
    /// Create a new `PrefixGroupBuilder`.
    #[must_use]
    pub fn new(
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
    pub fn add_file(
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
        } else if !self.has_concatenated_form {
            // Without a concatenated form, still choose a deterministic dotted display prefix.
            // This avoids platform-dependent directory names caused by filesystem iteration order.
            let should_replace = Self::is_better_prefix(&prefix, &self.original_prefix);
            if should_replace {
                self.original_prefix = prefix;
            }
        }
    }

    /// Determine if `new_prefix` is better than `current_prefix` for display.
    /// Prefers CamelCase (mixed case) over all-uppercase or all-lowercase.
    /// Uses alphabetical order as final tiebreaker for determinism.
    #[must_use]
    pub fn is_better_prefix(new_prefix: &str, current_prefix: &str) -> bool {
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
    #[must_use]
    pub fn casing_score(prefix: &str) -> u8 {
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
    #[must_use]
    pub fn into_prefix_group(self) -> (String, PrefixGroup) {
        (
            self.original_prefix,
            PrefixGroup::new(self.files, self.part_count, self.min_start_position),
        )
    }
}

#[cfg(test)]
mod test_prefix_group_builder {
    use super::*;

    #[test]
    fn dotted_prefix_prefers_stable_display_casing() {
        let first_file = PathBuf::from("summer.vacation.photo4.jpg");
        let second_file = PathBuf::from("SUMMER.VACATION.Photo3.jpg");
        let third_file = PathBuf::from("Summer.Vacation.Photo1.jpg");

        let mut builder = PrefixGroupBuilder::new("summer.vacation".to_string(), first_file, 2, false, 0);
        builder.add_file(second_file, 2, false, 0, "SUMMER.VACATION".to_string());
        builder.add_file(third_file, 2, false, 0, "Summer.Vacation".to_string());

        assert_eq!(builder.original_prefix, "Summer.Vacation");
    }

    #[test]
    fn concatenated_form_is_not_replaced_by_better_dotted_casing() {
        let first_file = PathBuf::from("summervacation_photo1.jpg");
        let second_file = PathBuf::from("Summer.Vacation.Photo2.jpg");

        let mut builder = PrefixGroupBuilder::new("summervacation".to_string(), first_file, 1, true, 0);
        builder.add_file(second_file, 2, false, 0, "Summer.Vacation".to_string());

        // Concatenated form should be retained
        assert_eq!(builder.original_prefix, "summervacation");
    }
}
