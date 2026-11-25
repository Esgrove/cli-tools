//! Prefix candidates that file names share, used to group files in create mode.
//!
//! `FileInfo` holds a file's original and filtered names with their parts precomputed,
//! and `find_prefix_candidates` finds every one, two, and three part prefix that enough files share,
//! checking that the prefix parts appear contiguously in each original file name.

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;

use crate::prefix_index::PrefixIndex;
use crate::utils::normalize_name;

/// A file's original and filtered names, used to find the prefixes it shares with other files.
///
/// The split and lowercased parts are computed once,
/// since prefix matching and contiguity checks run for every candidate prefix of every file.
pub struct FileInfo {
    /// Path to the file.
    pub path: PathBuf,
    /// Original filename after stripping ignored prefixes (used for contiguity checks).
    pub original_name: String,
    /// Filtered filename with numeric/resolution/glue parts removed (used for prefix matching).
    pub filtered_name: String,
    /// Pre-computed: `original_name` split by `'.'`.
    original_parts: Vec<String>,
    /// Pre-computed: `original_parts` lowercased, in the same order.
    original_parts_lower: Vec<String>,
    /// Pre-computed filtered name parts and their combinations for efficient prefix matching.
    filtered_parts: FilteredParts,
}

/// Pre-computed part combinations from a filtered filename for efficient prefix matching.
///
/// Stores single, 2-part, and 3-part combinations in both lowercased and original-cased forms.
/// This eliminates redundant `split('.')`, `to_lowercase()`,
/// and `format!()` calls when every candidate prefix is checked against every file.
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

impl FileInfo {
    /// Create a new `FileInfo`, pre-computing the split and lowercased parts for matching in hot loops.
    pub fn new(path: PathBuf, original_name: String, filtered_name: String) -> Self {
        let original_parts: Vec<String> = original_name.split('.').map(String::from).collect();
        let original_parts_lower: Vec<String> = original_parts.iter().map(|part| part.to_lowercase()).collect();
        let filtered_parts = FilteredParts::new(&filtered_name);

        Self {
            path,
            original_name,
            filtered_name,
            original_parts,
            original_parts_lower,
            filtered_parts,
        }
    }

    /// Return `original_name` split on `'.'`.
    #[must_use]
    pub fn original_parts(&self) -> &[String] {
        &self.original_parts
    }

    /// Return `original_parts` lowercased, in the same order.
    #[must_use]
    pub fn original_parts_lower(&self) -> &[String] {
        &self.original_parts_lower
    }

    /// Return the pre-computed part combinations of `filtered_name`.
    #[must_use]
    pub const fn filtered_parts(&self) -> &FilteredParts {
        &self.filtered_parts
    }
}

impl fmt::Debug for FileInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileInfo")
            .field("path", &self.path)
            .field("original_name", &self.original_name)
            .field("filtered_name", &self.filtered_name)
            .finish_non_exhaustive()
    }
}

impl FilteredParts {
    /// Create a new `FilteredParts` by splitting the filtered name on `'.'`
    /// and pre-computing all single, 2-part,
    /// and 3-part combinations in both lowercased and original-cased forms.
    pub fn new(filtered_name: &str) -> Self {
        let parts_original: Vec<String> = filtered_name.split('.').map(String::from).collect();

        let parts_lower: Vec<String> = parts_original.iter().map(|part| part.to_lowercase()).collect();

        let two_parts_lower: Vec<String> = parts_lower
            .iter()
            .zip(parts_lower.iter().skip(1))
            .map(|(first, second)| format!("{first}{second}"))
            .collect();

        let three_parts_lower: Vec<String> = parts_lower
            .iter()
            .zip(parts_lower.iter().skip(1))
            .zip(parts_lower.iter().skip(2))
            .map(|((first, second), third)| format!("{first}{second}{third}"))
            .collect();

        let two_parts_original: Vec<String> = parts_original
            .iter()
            .zip(parts_original.iter().skip(1))
            .map(|(first, second)| format!("{first}{second}"))
            .collect();

        let three_parts_original: Vec<String> = parts_original
            .iter()
            .zip(parts_original.iter().skip(1))
            .zip(parts_original.iter().skip(2))
            .map(|((first, second), third)| format!("{first}{second}{third}"))
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
    /// Checks single parts, 2-part, and 3-part lowered combinations against `target_normalized`,
    /// requiring a valid word boundary (checked on the corresponding original-cased parts)
    /// for `starts_with` matches.
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

    /// Check if there is a valid word boundary at byte position `boundary` in the **original-cased** text.
    ///
    /// The position is a byte offset (typically from `str::len()` on a lowercased counterpart).
    /// If it does not fall on a UTF-8 character boundary in `original_text`,
    /// the function conservatively returns `false`.
    ///
    /// Handles Unicode letters (including Scandic characters such as Ä, Ö, Ü, Å)
    /// by inspecting the `char` values on either side of the boundary.
    #[must_use]
    pub fn has_word_boundary_at(original_text: &str, boundary: usize) -> bool {
        if boundary == 0 || boundary >= original_text.len() {
            return true;
        }

        // If the byte offset lands in the middle of a multi-byte character,
        // it is not a valid boundary.
        if !original_text.is_char_boundary(boundary) {
            return false;
        }

        let Some((before, after)) = original_text.split_at_checked(boundary) else {
            return false;
        };
        let (Some(prev), Some(next)) = (before.chars().next_back(), after.chars().next()) else {
            return false;
        };

        if next.is_uppercase() {
            true
        } else if next.is_ascii_digit() {
            !prev.is_ascii_digit()
        } else if !next.is_alphanumeric() {
            true
        } else {
            // next is a lowercase letter (ASCII or Unicode).
            // Only a boundary if the previous character was a digit.
            prev.is_ascii_digit()
        }
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

/// Find prefix candidates for a file from any position in the filename.
///
/// Returns candidates in priority order: 3-part sequences, 2-part sequences, 1-part sequences.
/// Longer prefixes are preferred as they provide more specific grouping.
/// Also handles case variations and dot-separated vs concatenated forms.
///
/// Unlike prefix-only matching, this function extracts candidates from all positions in the filename,
/// allowing common group names that appear in the middle of filenames to be detected.
///
/// The file extension (last part after the final dot) is excluded from candidate generation.
///
/// Builds a [`PrefixIndex`] of `all_files` for the call,
/// so use [`find_prefix_candidates_indexed`] with a shared index when calling it for many files.
#[must_use]
pub fn find_prefix_candidates<'a>(
    file_name: &'a str,
    all_files: &[FileInfo],
    min_group_size: usize,
    min_prefix_chars: usize,
) -> Vec<PrefixCandidate<'a>> {
    let index = PrefixIndex::new(all_files);
    find_prefix_candidates_indexed(file_name, all_files, &index, min_group_size, min_prefix_chars)
}

/// Find prefix candidates like [`find_prefix_candidates`], looking matching files up in an index of `all_files`.
///
/// The index must have been built from the same `all_files` slice.
#[must_use]
pub fn find_prefix_candidates_indexed<'a>(
    file_name: &'a str,
    all_files: &[FileInfo],
    index: &PrefixIndex<'_>,
    min_group_size: usize,
    min_prefix_chars: usize,
) -> Vec<PrefixCandidate<'a>> {
    let mut candidates: Vec<PrefixCandidate<'a>> = Vec::new();
    let mut seen_normalized: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut matches: Vec<usize> = Vec::new();
    let contiguous_count = |matches: &[usize], prefix_parts: &[&str], prefix_combined: &str| {
        matches
            .iter()
            .filter_map(|&file_index| all_files.get(file_index))
            .filter(|file| {
                parts_are_contiguous_lowered(
                    &file.original_parts,
                    &file.original_parts_lower,
                    prefix_parts,
                    prefix_combined,
                )
            })
            .count()
    };

    // Get the filename without the extension to avoid matching file extensions as group names
    let name_without_extension = file_name.rsplit_once('.').map_or(file_name, |(name, _ext)| name);

    // Check all 3-part sequences from any position (excluding extension)
    for (position, three_part) in get_all_n_part_sequences(name_without_extension, 3)
        .into_iter()
        .enumerate()
    {
        let char_count = count_prefix_chars(three_part);
        if char_count >= min_prefix_chars {
            let three_part_normalized = normalize_name(three_part);
            // Skip if we've already processed this normalized form
            if seen_normalized.contains(&three_part_normalized) {
                continue;
            }
            seen_normalized.insert(three_part_normalized.clone());

            let prefix_parts: Vec<&str> = three_part.split('.').collect();
            let prefix_combined = prefix_parts.join("").to_lowercase();
            index.matching_files(&three_part_normalized, &mut matches);
            let match_count = contiguous_count(&matches, &prefix_parts, &prefix_combined);
            if match_count >= min_group_size {
                candidates.push(PrefixCandidate::new(
                    Cow::Borrowed(three_part),
                    match_count,
                    3,
                    position,
                ));
            }
        }
    }

    // Check all 2-part sequences from any position (excluding extension)
    for (position, two_part) in get_all_n_part_sequences(name_without_extension, 2)
        .into_iter()
        .enumerate()
    {
        let char_count = count_prefix_chars(two_part);
        if char_count >= min_prefix_chars {
            let two_part_normalized = normalize_name(two_part);
            // Skip if we've already processed this normalized form
            if seen_normalized.contains(&two_part_normalized) {
                continue;
            }
            seen_normalized.insert(two_part_normalized.clone());

            let prefix_parts: Vec<&str> = two_part.split('.').collect();
            let prefix_combined = prefix_parts.join("").to_lowercase();
            index.matching_files(&two_part_normalized, &mut matches);
            let match_count = contiguous_count(&matches, &prefix_parts, &prefix_combined);
            if match_count >= min_group_size {
                candidates.push(PrefixCandidate::new(Cow::Borrowed(two_part), match_count, 2, position));
            }
        }
    }

    // Check all 1-part sequences from any position (excluding extension)
    for (position, single_part) in get_all_n_part_sequences(name_without_extension, 1)
        .into_iter()
        .enumerate()
    {
        if single_part.chars().count() >= min_prefix_chars {
            let single_part_normalized = single_part.to_lowercase();
            // Skip if we've already processed this normalized form
            if seen_normalized.contains(&single_part_normalized) {
                continue;
            }
            seen_normalized.insert(single_part_normalized.clone());

            index.matching_files(&single_part_normalized, &mut matches);
            let match_count = matches.len();
            if match_count >= min_group_size {
                candidates.push(PrefixCandidate::new(
                    Cow::Borrowed(single_part),
                    match_count,
                    1,
                    position,
                ));
            }
        }
    }

    candidates
}

/// Count the number of characters in a prefix, excluding dots.
/// Uses `chars().count()` to properly handle Unicode characters.
#[must_use]
pub fn count_prefix_chars(prefix: &str) -> usize {
    prefix.chars().filter(|character| *character != '.').count()
}

/// Check if pre-computed [`FileInfo`] parts contain the given normalized target.
///
/// Delegates to [`FilteredParts::prefix_matches_normalized`].
/// Grouping looks files up through [`PrefixIndex`] instead,
/// and this per-file check is the reference that the index is tested and benchmarked against.
#[must_use]
pub fn prefix_matches_normalized_precomputed(file_info: &FileInfo, target_normalized: &str) -> bool {
    file_info.filtered_parts.prefix_matches_normalized(target_normalized)
}

