use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use cli_tools::path_to_filename_string;

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
    pub(crate) path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    pub(crate) name: String,
}

/// Information about a file for grouping purposes.
/// Uses `Cow` for efficient string handling - avoids cloning when possible.
///
/// Includes pre-computed split and lowercased parts to avoid redundant work
/// in hot loops where `prefix_matches_normalized` and `parts_are_contiguous_in_original`
/// are called O(N × K × N) times.
pub struct FileInfo<'a> {
    /// Path to the file.
    pub(crate) path: Cow<'a, Path>,
    /// Original filename after stripping ignored prefixes (used for contiguity checks).
    pub(crate) original_name: Cow<'a, str>,
    /// Filtered filename with numeric/resolution/glue parts removed (used for prefix matching).
    pub(crate) filtered_name: Cow<'a, str>,
    /// Pre-computed: `original_name` split by `'.'` (owned, for thread safety).
    pub(crate) original_parts: Vec<String>,
    /// Pre-computed filtered name parts and their combinations for efficient prefix matching.
    pub(crate) filtered_parts: FilteredParts,
}

/// Pre-computed part combinations from a filtered filename for efficient prefix matching.
///
/// Stores single, 2-part, and 3-part combinations in both lowercased and original-cased
/// forms. This eliminates redundant `split('.')`, `to_lowercase()`, and `format!()` calls
/// in the O(N × K × N) hot loops inside `find_prefix_candidates` and
/// `prefix_matches_normalized`.
pub struct FilteredParts {
    /// Single parts, lowercased (e.g., `["photo", "lab", "image"]`).
    pub(crate) parts_lower: Vec<String>,
    /// Contiguous 2-part combinations, lowercased (e.g., `["photolab", "labimage"]`).
    pub(crate) two_parts_lower: Vec<String>,
    /// Contiguous 3-part combinations, lowercased (e.g., `["photolabimage"]`).
    pub(crate) three_parts_lower: Vec<String>,
    /// Single parts, original casing (e.g., `["Photo", "Lab", "Image"]`).
    pub(crate) parts_original: Vec<String>,
    /// Contiguous 2-part combinations, original casing (e.g., `["PhotoLab", "LabImage"]`).
    pub(crate) two_parts_original: Vec<String>,
    /// Contiguous 3-part combinations, original casing (e.g., `["PhotoLabImage"]`).
    pub(crate) three_parts_original: Vec<String>,
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
    /// Pre-computes split and lowercased parts for efficient matching in hot loops.
    pub(crate) fn new(path: PathBuf, original_name: String, filtered_name: String) -> Self {
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

impl FilteredParts {
    /// Create a new `FilteredParts` by splitting the filtered name on `'.'` and
    /// pre-computing all single, 2-part, and 3-part combinations in both lowercased
    /// and original-cased forms.
    pub(crate) fn new(filtered_name: &str) -> Self {
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
    pub(crate) fn prefix_matches_normalized(&self, target_normalized: &str) -> bool {
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

    /// Check if there is a valid word boundary at position `prefix_len` in the
    /// **original-cased** text.
    ///
    /// Assumes ASCII-compatible filenames.
    pub(crate) fn has_word_boundary_at(original_text: &str, prefix_len: usize) -> bool {
        if prefix_len == 0 || prefix_len >= original_text.len() {
            return true;
        }
        let prev = original_text.as_bytes()[prefix_len - 1];
        let next = original_text.as_bytes()[prefix_len];

        match next {
            b'A'..=b'Z' => true,
            b'0'..=b'9' => !prev.is_ascii_digit(),
            c if !c.is_ascii_alphanumeric() => true,
            _ => prev.is_ascii_digit(),
        }
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
/// A validated prefix candidate ready to be merged into prefix groups.
/// Produced during the parallel first pass of `collect_all_prefix_groups`.
pub struct ValidCandidate {
    /// Normalized group key (lowercase, no dots).
    pub(crate) key: String,
    /// Path to the file this candidate belongs to.
    pub(crate) file_path: PathBuf,
    /// Number of dot-separated parts in the prefix.
    pub(crate) part_count: usize,
    /// Whether the prefix is a concatenated (no-dot) form.
    pub(crate) is_concatenated: bool,
    /// Position in the filename where this prefix starts.
    pub(crate) start_position: usize,
    /// The original prefix string.
    pub(crate) prefix: String,
}

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

#[cfg(test)]
mod test_filtered_parts_new {
    use super::*;

    #[test]
    fn single_parts_split_correctly() {
        let parts = FilteredParts::new("Photo.Lab.Image.jpg");
        assert_eq!(parts.parts_original, ["Photo", "Lab", "Image", "jpg"]);
        assert_eq!(parts.parts_lower, ["photo", "lab", "image", "jpg"]);
    }

    #[test]
    fn two_part_combinations_computed() {
        let parts = FilteredParts::new("Photo.Lab.Image");
        assert_eq!(parts.two_parts_lower, ["photolab", "labimage"]);
        assert_eq!(parts.two_parts_original, ["PhotoLab", "LabImage"]);
    }

    #[test]
    fn three_part_combinations_computed() {
        let parts = FilteredParts::new("Photo.Lab.Image.Extra");
        assert_eq!(parts.three_parts_lower, ["photolabimage", "labimageextra"]);
        assert_eq!(parts.three_parts_original, ["PhotoLabImage", "LabImageExtra"]);
    }

    #[test]
    fn single_part_name_has_no_combinations() {
        let parts = FilteredParts::new("standalone");
        assert_eq!(parts.parts_original, ["standalone"]);
        assert_eq!(parts.parts_lower, ["standalone"]);
        assert!(parts.two_parts_lower.is_empty());
        assert!(parts.three_parts_lower.is_empty());
        assert!(parts.two_parts_original.is_empty());
        assert!(parts.three_parts_original.is_empty());
    }

    #[test]
    fn two_part_name_has_no_three_part_combinations() {
        let parts = FilteredParts::new("Photo.Lab");
        assert_eq!(parts.two_parts_lower, ["photolab"]);
        assert_eq!(parts.two_parts_original, ["PhotoLab"]);
        assert!(parts.three_parts_lower.is_empty());
        assert!(parts.three_parts_original.is_empty());
    }

    #[test]
    fn preserves_original_casing() {
        let parts = FilteredParts::new("PhotoLab.ImagePRO.Test");
        assert_eq!(parts.parts_original, ["PhotoLab", "ImagePRO", "Test"]);
        assert_eq!(parts.two_parts_original, ["PhotoLabImagePRO", "ImagePROTest"]);
        assert_eq!(parts.three_parts_original, ["PhotoLabImagePROTest"]);
    }

    #[test]
    fn lowercased_parts_are_consistent_with_original() {
        let parts = FilteredParts::new("UPPER.Mixed.lower");
        assert_eq!(parts.parts_lower, ["upper", "mixed", "lower"]);
        assert_eq!(parts.two_parts_lower, ["uppermixed", "mixedlower"]);
        assert_eq!(parts.three_parts_lower, ["uppermixedlower"]);
    }

    #[test]
    fn empty_string_produces_single_empty_part() {
        let parts = FilteredParts::new("");
        assert_eq!(parts.parts_original, [""]);
        assert_eq!(parts.parts_lower, [""]);
        assert!(parts.two_parts_lower.is_empty());
        assert!(parts.three_parts_lower.is_empty());
    }

    #[test]
    fn many_parts_produce_correct_combination_counts() {
        let parts = FilteredParts::new("A.B.C.D.E");
        assert_eq!(parts.parts_original.len(), 5);
        assert_eq!(parts.two_parts_original.len(), 4);
        assert_eq!(parts.three_parts_original.len(), 3);
    }
}

#[cfg(test)]
mod test_filtered_parts_prefix_matches {
    use super::*;

    #[test]
    fn single_part_exact_match() {
        let parts = FilteredParts::new("PhotoLab.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn single_part_exact_match_case_insensitive() {
        let parts = FilteredParts::new("PHOTOLAB.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn two_part_combined_exact_match() {
        let parts = FilteredParts::new("Photo.Lab.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn three_part_combined_exact_match() {
        let parts = FilteredParts::new("Ph.oto.Lab.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn no_match_returns_false() {
        let parts = FilteredParts::new("Other.Album.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn match_at_middle_position() {
        let parts = FilteredParts::new("Extra.StudioName.video.mp4");
        assert!(parts.prefix_matches_normalized("studioname"));
    }

    #[test]
    fn two_part_match_at_middle_position() {
        let parts = FilteredParts::new("Extra.Photo.Lab.video.mp4");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_at_word_boundary_uppercase() {
        // "PhotoLabTV" — 'T' after "PhotoLab" is uppercase, valid boundary
        let parts = FilteredParts::new("PhotoLabTV.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_rejected_at_lowercase_continuation() {
        // "PhotoLabs" — 's' after "PhotoLab" is lowercase, NOT a boundary
        let parts = FilteredParts::new("PhotoLabs.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_at_digit_boundary() {
        // "Studio2" — '2' after "Studio" is a digit following a letter, valid boundary
        let parts = FilteredParts::new("Studio2.Video.mp4");
        assert!(parts.prefix_matches_normalized("studio"));
    }

    #[test]
    fn two_part_combined_starts_with_at_word_boundary() {
        // "Photo.LabPro" combined is "PhotoLabPro", starts with "photolab" at uppercase 'P'
        let parts = FilteredParts::new("Photo.LabPro.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn two_part_combined_starts_with_rejected_at_lowercase() {
        // "Photo.Labs" combined is "PhotoLabs", starts with "photolab" but 's' is lowercase
        let parts = FilteredParts::new("Photo.Labs.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn three_part_combined_starts_with_at_word_boundary() {
        // "Al.pha.BetaGamma" combined is "AlphaBetaGamma", starts with "alphabeta" at uppercase 'G'
        let parts = FilteredParts::new("Al.pha.BetaGamma.video.mp4");
        assert!(parts.prefix_matches_normalized("alphabeta"));
    }

    #[test]
    fn three_part_combined_starts_with_rejected_at_lowercase() {
        // "Al.pha.Betas" combined is "AlphaBetas", starts with "alphabeta" but 's' is lowercase
        let parts = FilteredParts::new("Al.pha.Betas.video.mp4");
        assert!(!parts.prefix_matches_normalized("alphabeta"));
    }

    #[test]
    fn single_part_file_exact_match() {
        let parts = FilteredParts::new("standalone");
        assert!(parts.prefix_matches_normalized("standalone"));
    }

    #[test]
    fn single_part_file_no_match() {
        let parts = FilteredParts::new("standalone");
        assert!(!parts.prefix_matches_normalized("other"));
    }

    #[test]
    fn empty_target_matches_nothing() {
        let parts = FilteredParts::new("Some.File.mp4");
        assert!(!parts.prefix_matches_normalized(""));
    }

    #[test]
    fn prefix_not_at_start_of_any_part() {
        // "XPhotoLab" does not start with "photolab" — "x" comes first
        let parts = FilteredParts::new("XPhotoLab.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn all_uppercase_name_with_word_boundary() {
        // "PHOTOLABPRO" — all uppercase, boundary at position 8 sees 'P' (uppercase) → match
        let parts = FilteredParts::new("PHOTOLABPRO.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn intense_not_matched_by_intensely() {
        let parts = FilteredParts::new("Intensely.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("intense"));
    }

    #[test]
    fn intense_exact_match() {
        let parts = FilteredParts::new("Intense.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("intense"));
    }
}