/// Contiguity check of [`parts_are_contiguous_with_combined`] with the original parts already lowercased.
///
/// `original_parts_lower` must hold `original_parts` lowercased, in the same order,
/// as [`FileInfo::original_parts_lower`] does.
#[must_use]
pub fn parts_are_contiguous_lowered(
    original_parts: &[String],
    original_parts_lower: &[String],
    prefix_parts: &[&str],
    prefix_combined: &str,
) -> bool {
    if prefix_parts.is_empty() {
        return true;
    }

    // Check if the prefix parts match contiguous original parts exactly
    if parts_match_a_window(original_parts, prefix_parts) {
        return true;
    }

    // Also check if prefix parts combined match a single original part (concatenated form)
    // e.g., prefix ["Photo", "Lab"] matches original part "PhotoLab"
    // Also matches if the original part STARTS WITH the prefix at a word boundary (extended form)
    // e.g., prefix ["Joseph", "Example"] matches original part "JosephExampleTV"
    // but NOT "JosephExamples" (no word boundary after "Example")
    for (original_part, original_lower) in original_parts.iter().zip(original_parts_lower) {
        if *original_lower == prefix_combined
            || (original_lower.starts_with(prefix_combined)
                && FilteredParts::has_word_boundary_at(original_part, prefix_combined.len()))
        {
            return true;
        }
    }

    // Also check if prefix parts combined match multiple contiguous original parts combined
    // e.g., prefix ["PhotoLab"] (single part) matches original ["Photo", "Lab"] (two parts)
    // Also matches if the combined parts START WITH the prefix at a word boundary
    for start_index in 0..original_parts.len() {
        if !lowered_run_can_start_with(
            original_parts_lower.get(start_index..).unwrap_or_default(),
            prefix_combined,
        ) {
            continue;
        }
        let mut combined = String::new();
        let mut combined_lower = String::new();
        for (part, part_lower) in original_parts.iter().zip(original_parts_lower).skip(start_index) {
            combined.push_str(part);
            combined_lower.push_str(part_lower);
            if combined_lower == prefix_combined
                || (combined_lower.starts_with(prefix_combined)
                    && FilteredParts::has_word_boundary_at(&combined, prefix_combined.len()))
            {
                return true;
            }
            // Stop if we've exceeded the target length (but allow starts_with)
            if combined.len() > prefix_combined.len() + 20 {
                break;
            }
        }
    }

    false
}

/// Extract all N-part sequences from a filename as string slices.
///
/// For `A.B.C.D` with a `sequence_length` of 2, returns `["A.B", "B.C", "C.D"]`.
/// This allows finding common group names that appear anywhere in filenames,
/// not just at the start.
#[must_use]
pub fn get_all_n_part_sequences(file_name: &str, sequence_length: usize) -> Vec<&str> {
    if sequence_length == 0 {
        return Vec::new();
    }

    // Collect the start position of each part (after each dot, plus position 0)
    let mut part_starts: Vec<usize> = vec![0];
    for (index, byte) in file_name.bytes().enumerate() {
        if byte == b'.' {
            part_starts.push(index + 1);
        }
    }
    let part_total = part_starts.len();

    if part_total < sequence_length {
        return Vec::new();
    }

    let mut sequences = Vec::new();
    for start in 0..=(part_total - sequence_length) {
        let Some(&start_pos) = part_starts.get(start) else {
            continue;
        };
        let end_pos = part_starts
            .get(start + sequence_length)
            .map_or(file_name.len(), |next_start| {
                // End just before the next part's dot
                next_start.saturating_sub(1)
            });

        if start_pos < end_pos
            && let Some(sequence) = file_name.get(start_pos..end_pos)
        {
            sequences.push(sequence);
        }
    }

    sequences
}

/// Whether the prefix parts equal a run of contiguous original parts, ignoring ASCII case.
fn parts_match_a_window(original_parts: &[String], prefix_parts: &[&str]) -> bool {
    original_parts.windows(prefix_parts.len()).any(|window| {
        window
            .iter()
            .zip(prefix_parts)
            .all(|(original_part, prefix_part)| original_part.eq_ignore_ascii_case(prefix_part))
    })
}

/// Whether the concatenation of the lowercased parts, once as long as `prefix`, starts with it.
///
/// Only such a run can equal or start with the prefix at any length,
/// so a start position failing this cannot match and needs no concatenated strings.
fn lowered_run_can_start_with(parts_lower: &[String], prefix: &str) -> bool {
    let mut remaining = prefix;
    for part in parts_lower {
        if remaining.len() <= part.len() {
            return part.starts_with(remaining);
        }
        match remaining.strip_prefix(part.as_str()) {
            Some(rest) => remaining = rest,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod test_helpers {
    use std::path::PathBuf;

    use super::*;
    pub use crate::test_helpers::make_test_files;
    use crate::utils::filter_numeric_resolution_and_glue_parts;

    /// Create `FileInfo` from original filenames by applying the standard filtering.
    /// The original name and filtered name are both derived from the input.
    pub fn make_filtered_files(names: &[&str]) -> Vec<FileInfo> {
        names
            .iter()
            .map(|name| {
                let filtered = filter_numeric_resolution_and_glue_parts(name);
                FileInfo::new(PathBuf::from(*name), (*name).to_string(), filtered)
            })
            .collect()
    }

    /// Helper to create a `PrefixCandidate` for test assertions.
    pub fn candidate(
        prefix: &str,
        match_count: usize,
        part_count: usize,
        start_position: usize,
    ) -> PrefixCandidate<'static> {
        PrefixCandidate::new(Cow::Owned(prefix.to_string()), match_count, part_count, start_position)
    }

    /// Check contiguity by splitting the original filename on the fly.
    pub fn parts_are_contiguous_in_original(original_filename: &str, prefix_parts: &[&str]) -> bool {
        let original_parts: Vec<String> = original_filename.split('.').map(String::from).collect();
        parts_are_contiguous_in_original_precomputed(&original_parts, prefix_parts)
    }

    /// Check contiguity using pre-split original parts.
    pub fn parts_are_contiguous_in_original_precomputed(original_parts: &[String], prefix_parts: &[&str]) -> bool {
        if prefix_parts.is_empty() {
            return true;
        }

        let prefix_combined = prefix_parts.join("").to_lowercase();
        parts_are_contiguous_with_combined(original_parts, prefix_parts, &prefix_combined)
    }

    /// Check contiguity with the prefix parts already joined and lowercased into `prefix_combined`,
    /// lowercasing the original parts on the fly before calling [`parts_are_contiguous_lowered`].
    pub fn parts_are_contiguous_with_combined(
        original_parts: &[String],
        prefix_parts: &[&str],
        prefix_combined: &str,
    ) -> bool {
        if prefix_parts.is_empty() || parts_match_a_window(original_parts, prefix_parts) {
            return true;
        }
        let original_parts_lower: Vec<String> = original_parts.iter().map(|part| part.to_lowercase()).collect();
        parts_are_contiguous_lowered(original_parts, &original_parts_lower, prefix_parts, prefix_combined)
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
        assert_eq!(parts.two_parts_lower, [] as [String; 0]);
        assert_eq!(parts.three_parts_lower, [] as [String; 0]);
        assert_eq!(parts.two_parts_original, [] as [String; 0]);
        assert_eq!(parts.three_parts_original, [] as [String; 0]);
    }

    #[test]
    fn two_part_name_has_no_three_part_combinations() {
        let parts = FilteredParts::new("Photo.Lab");
        assert_eq!(parts.two_parts_lower, ["photolab"]);
        assert_eq!(parts.two_parts_original, ["PhotoLab"]);
        assert_eq!(parts.three_parts_lower, [] as [String; 0]);
        assert_eq!(parts.three_parts_original, [] as [String; 0]);
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
        assert_eq!(parts.two_parts_lower, [] as [String; 0]);
        assert_eq!(parts.three_parts_lower, [] as [String; 0]);
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
        // "PhotoLabTV": 'T' after "PhotoLab" is uppercase, valid boundary
        let parts = FilteredParts::new("PhotoLabTV.Image.jpg");
        assert!(parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_rejected_at_lowercase_continuation() {
        // "PhotoLabs": 's' after "PhotoLab" is lowercase, NOT a boundary
        let parts = FilteredParts::new("PhotoLabs.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn starts_with_at_digit_boundary() {
        // "Studio2": '2' after "Studio" is a digit following a letter, valid boundary
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
        // "XPhotoLab" does not start with "photolab": "x" comes first
        let parts = FilteredParts::new("XPhotoLab.Image.jpg");
        assert!(!parts.prefix_matches_normalized("photolab"));
    }

    #[test]
    fn all_uppercase_name_with_word_boundary() {
        // "PHOTOLABPRO": all uppercase, boundary at position 8 sees 'P' (uppercase) → match
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

    #[test]
    fn scandic_single_part_exact_match() {
        let parts = FilteredParts::new("Hälso.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_uppercase_boundary() {
        // "HälsoCenter" starts with "hälso" and 'C' is uppercase → valid boundary
        let parts = FilteredParts::new("HälsoCenter.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_lowercase_rejected() {
        // "Hälsosam" starts with "hälso" but 's' is lowercase → not a boundary
        let parts = FilteredParts::new("Hälsosam.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_scandic_uppercase_boundary() {
        // "HälsoÖversikt" starts with "hälso" and 'Ö' is uppercase → valid boundary
        let parts = FilteredParts::new("HälsoÖversikt.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_starts_with_scandic_lowercase_rejected() {
        // "Hälsoöversikt" starts with "hälso" but 'ö' is lowercase → not a boundary
        let parts = FilteredParts::new("Hälsoöversikt.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_two_part_combined_exact_match() {
        // "Häl.so" combined is "hälso" (lowered). Exact match
        let parts = FilteredParts::new("Häl.so.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_two_part_combined_with_uppercase_boundary() {
        // "Häl.soCenter" combined is "HälsoCenter", starts with "hälso" at uppercase 'C'
        let parts = FilteredParts::new("Häl.soCenter.Video.001.mp4");
        assert!(parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn scandic_two_part_combined_with_lowercase_rejected() {
        // "Häl.sosam" combined is "Hälsosam", starts with "hälso" but 's' is lowercase
        let parts = FilteredParts::new("Häl.sosam.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("hälso"));
    }

    #[test]
    fn umlaut_prefix_not_confused_with_ascii() {
        // "Ställe" should not match "stalle" (different characters)
        let parts = FilteredParts::new("Ställe.Video.001.mp4");
        assert!(!parts.prefix_matches_normalized("stalle"));
    }
}

#[cfg(test)]
mod test_word_boundary {
    use super::*;

    #[test]
    fn exact_match_is_boundary() {
        assert!(FilteredParts::has_word_boundary_at("Intense", 7));
    }

    #[test]
    fn uppercase_after_prefix_is_boundary() {
        // CamelCase: "IntenseAction" → boundary at position 7 (before 'A')
        assert!(FilteredParts::has_word_boundary_at("IntenseAction", 7));
    }

    #[test]
    fn lowercase_after_prefix_is_not_boundary() {
        // "Intensely" → no boundary at position 7 (before 'l')
        assert!(!FilteredParts::has_word_boundary_at("Intensely", 7));
    }

    #[test]
    fn digit_after_letter_is_boundary() {
        assert!(FilteredParts::has_word_boundary_at("Studio2", 6));
    }

    #[test]
    fn letter_after_digit_is_boundary() {
        assert!(FilteredParts::has_word_boundary_at("123Go", 3));
    }

    #[test]
    fn digit_after_digit_is_not_boundary() {
        assert!(!FilteredParts::has_word_boundary_at("12345", 3));
    }

    #[test]
    fn camel_case_multi_word() {
        // "JosephExampleTV" → boundary at position 14 (before 'T')
        assert!(FilteredParts::has_word_boundary_at("JosephExampleTV", 13));
        // "StudioAlphaProductions" → boundary at position 11 (before 'P')
        assert!(FilteredParts::has_word_boundary_at("StudioAlphaProductions", 11));
    }

    #[test]
    fn lowercase_continuation_rejected() {
        // "Intense" → position 6 means before 'e', which is lowercase → not a boundary
        assert!(!FilteredParts::has_word_boundary_at("Intense", 6));
        // "Intensely" → position 6 means before 'e' → not a boundary
        assert!(!FilteredParts::has_word_boundary_at("Intensely", 6));
    }

    #[test]
    fn uppercase_continuation_accepted() {
        // Position 6 in "IntensEly" → before 'E' which is uppercase → boundary
        assert!(FilteredParts::has_word_boundary_at("IntensEly", 6));
    }

    #[test]
    fn beyond_string_length_is_boundary() {
        assert!(FilteredParts::has_word_boundary_at("abc", 3));
        assert!(FilteredParts::has_word_boundary_at("abc", 10));
    }

    #[test]
    fn real_world_photolab_pro() {
        // "PhotoLabPro" at position 8 (len of "photolab") → before 'P' → boundary
        assert!(FilteredParts::has_word_boundary_at("PhotoLabPro", 8));
    }

    #[test]
    fn real_world_photolabs() {
        // "PhotoLabs" at position 8 (len of "photolab") → before 's' → NOT a boundary
        assert!(!FilteredParts::has_word_boundary_at("PhotoLabs", 8));
    }

    #[test]
    fn scandic_uppercase_after_prefix_is_boundary() {
        // "StudioÖversikt" → 'Ö' (uppercase, 2 bytes) after "Studio" at position 6
        assert!(FilteredParts::has_word_boundary_at("StudioÖversikt", 6));
    }

    #[test]
    fn scandic_lowercase_after_prefix_is_not_boundary() {
        // "Studioöversikt" → 'ö' (lowercase, 2 bytes) after "Studio" is not a boundary
        assert!(!FilteredParts::has_word_boundary_at("Studioöversikt", 6));
    }

    #[test]
    fn scandic_uppercase_after_scandic_prefix_is_boundary() {
        // "ÄventyrÖversikt" → boundary at byte 8 (Ä=2 + v=1 + e=1 + n=1 + t=1 + y=1 + r=1)
        // before 'Ö' (uppercase)
        assert!(FilteredParts::has_word_boundary_at("ÄventyrÖversikt", 8));
    }

    #[test]
    fn scandic_lowercase_continuation_not_boundary() {
        // "Ävenyträkning" → boundary check at byte 8 before 'ä' (lowercase)
        assert!(!FilteredParts::has_word_boundary_at("Äventyräkning", 8));
    }

    #[test]
    fn mid_codepoint_position_returns_false() {
        // 'Ö' is bytes [0xC3, 0x96]. Position 7 lands inside the 'Ö' codepoint
        assert!(!FilteredParts::has_word_boundary_at("StudioÖversikt", 7));
    }

    #[test]
    fn digit_after_scandic_letter_is_boundary() {
        // "Ställe2" → digit '2' after 'e' at byte position 7 (S=1 + t=1 + ä=2 + l=1 + l=1 + e=1)
        assert!(FilteredParts::has_word_boundary_at("Ställe2", 7));
    }

    #[test]
    fn scandic_chars_preserve_boundary_logic_in_prefix() {
        // "HälsoCenter" → 'C' (uppercase) after "Hälso" at byte 6 (H=1 + ä=2 + l=1 + s=1 + o=1)
        assert!(FilteredParts::has_word_boundary_at("HälsoCenter", 6));
        // "Hälsosam" → 's' (lowercase) after "Hälso": not a boundary
        assert!(!FilteredParts::has_word_boundary_at("Hälsosam", 6));
    }
}

#[cfg(test)]
mod test_contiguity {
    use super::test_helpers::*;

    // === Basic contiguous matches ===

    #[test]
    fn exact_single_part_match() {
        // Single part prefix always matches if present
        assert!(parts_are_contiguous_in_original("Studio.Alpha.Video.mp4", &["Studio"]));
    }

    #[test]
    fn exact_two_part_match_at_start() {
        assert!(parts_are_contiguous_in_original(
            "Studio.Alpha.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn exact_three_part_match_at_start() {
        assert!(parts_are_contiguous_in_original(
            "Studio.Alpha.Beta.Video.mp4",
            &["Studio", "Alpha", "Beta"]
        ));
    }

    #[test]
    fn exact_two_part_match_in_middle() {
        // Prefix parts can appear anywhere, not just at start
        assert!(parts_are_contiguous_in_original(
            "Prefix.Studio.Alpha.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn exact_two_part_match_at_end() {
        assert!(parts_are_contiguous_in_original(
            "Video.Studio.Alpha.mp4",
            &["Studio", "Alpha"]
        ));
    }

    // === Non-contiguous (should NOT match) ===

    #[test]
    fn two_parts_separated_by_date() {
        // "Studio" and "Alpha" separated by date components
        assert!(!parts_are_contiguous_in_original(
            "Studio.2023.04.13.Alpha.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn two_parts_separated_by_single_number() {
        assert!(!parts_are_contiguous_in_original(
            "Studio.2023.Alpha.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn two_parts_separated_by_word() {
        assert!(!parts_are_contiguous_in_original(
            "Studio.Productions.Alpha.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn three_parts_with_gap_in_middle() {
        // "Studio" and "Beta" are present but "Alpha" is not between them
        assert!(!parts_are_contiguous_in_original(
            "Studio.2023.Alpha.Beta.mp4",
            &["Studio", "Alpha", "Beta"]
        ));
    }

    #[test]
    fn parts_in_wrong_order() {
        assert!(!parts_are_contiguous_in_original(
            "Alpha.Studio.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    // === Concatenated forms (dotted prefix matches concatenated original) ===

    #[test]
    fn dotted_prefix_matches_concatenated_original() {
        // prefix ["Photo", "Lab"] should match original "PhotoLab"
        assert!(parts_are_contiguous_in_original(
            "PhotoLab.Image.01.jpg",
            &["Photo", "Lab"]
        ));
    }

    #[test]
    fn dotted_three_part_prefix_matches_concatenated() {
        assert!(parts_are_contiguous_in_original(
            "StudioAlphaBeta.Video.mp4",
            &["Studio", "Alpha", "Beta"]
        ));
    }

    #[test]
    fn concatenated_prefix_matches_dotted_original() {
        // prefix ["PhotoLab"] should match original "Photo.Lab"
        assert!(parts_are_contiguous_in_original(
            "Photo.Lab.Image.01.jpg",
            &["PhotoLab"]
        ));
    }

    #[test]
    fn concatenated_three_part_prefix_matches_dotted() {
        assert!(parts_are_contiguous_in_original(
            "Studio.Alpha.Beta.Video.mp4",
            &["StudioAlphaBeta"]
        ));
    }

    #[test]
    fn mixed_concatenated_in_middle() {
        // PhotoLab appears in middle of filename
        assert!(parts_are_contiguous_in_original(
            "Prefix.PhotoLab.Image.jpg",
            &["Photo", "Lab"]
        ));
    }

    // === Case insensitivity ===

    #[test]
    fn case_insensitive_exact_match() {
        assert!(parts_are_contiguous_in_original(
            "STUDIO.ALPHA.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn case_insensitive_mixed_case() {
        assert!(parts_are_contiguous_in_original(
            "sTuDiO.aLpHa.Video.mp4",
            &["STUDIO", "ALPHA"]
        ));
    }

    #[test]
    fn case_insensitive_concatenated() {
        assert!(parts_are_contiguous_in_original(
            "PHOTOLAB.Image.jpg",
            &["photo", "lab"]
        ));
    }

    #[test]
    fn case_insensitive_concatenated_reverse() {
        assert!(parts_are_contiguous_in_original("photo.lab.Image.jpg", &["PHOTOLAB"]));
    }

    // === Edge cases ===

    #[test]
    fn empty_prefix_parts() {
        assert!(parts_are_contiguous_in_original("Any.File.Name.mp4", &[]));
    }

    #[test]
    fn single_part_filename() {
        assert!(parts_are_contiguous_in_original("SingleWord.mp4", &["SingleWord"]));
    }

    #[test]
    fn prefix_longer_than_filename() {
        assert!(!parts_are_contiguous_in_original(
            "Short.mp4",
            &["Short", "Medium", "Long"]
        ));
    }

    #[test]
    fn prefix_part_not_in_filename() {
        assert!(!parts_are_contiguous_in_original(
            "Studio.Alpha.Video.mp4",
            &["Studio", "Beta"]
        ));
    }

    #[test]
    fn partial_match_not_sufficient() {
        // "Photo" is there but "Laboratory" is not (only "Lab" is)
        assert!(!parts_are_contiguous_in_original(
            "Photo.Lab.Image.jpg",
            &["Photo", "Laboratory"]
        ));
    }

    // === Long filenames with many parts ===

    #[test]
    fn long_filename_match_at_start() {
        assert!(parts_are_contiguous_in_original(
            "Studio.Alpha.Production.2023.01.15.Episode.01.Title.Here.1080p.x265.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn long_filename_match_deep_in_middle() {
        assert!(parts_are_contiguous_in_original(
            "Site.2023.01.15.Person.Name.Video.Title.1080p.mp4",
            &["Person", "Name"]
        ));
    }

    #[test]
    fn long_filename_non_contiguous() {
        // "Site" at start, "Person" deep in middle - not contiguous
        assert!(!parts_are_contiguous_in_original(
            "Site.2023.01.15.Person.Name.Video.1080p.mp4",
            &["Site", "Person"]
        ));
    }

    // === Numbers and special patterns ===

    #[test]
    fn numeric_parts_are_literal() {
        // Numbers are matched literally as parts
        assert!(parts_are_contiguous_in_original(
            "Show.2023.Episode.mp4",
            &["Show", "2023"]
        ));
    }

    #[test]
    fn resolution_pattern_as_part() {
        assert!(parts_are_contiguous_in_original(
            "Movie.1080p.Version.mp4",
            &["Movie", "1080p"]
        ));
    }

    #[test]
    fn episode_code_as_part() {
        assert!(parts_are_contiguous_in_original(
            "Show.Name.S01E05.mp4",
            &["Show", "Name", "S01E05"]
        ));
    }

    // === Similar but different names ===

    #[test]
    fn extended_prefix_matches() {
        // "PhotoLabPro" starts with "Photo" + "Lab", so it should match
        assert!(parts_are_contiguous_in_original(
            "PhotoLabPro.Image.jpg",
            &["Photo", "Lab"]
        ));
    }

    #[test]
    fn extended_single_part_prefix_matches() {
        // "JosephExampleTV" starts with "JosephExample", so it should match
        assert!(parts_are_contiguous_in_original(
            "JosephExampleTV.filename.mp4",
            &["JosephExample"]
        ));
    }

    #[test]
    fn extended_dotted_prefix_matches_concatenated() {
        // "JosephExampleTV" starts with "JosephExample" (prefix ["Joseph", "Example"])
        assert!(parts_are_contiguous_in_original(
            "JosephExampleTV.Show.mp4",
            &["Joseph", "Example"]
        ));
    }

    #[test]
    fn extended_prefix_all_forms_match() {
        // All these should match prefix ["Joseph", "Example"]:
        // - JosephExample.Video.mp4 (exact dotted)
        // - JosephExampleTV.Show.mp4 (concatenated starts with)
        // - JosephExample.TV.Show.mp4 (exact dotted, TV is separate)
        let prefix = &["Joseph", "Example"];

        assert!(parts_are_contiguous_in_original("JosephExample.Video.mp4", prefix));
        assert!(parts_are_contiguous_in_original("JosephExampleTV.Show.mp4", prefix));
        assert!(parts_are_contiguous_in_original("JosephExample.TV.Show.mp4", prefix));
    }

    #[test]
    fn extended_single_part_prefix_various_extensions() {
        // Single part prefix "JosephExample" should match files where first part starts with it
        let prefix = &["JosephExample"];

        assert!(parts_are_contiguous_in_original("JosephExample.Video.mp4", prefix));
        assert!(parts_are_contiguous_in_original("JosephExampleTV.Show.mp4", prefix));
        assert!(parts_are_contiguous_in_original(
            "JosephExampleProductions.Film.mp4",
            prefix
        ));
    }

    #[test]
    fn extended_prefix_matches_longer_concatenated() {
        // "StudioAlphaProductions" starts with "StudioAlpha", so it SHOULD match prefix ["Studio", "Alpha"].
        // This allows grouping files like:
        // - Studio.Alpha.Video.mp4
        // - StudioAlpha.Film.mp4
        // - StudioAlphaProductions.Movie.mp4
        // all under a "Studio.Alpha" or "StudioAlpha" group
        assert!(parts_are_contiguous_in_original(
            "StudioAlphaProductions.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn exact_concatenated_match_not_substring() {
        // "StudioAlpha" exactly matches "Studio" + "Alpha"
        assert!(parts_are_contiguous_in_original(
            "StudioAlpha.Productions.Video.mp4",
            &["Studio", "Alpha"]
        ));
    }

    // === Multiple possible match positions ===

    #[test]
    fn matches_first_occurrence() {
        // "Alpha.Beta" appears twice, should match the first
        assert!(parts_are_contiguous_in_original(
            "Alpha.Beta.Middle.Alpha.Beta.End.mp4",
            &["Alpha", "Beta"]
        ));
    }

    #[test]
    fn repeated_single_part() {
        assert!(parts_are_contiguous_in_original(
            "Studio.Studio.Studio.mp4",
            &["Studio"]
        ));
    }

    // === Real-world scenarios ===

    #[test]
    fn realistic_adult_site_with_date() {
        // Common pattern: Site.Date.Performer.Title
        // "Site" and "Performer" should NOT match as 2-part prefix
        assert!(!parts_are_contiguous_in_original(
            "ContentSite.2023.04.13.Jane.Doe.Scene.Title.1080p.mp4",
            &["ContentSite", "Jane"]
        ));
    }

    #[test]
    fn realistic_adult_site_without_date() {
        // When performer is right after site, should match
        assert!(parts_are_contiguous_in_original(
            "ContentSite.Jane.Doe.Scene.Title.1080p.mp4",
            &["ContentSite", "Jane"]
        ));
    }

    #[test]
    fn realistic_tv_show_pattern() {
        assert!(parts_are_contiguous_in_original(
            "Breaking.Bad.S01E01.Pilot.1080p.mp4",
            &["Breaking", "Bad"]
        ));
    }

    #[test]
    fn realistic_movie_with_year() {
        // Year between title parts should break contiguity
        assert!(!parts_are_contiguous_in_original(
            "The.Matrix.1999.Remastered.1080p.mp4",
            &["The", "Matrix", "Remastered"]
        ));
    }

    #[test]
    fn realistic_movie_title_contiguous() {
        assert!(parts_are_contiguous_in_original(
            "The.Matrix.Reloaded.2003.1080p.mp4",
            &["The", "Matrix", "Reloaded"]
        ));
    }

    // === Concatenated vs dotted variations ===

    #[test]
    fn three_variations_of_same_name_dotted() {
        let prefix = &["Dark", "Star", "Media"];
        assert!(parts_are_contiguous_in_original("Dark.Star.Media.Video.mp4", prefix));
    }

    #[test]
    fn three_variations_of_same_name_concatenated() {
        let prefix = &["Dark", "Star", "Media"];
        assert!(parts_are_contiguous_in_original("DarkStarMedia.Video.mp4", prefix));
    }

    #[test]
    fn three_variations_of_same_name_partial_concat() {
        // "DarkStar.Media" - first two concatenated, third separate
        let prefix = &["Dark", "Star", "Media"];
        assert!(parts_are_contiguous_in_original("DarkStar.Media.Video.mp4", prefix));
    }

    #[test]
    fn three_variations_reverse_partial_concat() {
        // "Dark.StarMedia" - first separate, last two concatenated
        let prefix = &["Dark", "Star", "Media"];
        assert!(parts_are_contiguous_in_original("Dark.StarMedia.Video.mp4", prefix));
    }

    #[test]
    fn single_concatenated_prefix_matches_three_dotted() {
        assert!(parts_are_contiguous_in_original(
            "Dark.Star.Media.Video.mp4",
            &["DarkStarMedia"]
        ));
    }

    // === Boundary conditions ===

    #[test]
    fn prefix_matches_entire_filename_except_extension() {
        assert!(parts_are_contiguous_in_original(
            "Studio.Alpha.mp4",
            &["Studio", "Alpha"]
        ));
    }

    #[test]
    fn prefix_matches_including_extension_part() {
        // Extension is just another part when splitting by dots
        assert!(parts_are_contiguous_in_original("Studio.Alpha.mp4", &["Alpha", "mp4"]));
    }

    #[test]
    fn very_long_concatenated_name() {
        assert!(parts_are_contiguous_in_original(
            "ThisIsAVeryLongConcatenatedStudioName.Video.mp4",
            &["ThisIsAVeryLongConcatenatedStudioName"]
        ));
    }

    #[test]
    fn very_long_dotted_name_matches_concatenated() {
        assert!(parts_are_contiguous_in_original(
            "This.Is.A.Very.Long.Dotted.Name.Video.mp4",
            &["ThisIsAVeryLongDottedName"]
        ));
    }
}

#[cfg(test)]
/// Tests for prefix candidate finding with realistic filenames.
/// Group names can be found at any position in the filename.
mod test_prefix_candidates {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn single_file_no_match() {
        // Original test: LongName only appears in one file, no group formed
        let files = make_test_files(&["LongName.v1.mp4", "Other.v2.mp4"]);
        let candidates = find_prefix_candidates("LongName.v1.mp4", &files, 2, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);
    }

    #[test]
    fn simple_prefix_multiple_files() {
        let files = make_test_files(&["LongName.v1.mp4", "LongName.v2.mp4", "Other.v2.mp4"]);
        let candidates = find_prefix_candidates("LongName.v1.mp4", &files, 2, 1);
        assert_eq!(candidates, vec![candidate("LongName", 2, 1, 0)]);
    }

    #[test]
    fn prioritizes_longer_prefix() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Thing.v3.mp4",
        ]);
        let candidates = find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 2, 1);
        // With position-agnostic matching, we find candidates from all positions
        // Core expected candidates should be present
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Some.Name.Thing" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Some.Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Some" && c.match_count == 3));
        // Position-agnostic matching also finds Name.Thing, Name, Thing
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Name.Thing" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Thing" && c.match_count == 3));
    }

    #[test]
    fn mixed_prefixes_different_third_parts() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        let candidates = find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 2, 1);
        // Core expected candidates
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Some.Name.Thing" && c.match_count == 2)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Some.Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Some" && c.match_count == 3));
    }

    #[test]
    fn fallback_to_two_part_when_no_three_part_matches() {
        let files = make_test_files(&["Some.Name.Thing.mp4", "Some.Name.Other.mp4", "Some.Name.More.mp4"]);
        let candidates = find_prefix_candidates("Some.Name.Thing.mp4", &files, 2, 1);
        // Core expected candidates
        assert!(candidates.iter().any(|c| c.prefix == "Some.Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Some" && c.match_count == 3));
    }

    #[test]
    fn single_word_fallback() {
        let files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);
        let candidates = find_prefix_candidates("ABC.2023.Thing.mp4", &files, 3, 1);
        assert!(candidates.iter().any(|c| c.prefix == "ABC" && c.match_count == 3));
    }

    #[test]
    fn respects_min_group_size() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        // With min_group_size=3: only prefixes with 3+ files qualify 3-part "Some.Name.Thing" has 2 files < 3,
        // so excluded 2-part "Some.Name" has 3 files >= 3, 1-part "Some" has 3 files >= 3
        let candidates = find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 3, 1);
        assert!(candidates.iter().any(|c| c.prefix == "Some.Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Some" && c.match_count == 3));
        // Thing only appears in 2 files, below threshold
        assert!(
            !candidates
                .iter()
                .any(|c| c.prefix == "Some.Name.Thing" && c.match_count >= 3)
        );
    }

    #[test]
    fn no_matches_below_threshold() {
        let files = make_test_files(&["ABC.random.mp4", "XYZ.other.mp4"]);
        let candidates = find_prefix_candidates("ABC.random.mp4", &files, 2, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);
    }

    #[test]
    fn returns_all_viable_options_for_alternatives() {
        let files = make_test_files(&[
            "Show.Name.S01E01.mp4",
            "Show.Name.S01E02.mp4",
            "Show.Name.S01E03.mp4",
            "Show.Other.S01E01.mp4",
            "Show.Other.S01E02.mp4",
        ]);
        let candidates = find_prefix_candidates("Show.Name.S01E01.mp4", &files, 2, 1);
        // Core expected candidates
        assert!(candidates.iter().any(|c| c.prefix == "Show.Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Show" && c.match_count == 5));
    }

    #[test]
    fn empty_file_list() {
        let files: Vec<FileInfo> = Vec::new();
        let candidates = find_prefix_candidates("Some.Name.mp4", &files, 2, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);
    }

    #[test]
    fn file_not_in_list() {
        let files = make_test_files(&["Other.Name.mp4", "Different.File.mp4"]);
        let candidates = find_prefix_candidates("Some.Name.mp4", &files, 2, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);
    }

    #[test]
    fn min_group_size_one() {
        let files = make_test_files(&["Unique.Name.v1.mp4", "Other.v2.mp4"]);
        // With min_group_size=1, threshold is min(1, 2) = 1
        // All prefixes with at least 1 match qualify
        let candidates = find_prefix_candidates("Unique.Name.v1.mp4", &files, 1, 1);
        // Core expected candidates from prefix position
        assert!(candidates.iter().any(|c| c.prefix == "Unique.Name.v1"));
        assert!(candidates.iter().any(|c| c.prefix == "Unique.Name"));
        assert!(candidates.iter().any(|c| c.prefix == "Unique"));
        // Position-agnostic matching also finds Name
        assert!(candidates.iter().any(|c| c.prefix == "Name"));
    }

    #[test]
    fn many_files_same_prefix() {
        let files = make_test_files(&[
            "Series.Episode.01.mp4",
            "Series.Episode.02.mp4",
            "Series.Episode.03.mp4",
            "Series.Episode.04.mp4",
            "Series.Episode.05.mp4",
            "Series.Episode.06.mp4",
            "Series.Episode.07.mp4",
            "Series.Episode.08.mp4",
            "Series.Episode.09.mp4",
            "Series.Episode.10.mp4",
        ]);
        let candidates = find_prefix_candidates("Series.Episode.01.mp4", &files, 5, 1);
        // Core expected candidates
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Series.Episode" && c.match_count == 10)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Series" && c.match_count == 10));
        // Position-agnostic matching also finds Episode
        assert!(candidates.iter().any(|c| c.prefix == "Episode" && c.match_count == 10));
    }

    #[test]
    fn case_insensitive_prefix_matching() {
        let files = make_test_files(&["Show.Name.v1.mp4", "show.name.v2.mp4", "SHOW.NAME.v3.mp4"]);
        let candidates = find_prefix_candidates("Show.Name.v1.mp4", &files, 2, 1);
        // Case-insensitive matching should group all three files
        // With position-agnostic matching, "Name" is also found as a candidate
        assert!(candidates.iter().any(|c| c.prefix == "Show.Name" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Show" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Name" && c.match_count == 3));
    }

    #[test]
    fn dot_separated_matches_concatenated() {
        // "Photo.Lab" and "PhotoLab" should be treated as equivalent
        let files = make_test_files(&[
            "Photo.Lab.Image.One.jpg",
            "PhotoLab.Image.Two.jpg",
            "PhotoLab.Image.Three.jpg",
            "Photolab.Image.Four.jpg",
        ]);
        let candidates = find_prefix_candidates("PhotoLab.Image.Two.jpg", &files, 2, 1);
        // All files should match - PhotoLab = Photo.Lab = Photolab
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        // The 1-part prefix "PhotoLab" should match all 4 files
        let photolab = candidates.iter().find(|c| c.prefix.to_lowercase() == "photolab");
        assert!(photolab.is_some());
        assert_eq!(photolab.unwrap().match_count, 4);

        // Verify with dotted form as source
        let files = make_test_files(&[
            "Studio.TV.First.Episode.mp4",
            "StudioTV.Second.Episode.mp4",
            "STUDIOTV.Third.Episode.mp4",
            "Studiotv.Fourth.Episode.mp4",
        ]);
        let candidates = find_prefix_candidates("StudioTV.Second.Episode.mp4", &files, 2, 1);
        // All files should match on the single-part prefix (StudioTV = Studio.TV = Studiotv)
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        let studiotv = candidates.iter().find(|c| c.prefix.to_lowercase() == "studiotv");
        assert!(studiotv.is_some());
        assert_eq!(studiotv.unwrap().match_count, 4);
    }

    #[test]
    fn dot_separated_three_parts_matches_concatenated() {
        // "Sun.Set.HD" and "SunSetHD" should be treated as equivalent
        let files = make_test_files(&[
            "Sun.Set.HD.Image.One.jpg",
            "SunSetHD.Image.Two.jpg",
            "Sunsethd.Image.Three.jpg",
        ]);
        let candidates = find_prefix_candidates("SunSetHD.Image.Two.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        // The 1-part prefix "SunSetHD" should match all 3 files
        let sunsethd = candidates.iter().find(|c| c.prefix.to_lowercase() == "sunsethd");
        assert!(sunsethd.is_some());
        assert_eq!(sunsethd.unwrap().match_count, 3);

        // "Show.T.V" and "ShowTV" should be treated as equivalent
        let files = make_test_files(&[
            "Show.T.V.First.Episode.mp4",
            "ShowTV.Second.Episode.mp4",
            "Showtv.Third.Episode.mp4",
        ]);
        let candidates = find_prefix_candidates("ShowTV.Second.Episode.mp4", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        let showtv = candidates.iter().find(|c| c.prefix.to_lowercase() == "showtv");
        assert!(showtv.is_some());
        assert_eq!(showtv.unwrap().match_count, 3);
    }

    #[test]
    fn normalize_name_removes_dots_spaces_and_lowercases() {
        assert_eq!(normalize_name("PhotoLab"), "photolab");
        assert_eq!(normalize_name("Photo.Lab"), "photolab");
        assert_eq!(normalize_name("photo.lab"), "photolab");
        assert_eq!(normalize_name("Album.Name.Here"), "albumnamehere");
        assert_eq!(normalize_name("StudioTV"), "studiotv");
        assert_eq!(normalize_name("Studio.TV"), "studiotv");
        assert_eq!(normalize_name("studio.tv"), "studiotv");
        assert_eq!(normalize_name("Show.Name.Here"), "shownamehere");
        assert_eq!(normalize_name("Photo Lab"), "photolab");
        assert_eq!(normalize_name("Album Name Here"), "albumnamehere");
        assert_eq!(normalize_name("Studio TV"), "studiotv");
        assert_eq!(normalize_name("  Photo Lab  "), "photolab");
    }

    #[test]
    fn prefix_matches_normalized_single_part() {
        assert!(FilteredParts::new("PhotoLab.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("PHOTOLAB.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("ShowTV.Episode.mp4").prefix_matches_normalized("showtv"));
        assert!(FilteredParts::new("SHOWTV.Episode.mp4").prefix_matches_normalized("showtv"));
    }

    #[test]
    fn prefix_matches_normalized_two_parts() {
        assert!(FilteredParts::new("Photo.Lab.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("photo.lab.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("Show.TV.Episode.mp4").prefix_matches_normalized("showtv"));
        assert!(FilteredParts::new("show.tv.Episode.mp4").prefix_matches_normalized("showtv"));
    }

    #[test]
    fn prefix_matches_normalized_three_parts() {
        assert!(FilteredParts::new("Ph.oto.Lab.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("Sh.ow.TV.Episode.mp4").prefix_matches_normalized("showtv"));
    }

    #[test]
    fn prefix_matches_normalized_no_match() {
        // These don't match because no part starts with the prefix
        assert!(!FilteredParts::new("Other.Album.jpg").prefix_matches_normalized("photolab"));
        assert!(!FilteredParts::new("Other.Show.mp4").prefix_matches_normalized("showtv"));
        // These don't match because "XPhotoLab" doesn't start with "photolab"
        assert!(!FilteredParts::new("XPhotoLab.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(!FilteredParts::new("XShowTV.Episode.mp4").prefix_matches_normalized("showtv"));
    }

    #[test]
    fn prefix_matches_normalized_starts_with() {
        // These match because a part STARTS WITH the prefix at a word boundary
        // This allows grouping PhotoLabProductions with PhotoLab files
        assert!(FilteredParts::new("PhotoLabX.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("ShowTVX.Episode.mp4").prefix_matches_normalized("showtv"));
        assert!(FilteredParts::new("PhotoLabProductions.Image.jpg").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("ShowTVNetwork.Episode.mp4").prefix_matches_normalized("showtv"));
    }

    #[test]
    fn min_group_size_filters_small_groups() {
        let files = make_test_files(&[
            "Vacation.Photos.Image1.jpg",
            "Vacation.Photos.Image2.jpg",
            "Other.Album.Image1.jpg",
        ]);
        // With min_group_size=3, only prefixes with 3+ files qualify
        // "Vacation.Photos" has 2 files < 3, so excluded
        let candidates = find_prefix_candidates("Vacation.Photos.Image1.jpg", &files, 3, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);

        // With min_group_size=2, "Vacation.Photos" qualifies
        let candidates = find_prefix_candidates("Vacation.Photos.Image1.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Vacation.Photos" && c.match_count == 2)
        );
    }

    #[test]
    fn min_group_size_at_exact_threshold() {
        let files = make_test_files(&[
            "Beach.Summer.Photo1.jpg",
            "Beach.Summer.Photo2.jpg",
            "Beach.Summer.Photo3.jpg",
        ]);
        // With min_group_size=3, prefixes with exactly 3 files qualify
        let candidates = find_prefix_candidates("Beach.Summer.Photo1.jpg", &files, 3, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        assert!(candidates.iter().any(|c| c.match_count == 3));

        // min_group_size=4, "Beach.Summer" with 3 files < 4, so excluded
        let candidates = find_prefix_candidates("Beach.Summer.Photo1.jpg", &files, 4, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);
    }

    #[test]
    fn mixed_case_variations_all_group_together() {
        let files = make_test_files(&[
            "MyAlbum.Photo.One.jpg",
            "MYALBUM.Photo.Two.jpg",
            "myalbum.Photo.Three.jpg",
            "Myalbum.Photo.Four.jpg",
            "myAlbum.Photo.Five.jpg",
        ]);
        let candidates = find_prefix_candidates("MyAlbum.Photo.One.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        // All 5 should be grouped together regardless of case
        let myalbum = candidates.iter().find(|c| c.prefix.to_lowercase() == "myalbum");
        assert!(myalbum.is_some());
        assert_eq!(myalbum.unwrap().match_count, 5);
    }

    #[test]
    fn dot_separated_with_mixed_case() {
        // Combining both dot-separation and case variations
        let files = make_test_files(&[
            "My.Album.Photo.One.jpg",
            "MyAlbum.Photo.Two.jpg",
            "MYALBUM.Photo.Three.jpg",
            "my.album.Photo.Four.jpg",
            "MY.ALBUM.Photo.Five.jpg",
        ]);
        let candidates = find_prefix_candidates("MyAlbum.Photo.One.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        // All 5 should be grouped together regardless of case
        let myalbum = candidates.iter().find(|c| c.prefix.to_lowercase() == "myalbum");
        assert!(myalbum.is_some());
        assert_eq!(myalbum.unwrap().match_count, 5);
    }

    #[test]
    fn dot_separated_two_parts_match_single_word() {
        // Two dot-separated parts should match single concatenated word
        let files = make_test_files(&[
            "Photo.Lab.Image1.jpg",
            "PhotoLab.Image2.jpg",
            "Photolab.Image3.jpg",
            "PHOTOLAB.Image4.jpg",
            "Photo.LAB.Image5.jpg",
        ]);
        let candidates = find_prefix_candidates("PhotoLab.Image2.jpg", &files, 3, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        // All 5 should be grouped together
        let photolab = candidates.iter().find(|c| c.prefix.to_lowercase() == "photolab");
        assert!(photolab.is_some());
        assert_eq!(photolab.unwrap().match_count, 5);
    }

    #[test]
    fn three_part_dot_separated_match_single_word() {
        let files = make_test_files(&[
            "Sun.Set.HD.Image1.jpg",
            "SunSetHD.Image2.jpg",
            "Sunsethd.Image3.jpg",
            "SUN.SET.HD.Image4.jpg",
        ]);
        let candidates = find_prefix_candidates("SunSetHD.Image2.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        let sunsethd = candidates.iter().find(|c| c.prefix.to_lowercase() == "sunsethd");
        assert!(sunsethd.is_some());
        assert_eq!(sunsethd.unwrap().match_count, 4);
    }

    #[test]
    fn prefers_longer_prefix_over_short_with_more_matches() {
        let files = make_test_files(&[
            "Album.Name.Set1.Photo1.jpg",
            "Album.Name.Set1.Photo2.jpg",
            "Album.Name.Set2.Photo1.jpg",
            "Album.Other.Set1.Photo1.jpg",
        ]);
        let candidates = find_prefix_candidates("Album.Name.Set1.Photo1.jpg", &files, 2, 1);
        // Should find longer prefixes first: 3-part, then 2-part, then 1-part
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Album.Name.Set1" && c.match_count == 2)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Album.Name" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Album" && c.match_count == 4));
    }

    #[test]
    fn min_group_size_one_matches_single_file() {
        let files = make_test_files(&["Unique.Name.File.jpg", "Other.File.jpg"]);
        // With min_group_size=1, all prefixes with at least 1 match qualify
        let candidates = find_prefix_candidates("Unique.Name.File.jpg", &files, 1, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        assert!(candidates.iter().any(|c| c.prefix == "Unique.Name.File"));
        assert!(candidates.iter().any(|c| c.prefix == "Unique.Name"));
        assert!(candidates.iter().any(|c| c.prefix == "Unique"));
        // Position-agnostic: File appears in both files
        assert!(candidates.iter().any(|c| c.prefix == "File" && c.match_count == 2));
    }

    #[test]
    fn high_min_group_size_filters_all() {
        let files = make_test_files(&[
            "Gallery.Photos.Img1.jpg",
            "Gallery.Photos.Img2.jpg",
            "Gallery.Photos.Img3.jpg",
            "Gallery.Photos.Img4.jpg",
            "Gallery.Photos.Img5.jpg",
        ]);
        // min_group_size=10, all prefixes with 5 files < 10, so none qualify
        let candidates = find_prefix_candidates("Gallery.Photos.Img1.jpg", &files, 10, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);

        // min_group_size=5, prefixes with exactly 5 files qualify
        let candidates = find_prefix_candidates("Gallery.Photos.Img1.jpg", &files, 5, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Gallery.Photos" && c.match_count == 5)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Gallery" && c.match_count == 5));
    }

    #[test]
    fn identical_prefix_files_are_grouped() {
        // Files with exact same prefix should always be grouped
        let files = make_test_files(&[
            "Wedding.Photos.IMG001.jpg",
            "Wedding.Photos.IMG002.jpg",
            "Wedding.Photos.IMG003.jpg",
            "Wedding.Photos.IMG004.jpg",
        ]);
        let candidates = find_prefix_candidates("Wedding.Photos.IMG001.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        // Should find 2-part prefix with all 4 files
        let two_part = candidates.iter().find(|c| c.prefix == "Wedding.Photos");
        assert!(two_part.is_some());
        assert_eq!(two_part.unwrap().match_count, 4);
    }

    #[test]
    fn identical_single_word_prefix_grouped() {
        let files = make_test_files(&["Concert.Image1.jpg", "Concert.Image2.jpg", "Concert.Image3.jpg"]);
        let candidates = find_prefix_candidates("Concert.Image1.jpg", &files, 2, 1);
        assert_ne!(candidates, [] as [PrefixCandidate<'_>; 0]);
        let one_part = candidates.iter().find(|c| c.prefix == "Concert");
        assert!(one_part.is_some());
        assert_eq!(one_part.unwrap().match_count, 3);
    }

    #[test]
    fn tv_show_season_episodes() {
        let files = make_test_files(&[
            "Drama.Series.S01E01.mp4",
            "Drama.Series.S01E02.mp4",
            "Drama.Series.S01E03.mp4",
            "Drama.Series.S02E01.mp4",
            "Drama.Series.S02E02.mp4",
        ]);
        let candidates = find_prefix_candidates("Drama.Series.S01E01.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Drama.Series" && c.match_count == 5)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Drama" && c.match_count == 5));
        // Position-agnostic also finds Series
        assert!(candidates.iter().any(|c| c.prefix == "Series" && c.match_count == 5));
    }

    #[test]
    fn movie_series_with_years() {
        let files = make_test_files(&[
            "Studio.Action.2012.BluRay.mp4",
            "Studio.Action.2015.BluRay.mp4",
            "Studio.Comedy.2014.BluRay.mp4",
            "Studio.Comedy.2017.BluRay.mp4",
        ]);
        let candidates = find_prefix_candidates("Studio.Action.2012.BluRay.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Studio.Action" && c.match_count == 2)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Studio" && c.match_count == 4));
        // Position-agnostic also finds BluRay
        assert!(candidates.iter().any(|c| c.prefix == "BluRay" && c.match_count == 4));
    }

    #[test]
    fn long_name_with_year_after_prefix() {
        let files = make_test_files(&[
            "Drama.Series.Name.2020.S01E01.Pilot.1080p.mp4",
            "Drama.Series.Name.2020.S01E02.Awakening.1080p.mp4",
            "Drama.Series.Name.2020.S01E03.Revelation.1080p.mp4",
            "Drama.Series.Name.2021.S02E01.Return.1080p.mp4",
        ]);
        let candidates = find_prefix_candidates("Drama.Series.Name.2020.S01E01.Pilot.1080p.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Drama.Series.Name" && c.match_count == 4)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Drama.Series" && c.match_count == 4)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Drama" && c.match_count == 4));
    }

    #[test]
    fn long_name_with_only_year_after_prefix() {
        let files = make_test_files(&[
            "Action.Movie.Title.2019.Directors.Cut.mp4",
            "Action.Movie.Title.2020.Extended.Edition.mp4",
            "Action.Movie.Title.2021.Remastered.mp4",
        ]);
        let candidates = find_prefix_candidates("Action.Movie.Title.2019.Directors.Cut.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Action.Movie.Title" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Action.Movie" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Action" && c.match_count == 3));
    }

    #[test]
    fn long_name_with_date_after_prefix() {
        let files = make_test_files(&[
            "Daily.News.Show.2024.01.15.Morning.Report.mp4",
            "Daily.News.Show.2024.01.16.Evening.Edition.mp4",
            "Daily.News.Show.2024.01.17.Special.Coverage.mp4",
        ]);
        let candidates = find_prefix_candidates("Daily.News.Show.2024.01.15.Morning.Report.mp4", &files, 3, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Daily.News.Show" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Daily.News" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Daily" && c.match_count == 3));
    }

    #[test]
    fn long_name_franchise_with_year_variations() {
        let files = make_test_files(&[
            "Epic.Adventure.Saga.Part.One.2018.BluRay.Remux.mp4",
            "Epic.Adventure.Saga.Part.Two.2020.BluRay.Remux.mp4",
            "Epic.Adventure.Saga.Part.Three.2022.BluRay.Remux.mp4",
            "Epic.Adventure.Origins.Prequel.2015.BluRay.mp4",
        ]);
        let candidates = find_prefix_candidates("Epic.Adventure.Saga.Part.One.2018.BluRay.Remux.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Epic.Adventure.Saga" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Epic.Adventure" && c.match_count == 4)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Epic" && c.match_count == 4));
    }

    #[test]
    fn long_name_season_with_year_in_name() {
        let files = make_test_files(&[
            "Anthology.Series.Collection.S01E01.Genesis.1080p.WEB.mp4",
            "Anthology.Series.Collection.S01E02.Exodus.1080p.WEB.mp4",
            "Anthology.Series.Collection.S02E01.Revival.1080p.WEB.mp4",
            "Anthology.Series.Collection.S02E02.Finale.1080p.WEB.mp4",
        ]);
        let candidates =
            find_prefix_candidates("Anthology.Series.Collection.S01E01.Genesis.1080p.WEB.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Anthology.Series.Collection" && c.match_count == 4)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Anthology.Series" && c.match_count == 4)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Anthology" && c.match_count == 4));
    }

    #[test]
    fn long_name_documentary_with_regions() {
        let files = make_test_files(&[
            "Nature.Wildlife.Documentary.Africa.Savanna.2019.4K.mp4",
            "Nature.Wildlife.Documentary.Asia.Jungle.2020.4K.mp4",
            "Nature.Wildlife.Documentary.Europe.Alps.2021.4K.mp4",
        ]);
        let candidates = find_prefix_candidates("Nature.Wildlife.Documentary.Africa.Savanna.2019.4K.mp4", &files, 3, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Nature.Wildlife.Documentary" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Nature.Wildlife" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Nature" && c.match_count == 3));
    }

    #[test]
    fn long_name_with_version_and_year() {
        let files = make_test_files(&[
            "Software.Tutorial.Guide.v2.2023.Intro.Basics.mp4",
            "Software.Tutorial.Guide.v2.2023.Advanced.Topics.mp4",
            "Software.Tutorial.Guide.v2.2023.Expert.Masterclass.mp4",
        ]);
        let candidates = find_prefix_candidates("Software.Tutorial.Guide.v2.2023.Intro.Basics.mp4", &files, 3, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Software.Tutorial.Guide" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Software.Tutorial" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Software" && c.match_count == 3));
    }

    #[test]
    fn short_names_with_extensions() {
        let files = make_test_files(&["A.B.mp4", "A.C.mp4", "A.D.mp4"]);
        let candidates = find_prefix_candidates("A.B.mp4", &files, 2, 1);
        let a_candidate = candidates.iter().find(|c| c.prefix.to_lowercase() == "a");
        assert!(a_candidate.is_some());
        assert_eq!(a_candidate.unwrap().match_count, 3);
    }

    #[test]
    fn with_filtered_numeric_parts() {
        let files = make_test_files(&["Show.Name.S01.mp4", "Show.Name.S02.mp4", "Show.Name.S03.mp4"]);
        let candidates = find_prefix_candidates("Show.Name.S01.mp4", &files, 2, 1);
        let show_name = candidates.iter().find(|c| c.prefix.to_lowercase() == "show.name");
        assert!(show_name.is_some());
        assert_eq!(show_name.unwrap().match_count, 3);
    }

    #[test]
    fn numeric_filtering_groups_correctly() {
        let filtered_files = make_test_files(&["ABC.Thing.v1.mp4", "ABC.Thing.v2.mp4", "ABC.Thing.v3.mp4"]);
        let candidates = find_prefix_candidates("ABC.Thing.v1.mp4", &filtered_files, 3, 1);
        // With position-agnostic matching, "Thing" is also found as a candidate
        assert!(candidates.iter().any(|c| c.prefix == "ABC.Thing" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "ABC" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Thing" && c.match_count == 3));
    }

    #[test]
    fn mixed_years_without_filtering() {
        let unfiltered_files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);
        let candidates = find_prefix_candidates("ABC.2023.Thing.mp4", &unfiltered_files, 3, 1);
        assert_eq!(candidates, vec![candidate("ABC", 3, 1, 0)]);
    }

    #[test]
    fn tv_show_generic_scenario() {
        let filtered_files = make_test_files(&[
            "Series.Name.S01E01.1080p.mp4",
            "Series.Name.S01E02.1080p.mp4",
            "Series.Name.S01E03.1080p.mp4",
        ]);
        let candidates = find_prefix_candidates("Series.Name.S01E01.1080p.mp4", &filtered_files, 3, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Series.Name" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Series" && c.match_count == 3));
        // Position-agnostic also finds Name
        assert!(candidates.iter().any(|c| c.prefix == "Name" && c.match_count == 3));
    }

    #[test]
    fn long_name_mixed_year_positions() {
        let files = make_test_files(&[
            "Studio.Franchise.Title.Original.2020.Remastered.2023.HDR.mp4",
            "Studio.Franchise.Title.Original.2020.Remastered.2024.HDR.mp4",
            "Studio.Franchise.Other.Sequel.2021.Remastered.2023.HDR.mp4",
        ]);
        let candidates = find_prefix_candidates(
            "Studio.Franchise.Title.Original.2020.Remastered.2023.HDR.mp4",
            &files,
            2,
            1,
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Studio.Franchise.Title" && c.match_count == 2)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Studio.Franchise" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Studio" && c.match_count == 3));
    }

    #[test]
    fn long_name_decade_in_title() {
        let files = make_test_files(&[
            "Retro.Eighties.Collection.Vol1.Greatest.Hits.mp4",
            "Retro.Eighties.Collection.Vol2.Classic.Cuts.mp4",
            "Retro.Eighties.Collection.Vol3.Deep.Tracks.mp4",
        ]);
        let candidates = find_prefix_candidates("Retro.Eighties.Collection.Vol1.Greatest.Hits.mp4", &files, 3, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Retro.Eighties.Collection" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Retro.Eighties" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Retro" && c.match_count == 3));
    }

    #[test]
    fn three_part_prefix_with_mixed_fourth_parts() {
        let files = make_test_files(&[
            "Alpha.Beta.Gamma.One.mp4",
            "Alpha.Beta.Gamma.Two.mp4",
            "Alpha.Beta.Gamma.Three.mp4",
            "Alpha.Beta.Delta.One.mp4",
        ]);
        let candidates = find_prefix_candidates("Alpha.Beta.Gamma.One.mp4", &files, 2, 1);
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Alpha.Beta.Gamma" && c.match_count == 3)
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Alpha.Beta" && c.match_count == 4)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Alpha" && c.match_count == 4));
    }

    #[test]
    fn only_two_files_high_threshold() {
        let files = make_test_files(&["Show.Name.v1.mp4", "Show.Name.v2.mp4"]);
        // With min_group_size=5, "Show.Name" has 2 files < 5, so excluded
        let candidates = find_prefix_candidates("Show.Name.v1.mp4", &files, 5, 1);
        assert_eq!(candidates, [] as [PrefixCandidate<'_>; 0]);

        // With min_group_size=2, "Show.Name" qualifies
        // With position-agnostic matching, "Name" is also found
        let candidates = find_prefix_candidates("Show.Name.v1.mp4", &files, 2, 1);
        assert!(candidates.iter().any(|c| c.prefix == "Show.Name" && c.match_count == 2));
        assert!(candidates.iter().any(|c| c.prefix == "Show" && c.match_count == 2));
        assert!(candidates.iter().any(|c| c.prefix == "Name" && c.match_count == 2));
    }
}

#[cfg(test)]
mod test_get_all_n_part_sequences {
    use super::*;

    #[test]
    fn single_part_sequences() {
        let result = get_all_n_part_sequences("A.B.C.D", 1);
        assert_eq!(result, vec!["A", "B", "C", "D"]);
    }

    #[test]
    fn two_part_sequences() {
        let result = get_all_n_part_sequences("A.B.C.D", 2);
        assert_eq!(result, vec!["A.B", "B.C", "C.D"]);
    }

    #[test]
    fn three_part_sequences() {
        let result = get_all_n_part_sequences("A.B.C.D", 3);
        assert_eq!(result, vec!["A.B.C", "B.C.D"]);
    }

    #[test]
    fn four_part_sequences_exact_length() {
        let result = get_all_n_part_sequences("A.B.C.D", 4);
        assert_eq!(result, vec!["A.B.C.D"]);
    }

    #[test]
    fn too_few_parts() {
        let result = get_all_n_part_sequences("A.B", 3);
        assert_eq!(result, [] as [&str; 0]);
    }

    #[test]
    fn zero_parts_returns_empty() {
        let result = get_all_n_part_sequences("A.B.C", 0);
        assert_eq!(result, [] as [&str; 0]);
    }

    #[test]
    fn no_dots_single_part() {
        let result = get_all_n_part_sequences("SingleWord", 1);
        assert_eq!(result, vec!["SingleWord"]);
    }

    #[test]
    fn no_dots_multi_part_returns_empty() {
        let result = get_all_n_part_sequences("SingleWord", 2);
        assert_eq!(result, [] as [&str; 0]);
    }

    #[test]
    fn realistic_filename() {
        let result = get_all_n_part_sequences("Studio.Name.Video.Title.2024.mp4", 2);
        assert_eq!(
            result,
            vec!["Studio.Name", "Name.Video", "Video.Title", "Title.2024", "2024.mp4"]
        );
    }

    #[test]
    fn realistic_filename_three_parts() {
        let result = get_all_n_part_sequences("Studio.Name.Video.Title.mp4", 3);
        assert_eq!(result, vec!["Studio.Name.Video", "Name.Video.Title", "Video.Title.mp4"]);
    }
}

#[cfg(test)]
mod test_position_agnostic_matching {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn group_name_in_middle_of_filename() {
        // CommonGroup appears in the middle of filenames, not at the start
        let files = make_test_files(&[
            "Something.CommonGroup.file.1.txt",
            "New.CommonGroup.another.file.2.txt",
            "CommonGroup.simply.as.prefix.file.3.txt",
        ]);
        let candidates = find_prefix_candidates("Something.CommonGroup.file.1.txt", &files, 3, 5);
        // Should find CommonGroup as a candidate since it appears in all 3 files
        assert!(
            candidates.iter().any(|c| c.prefix == "CommonGroup"),
            "Expected to find CommonGroup candidate, got: {candidates:?}"
        );
    }

    #[test]
    fn group_name_at_various_positions() {
        let files = make_test_files(&[
            "Something.CommonGroup.file.1.txt",
            "New.CommonGroup.another.file.2.txt",
            "CommonGroup.simply.as.prefix.file.3.txt",
            "Extra.stuff.at.start.CommonGroup.file.4.txt",
        ]);
        let candidates = find_prefix_candidates("Something.CommonGroup.file.1.txt", &files, 3, 5);
        assert!(
            candidates.iter().any(|c| c.prefix == "CommonGroup"),
            "Expected to find CommonGroup candidate, got: {candidates:?}"
        );
    }

    #[test]
    fn dotted_group_name_in_middle() {
        // Common.GroupName as a 2-part sequence appearing in the middle
        let files = make_test_files(&[
            "Common.GroupName.simply.as.prefix.file.5.txt",
            "Content.Common.GroupName.even.more.versions.file.6.txt",
            "Another.Common.GroupName.variation.file.7.txt",
        ]);
        let candidates = find_prefix_candidates("Content.Common.GroupName.even.more.versions.file.6.txt", &files, 3, 5);
        // Should find Common.GroupName as a 2-part candidate
        assert!(
            candidates.iter().any(|c| c.prefix == "Common.GroupName"),
            "Expected to find Common.GroupName candidate, got: {candidates:?}"
        );
    }

    #[test]
    fn mixed_prefix_and_middle_positions() {
        // Mix of files where the group name is at the start for some and middle for others
        let files = make_test_files(&[
            "StudioName.video.one.mp4",
            "StudioName.video.two.mp4",
            "Extra.StudioName.video.three.mp4",
            "More.Extra.StudioName.video.four.mp4",
        ]);
        let candidates = find_prefix_candidates("Extra.StudioName.video.three.mp4", &files, 3, 5);
        // StudioName should be found since it appears in all 4 files (in various positions)
        assert!(
            candidates.iter().any(|c| c.prefix == "StudioName"),
            "Expected to find StudioName candidate, got: {candidates:?}"
        );
    }

    #[test]
    fn concatenated_and_dotted_at_various_positions() {
        // Mix of concatenated and dotted forms at various positions
        let files = make_test_files(&[
            "PhotoLab.Image.One.jpg",
            "Extra.Photo.Lab.Image.Two.jpg",
            "More.PhotoLab.Image.Three.jpg",
        ]);
        let candidates = find_prefix_candidates("PhotoLab.Image.One.jpg", &files, 3, 5);
        // PhotoLab should match all (via concatenation normalization)
        let photolab_candidate = candidates.iter().find(|c| normalize_name(&c.prefix) == "photolab");
        assert!(
            photolab_candidate.is_some(),
            "Expected to find PhotoLab-related candidate, got: {candidates:?}"
        );
    }

    #[test]
    fn prefix_matches_normalized_finds_anywhere() {
        // Test that prefix_matches_normalized checks all positions
        assert!(FilteredParts::new("Extra.StudioName.video.mp4").prefix_matches_normalized("studioname"));
        assert!(FilteredParts::new("More.Extra.StudioName.mp4").prefix_matches_normalized("studioname"));
        assert!(FilteredParts::new("StudioName.video.mp4").prefix_matches_normalized("studioname"));
    }

    #[test]
    fn prefix_matches_normalized_two_part_anywhere() {
        // Two-part prefix matching anywhere
        assert!(FilteredParts::new("Extra.Photo.Lab.video.mp4").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("Photo.Lab.video.mp4").prefix_matches_normalized("photolab"));
        assert!(FilteredParts::new("More.Photo.Lab.mp4").prefix_matches_normalized("photolab"));
    }

    #[test]
    fn prefix_matches_normalized_three_part_anywhere() {
        // Three-part prefix matching anywhere
        assert!(FilteredParts::new("Extra.Alpha.Beta.Gamma.video.mp4").prefix_matches_normalized("alphabetagamma"));
        assert!(FilteredParts::new("Alpha.Beta.Gamma.video.mp4").prefix_matches_normalized("alphabetagamma"));
    }

    #[test]
    fn group_name_not_found_when_not_contiguous() {
        // GroupName parts separated by other content should not match
        let files = make_test_files(&[
            "Common.2024.GroupName.file.1.txt",
            "Common.2023.GroupName.file.2.txt",
            "Common.2022.GroupName.file.3.txt",
        ]);
        let candidates = find_prefix_candidates("Common.2024.GroupName.file.1.txt", &files, 3, 5);
        // Common.GroupName should NOT be found because Common and GroupName are not contiguous
        // But Common should be found as a single-part candidate, and GroupName too
        let common_groupname = candidates.iter().find(|c| c.prefix == "Common.GroupName");
        assert!(
            common_groupname.is_none(),
            "Should NOT find Common.GroupName when parts are not contiguous"
        );
    }

    #[test]
    fn realistic_scenario_with_dates_in_middle() {
        // Realistic scenario where studio name appears after date
        let files = make_test_files(&[
            "2024.01.15.StudioName.Video.Title.mp4",
            "2024.01.16.StudioName.Another.Video.mp4",
            "2024.01.17.StudioName.Third.Video.mp4",
        ]);
        let candidates = find_prefix_candidates("2024.01.15.StudioName.Video.Title.mp4", &files, 3, 5);
        assert!(
            candidates.iter().any(|c| c.prefix == "StudioName"),
            "Expected to find StudioName candidate, got: {candidates:?}"
        );
    }
}

#[cfg(test)]
mod test_file_extension_exclusion {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn extension_not_included_as_candidate() {
        // File extension should never be offered as a group name
        let files = make_test_files(&[
            "StudioName.Video.Title.mp4",
            "StudioName.Another.Video.mp4",
            "StudioName.Third.Video.mp4",
        ]);
        let candidates = find_prefix_candidates("StudioName.Video.Title.mp4", &files, 3, 1);
        // "mp4" should NOT appear as a candidate
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "mp4"),
            "File extension 'mp4' should not be offered as a group name, got: {candidates:?}"
        );
    }

    #[test]
    fn extension_not_included_even_when_common() {
        // Even if all files share the same extension, it shouldn't be a candidate
        let files = make_test_files(&["Different.Name.One.jpg", "Another.Name.Two.jpg", "Third.Name.Three.jpg"]);
        let candidates = find_prefix_candidates("Different.Name.One.jpg", &files, 3, 1);
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "jpg"),
            "File extension 'jpg' should not be offered as a group name"
        );
    }

    #[test]
    fn extension_not_part_of_multi_part_candidate() {
        // Extension should not be part of any multi-part candidate
        let files = make_test_files(&[
            "Studio.Name.Video.mp4",
            "Studio.Name.Other.mp4",
            "Studio.Name.Third.mp4",
        ]);
        let candidates = find_prefix_candidates("Studio.Name.Video.mp4", &files, 3, 1);
        // Should not have candidates like "Video.mp4" or "Name.Video.mp4"
        assert!(
            !candidates.iter().any(|c| c.prefix.contains("mp4")),
            "No candidate should contain the file extension 'mp4', got: {candidates:?}"
        );
    }

    #[test]
    fn long_extension_excluded() {
        // Longer extensions like "torrent" should also be excluded
        let files = make_test_files(&[
            "Movie.Name.2024.torrent",
            "Movie.Name.2023.torrent",
            "Movie.Name.2022.torrent",
        ]);
        let candidates = find_prefix_candidates("Movie.Name.2024.torrent", &files, 3, 1);
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "torrent"),
            "File extension 'torrent' should not be offered as a group name"
        );
        assert!(
            !candidates.iter().any(|c| c.prefix.contains("torrent")),
            "No candidate should contain 'torrent'"
        );
    }

    #[test]
    fn valid_candidates_still_found_without_extension() {
        // Verify that valid candidates are still found when extension is excluded
        let files = make_test_files(&[
            "StudioName.Video.One.mkv",
            "StudioName.Video.Two.mkv",
            "StudioName.Video.Three.mkv",
        ]);
        let candidates = find_prefix_candidates("StudioName.Video.One.mkv", &files, 3, 5);
        // Should find StudioName and Video as candidates
        assert!(
            candidates.iter().any(|c| c.prefix == "StudioName"),
            "Should find 'StudioName' as a candidate"
        );
        assert!(
            candidates.iter().any(|c| c.prefix == "Video"),
            "Should find 'Video' as a candidate"
        );
        // But not mkv
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "mkv"),
            "Should NOT find 'mkv' as a candidate"
        );
    }

    #[test]
    fn name_that_looks_like_extension_at_non_extension_position() {
        // A word that looks like an extension but isn't at the extension position should still be considered
        // (e.g., "mp4" as part of a name)
        let files = make_test_files(&[
            "Convert.mp4.to.mkv.Guide.One.pdf",
            "Convert.mp4.to.mkv.Guide.Two.pdf",
            "Convert.mp4.to.mkv.Guide.Three.pdf",
        ]);
        let candidates = find_prefix_candidates("Convert.mp4.to.mkv.Guide.One.pdf", &files, 3, 1);
        // "pdf" at extension position should be excluded
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "pdf"),
            "Extension 'pdf' should be excluded"
        );
        // But "mp4" in the middle of the name could be found if it meets criteria
        // (though it's only 3 chars so might be excluded by min_prefix_chars)
    }

    #[test]
    fn double_extension_only_last_excluded() {
        // For files like "file.tar.gz", only "gz" is the extension
        let files = make_test_files(&["Archive.backup.tar.gz", "Archive.data.tar.gz", "Archive.logs.tar.gz"]);
        let candidates = find_prefix_candidates("Archive.backup.tar.gz", &files, 3, 1);
        // "gz" should be excluded as extension
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "gz"),
            "Extension 'gz' should be excluded"
        );
        // "tar" is NOT the extension, it's part of the name, so it could be a candidate
        // (tar.gz is treated as extension=gz, name ending in tar)
    }

    #[test]
    fn extension_case_insensitive_exclusion() {
        // Extensions with different cases should all be excluded
        let files = make_test_files(&["Video.One.MP4", "Video.Two.Mp4", "Video.Three.mp4"]);
        let candidates = find_prefix_candidates("Video.One.MP4", &files, 3, 1);
        // All case variations of the extension should be excluded
        assert!(
            !candidates.iter().any(|c| c.prefix.to_lowercase() == "mp4"),
            "Extension in any case should be excluded"
        );
    }
}

#[cfg(test)]
mod test_contiguity_equivalence {
    use super::*;

    /// Contiguity check that lowercases and joins parts on every call, used as the reference implementation.
    fn reference(original_parts: &[String], prefix_parts: &[&str], prefix_combined: &str) -> bool {
        if prefix_parts.is_empty() {
            return true;
        }
        if original_parts.windows(prefix_parts.len()).any(|window| {
            window
                .iter()
                .zip(prefix_parts)
                .all(|(original_part, prefix_part)| original_part.eq_ignore_ascii_case(prefix_part))
        }) {
            return true;
        }
        for original_part in original_parts {
            let original_lower = original_part.to_lowercase();
            if original_lower == prefix_combined
                || (original_lower.starts_with(prefix_combined)
                    && FilteredParts::has_word_boundary_at(original_part, prefix_combined.len()))
            {
                return true;
            }
        }
        for start_index in 0..original_parts.len() {
            let mut combined = String::new();
            let mut combined_lower = String::new();
            for part in original_parts.iter().skip(start_index) {
                combined.push_str(part);
                combined_lower.push_str(&part.to_lowercase());
                if combined_lower == prefix_combined
                    || (combined_lower.starts_with(prefix_combined)
                        && FilteredParts::has_word_boundary_at(&combined, prefix_combined.len()))
                {
                    return true;
                }
                if combined.len() > prefix_combined.len() + 20 {
                    break;
                }
            }
        }
        false
    }

    #[test]
    fn agrees_with_the_allocating_reference() {
        let names = [
            "Photo.Lab.Image.One.jpg",
            "PhotoLab.Image.Two.jpg",
            "PhotoLabTV.Show.mp4",
            "Photo.Labs.Extra.jpg",
            "Jane.Doe.S01E01.720p.mp4",
            "JaneDoe2024.Special.mp4",
            "İstanbul.Trip.İmage.jpg",
            "Ärtist.Nämé.Song.mp3",
            "A..B...C.mp4",
            "Some.Very.Long.Part.Name.That.Goes.On.And.On.Forever.mp4",
        ];
        let prefixes: [&[&str]; 14] = [
            &["Photo", "Lab"],
            &["PhotoLab"],
            &["Photo"],
            &["Lab", "Image"],
            &["Jane", "Doe"],
            &["JaneDoe"],
            &["janedoe2024"],
            &["İstanbul"],
            &["i̇stanbul", "Trip"],
            &["Ärtist", "Nämé"],
            &["A", "B"],
            &[""],
            &["SomeVeryLongPartNameThat"],
            &["Missing", "Prefix"],
        ];
        for name in names {
            let original_parts: Vec<String> = name.split('.').map(String::from).collect();
            let original_parts_lower: Vec<String> = original_parts.iter().map(|part| part.to_lowercase()).collect();
            for prefix_parts in prefixes {
                let prefix_combined = prefix_parts.join("").to_lowercase();
                assert_eq!(
                    parts_are_contiguous_lowered(
                        &original_parts,
                        &original_parts_lower,
                        prefix_parts,
                        &prefix_combined
                    ),
                    reference(&original_parts, prefix_parts, &prefix_combined),
                    "{name} with {prefix_parts:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod test_full_flow {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn simulation_with_filtering() {
        let original_filenames = [
            "ShowName.2023.S01E01.720p.mp4",
            "ShowName.2024.S01E02.720p.mp4",
            "ShowName.2025.S01E03.720p.mp4",
            "OtherShow.2024.Special.mp4",
            "OtherShow.2024.Episode.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "ShowName.S01E01.mp4");
        assert_eq!(filtered[1].filtered_name, "ShowName.S01E02.mp4");
        assert_eq!(filtered[2].filtered_name, "ShowName.S01E03.mp4");
        assert_eq!(filtered[3].filtered_name, "OtherShow.Special.mp4");
        assert_eq!(filtered[4].filtered_name, "OtherShow.Episode.mp4");

        let show_candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        assert_eq!(show_candidates, vec![candidate("ShowName", 3, 1, 0)]);

        let other_candidates = find_prefix_candidates(&filtered[3].filtered_name, &filtered, 2, 1);
        assert_eq!(other_candidates, vec![candidate("OtherShow", 2, 1, 0)]);
    }

    #[test]
    fn with_resolution_numbers() {
        let original_filenames = [
            "MovieName.2024.720.rip.mp4",
            "MovieName.2024.720.other.mp4",
            "MovieName.2024.720.more.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "MovieName.rip.mp4");
        assert_eq!(filtered[1].filtered_name, "MovieName.other.mp4");
        assert_eq!(filtered[2].filtered_name, "MovieName.more.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        assert_eq!(candidates, vec![candidate("MovieName", 3, 1, 0)]);
    }

    #[test]
    fn with_resolution_pattern() {
        let original_filenames = [
            "MovieName.2024.1080p.rip.mp4",
            "MovieName.2024.1080p.other.mp4",
            "MovieName.2024.1080p.more.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "MovieName.rip.mp4");
        assert_eq!(filtered[1].filtered_name, "MovieName.other.mp4");
        assert_eq!(filtered[2].filtered_name, "MovieName.more.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        assert_eq!(candidates, vec![candidate("MovieName", 3, 1, 0)]);
    }

    #[test]
    fn with_glue_words() {
        let original_filenames = [
            "Show.and.Tell.part1.mp4",
            "Show.and.Tell.part2.mp4",
            "Show.and.Tell.part3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Show.Tell.part1.mp4");
        assert_eq!(filtered[1].filtered_name, "Show.Tell.part2.mp4");
        assert_eq!(filtered[2].filtered_name, "Show.Tell.part3.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // Note: Show.Tell won't match because "Show" and "Tell" are not contiguous in original
        // (they're separated by "and")
        // With position-agnostic matching, "Tell" is also found as a candidate
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Show", 3, 1, 0));
        assert_eq!(candidates[1], candidate("Tell", 3, 1, 1));
    }

    #[test]
    fn short_prefix_with_shared_parts() {
        let original_filenames = [
            "ABC.2023.Thing.v1.mp4",
            "ABC.2024.Thing.v2.mp4",
            "ABC.2025.Thing.v3.mp4",
        ];

        let unfiltered = make_test_files(&original_filenames);

        let candidates_unfiltered = find_prefix_candidates(&unfiltered[0].filtered_name, &unfiltered, 3, 1);
        // With position-agnostic matching, "Thing" is also found
        assert_eq!(candidates_unfiltered.len(), 2);
        assert!(
            candidates_unfiltered
                .iter()
                .any(|c| c.prefix == "ABC" && c.match_count == 3)
        );
        assert!(
            candidates_unfiltered
                .iter()
                .any(|c| c.prefix == "Thing" && c.match_count == 3)
        );

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "ABC.Thing.v1.mp4");
        assert_eq!(filtered[1].filtered_name, "ABC.Thing.v2.mp4");
        assert_eq!(filtered[2].filtered_name, "ABC.Thing.v3.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // Note: ABC.Thing won't match because "ABC" and "Thing" are not contiguous in original
        // (they're separated by year like "2023")
        // With position-agnostic matching, "Thing" is also found as a candidate
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().any(|c| c.prefix == "ABC" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Thing" && c.match_count == 3));
    }

    #[test]
    fn files_with_resolution_grouped_correctly() {
        let original_filenames = [
            "Some.Video.1080p.part1.mp4",
            "Some.Video.1080p.part2.mp4",
            "Some.Video.1080p.part3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Some.Video.part1.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // With position-agnostic matching, "Video" is also found
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Some.Video" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Some" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Video" && c.match_count == 3));

        let some_video = candidates.iter().find(|c| c.prefix == "Some.Video").unwrap();
        let dir_name = some_video.prefix.replace('.', " ");
        assert_eq!(dir_name, "Some Video");
    }

    #[test]
    fn files_with_2160p_resolution() {
        let original_filenames = [
            "Movie.Name.2160p.file1.mp4",
            "Movie.Name.2160p.file2.mp4",
            "Movie.Name.2160p.file3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Movie.Name.file1.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // With position-agnostic matching, "Movie" and "Name" are also found
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Movie.Name" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Movie" && c.match_count == 3));

        let movie_name = candidates.iter().find(|c| c.prefix == "Movie.Name").unwrap();
        let dir_name = movie_name.prefix.replace('.', " ");
        assert_eq!(dir_name, "Movie Name");
    }

    #[test]
    fn files_with_dimension_resolution() {
        let original_filenames = [
            "Cool.Stuff.1920x1080.part1.mp4",
            "Cool.Stuff.1920x1080.part2.mp4",
            "Cool.Stuff.1920x1080.part3.mp4",
        ];

        let filtered = make_filtered_files(&original_filenames);

        assert_eq!(filtered[0].filtered_name, "Cool.Stuff.part1.mp4");

        let candidates = find_prefix_candidates(&filtered[0].filtered_name, &filtered, 3, 1);
        // With position-agnostic matching, "Stuff" is also found
        assert!(
            candidates
                .iter()
                .any(|c| c.prefix == "Cool.Stuff" && c.match_count == 3)
        );
        assert!(candidates.iter().any(|c| c.prefix == "Cool" && c.match_count == 3));
        assert!(candidates.iter().any(|c| c.prefix == "Stuff" && c.match_count == 3));

        let cool_stuff = candidates.iter().find(|c| c.prefix == "Cool.Stuff").unwrap();
        let dir_name = cool_stuff.prefix.replace('.', " ");
        assert_eq!(dir_name, "Cool Stuff");
    }
}
