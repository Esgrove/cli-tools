use std::borrow::Cow;
use std::path::Path;

use super::types::{FileInfo, FilteredParts, PrefixCandidate};
use crate::RE_RESOLUTION;

/// Common glue words to filter out from grouping names.
pub const GLUE_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "in", "of", "on", "or", "the", "to", "with",
];

/// Directory names that should be deleted when encountered.
pub const UNWANTED_DIRECTORIES: &[&str] = &[".unwanted"];

/// Normalize a name for comparison by lowercasing and removing spaces and dots.
/// This allows matching variations like "Jane Doe", "`JaneDoe`", and "Jane.Doe".
#[allow(clippy::doc_markdown)]
#[must_use]
pub fn normalize_name(name: &str) -> String {
    name.to_lowercase().replace([' ', '.'], "")
}

/// Recursively copy a directory and its contents.
///
/// # Errors
///
/// Returns an error if directory creation, reading, or file copying fails.
pub fn copy_dir_recursive(source: &Path, target: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(target)?;

    for entry in std::fs::read_dir(source)?.filter_map(Result::ok) {
        let src_path = entry.path();
        let dst_path = target.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Filter out dot-separated parts that contain only numeric digits, resolution patterns, or glue words.
///
/// For example, "Show.2024.S01E01.mkv" becomes "Show.S01E01.mkv".
/// For example, "Show.1080p.S01E01.mkv" becomes "Show.S01E01.mkv".
/// For example, "Show.and.Tell.mkv" becomes "Show.Tell.mkv".
#[must_use]
pub fn filter_numeric_resolution_and_glue_parts(filename: &str) -> String {
    filename
        .split('.')
        .filter(|part| {
            if part.is_empty() {
                return true;
            }
            // Filter purely numeric parts
            if part.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            // Filter resolution patterns
            if RE_RESOLUTION.is_match(part) {
                return false;
            }
            // Filter glue words (case-insensitive)
            !GLUE_WORDS.contains(&part.to_lowercase().as_str())
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Find prefix candidates for a file from any position in the filename.
///
/// Returns candidates in priority order: 3-part sequences, 2-part sequences, 1-part sequences.
/// Longer prefixes are preferred as they provide more specific grouping.
/// Also handles case variations and dot-separated vs concatenated forms.
///
/// Unlike prefix-only matching, this function extracts candidates from all positions
/// in the filename, allowing common group names that appear in the middle of filenames
/// to be detected.
///
/// The file extension (last part after the final dot) is excluded from candidate generation.
#[must_use]
pub fn find_prefix_candidates<'a>(
    file_name: &'a str,
    all_files: &[FileInfo<'_>],
    min_group_size: usize,
    min_prefix_chars: usize,
) -> Vec<PrefixCandidate<'a>> {
    let mut candidates: Vec<PrefixCandidate<'a>> = Vec::new();
    let mut seen_normalized: std::collections::HashSet<String> = std::collections::HashSet::new();

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
            let match_count = all_files
                .iter()
                .filter(|f| {
                    prefix_matches_normalized_precomputed(f, &three_part_normalized)
                        && parts_are_contiguous_with_combined(&f.original_parts, &prefix_parts, &prefix_combined)
                })
                .count();
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
            let match_count = all_files
                .iter()
                .filter(|f| {
                    prefix_matches_normalized_precomputed(f, &two_part_normalized)
                        && parts_are_contiguous_with_combined(&f.original_parts, &prefix_parts, &prefix_combined)
                })
                .count();
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

            let match_count = all_files
                .iter()
                .filter(|f| prefix_matches_normalized_precomputed(f, &single_part_normalized))
                .count();
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
    prefix.chars().filter(|c| *c != '.').count()
}

/// Check if pre-computed [`FileInfo`] parts contain the given normalized target.
///
/// Delegates to [`FilteredParts::prefix_matches_normalized`] which uses the pre-split
/// and pre-lowercased single, 2-part, and 3-part combinations stored on `FilteredParts`
/// to avoid redundant `split('.')`, `to_lowercase()`, and `format!()` calls in the
/// O(N × K × N) hot loop inside `find_prefix_candidates`.
#[must_use]
pub fn prefix_matches_normalized_precomputed(file_info: &FileInfo<'_>, target_normalized: &str) -> bool {
    file_info.filtered_parts.prefix_matches_normalized(target_normalized)
}

/// Inner implementation of contiguity checking that takes a pre-computed
/// `prefix_combined` (the joined, lowercased prefix parts).
///
/// Call this directly when the same `prefix_parts` are checked against many files
/// to avoid recomputing the `join + lowercase` on every call.
#[must_use]
pub fn parts_are_contiguous_with_combined(
    original_parts: &[String],
    prefix_parts: &[&str],
    prefix_combined: &str,
) -> bool {
    if prefix_parts.is_empty() {
        return true;
    }

    // Check if the prefix parts match contiguous original parts exactly
    'outer: for start_idx in 0..original_parts.len() {
        if start_idx + prefix_parts.len() > original_parts.len() {
            break;
        }

        for (offset, prefix_part) in prefix_parts.iter().enumerate() {
            if !original_parts[start_idx + offset].eq_ignore_ascii_case(prefix_part) {
                continue 'outer;
            }
        }

        // Found a contiguous match with exact parts
        return true;
    }

    // Also check if prefix parts combined match a single original part (concatenated form)
    // e.g., prefix ["Photo", "Lab"] matches original part "PhotoLab"
    // Also matches if the original part STARTS WITH the prefix at a word boundary (extended form)
    // e.g., prefix ["Joseph", "Example"] matches original part "JosephExampleTV"
    // but NOT "JosephExamples" (no word boundary after "Example")
    for original_part in original_parts {
        let original_lower = original_part.to_lowercase();
        if original_lower == prefix_combined
            || (original_lower.starts_with(prefix_combined)
                && FilteredParts::has_word_boundary_at(original_part, prefix_combined.len()))
        {
            return true;
        }
    }

    // Also check if prefix parts combined match multiple contiguous original parts combined
    // e.g., prefix ["PhotoLab"] (single part) matches original ["Photo", "Lab"] (two parts)
    // Also matches if the combined parts START WITH the prefix at a word boundary
    for start_idx in 0..original_parts.len() {
        let mut combined = String::new();
        let mut combined_lower = String::new();
        for part in original_parts.iter().skip(start_idx) {
            combined.push_str(part);
            combined_lower.push_str(&part.to_lowercase());
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
/// For `A.B.C.D`, with n=2, returns `["A.B", "B.C", "C.D"]`.
/// This allows finding common group names that appear anywhere in filenames,
/// not just at the start.
#[must_use]
pub fn get_all_n_part_sequences(file_name: &str, n: usize) -> Vec<&str> {
    if n == 0 {
        return Vec::new();
    }

    // Collect the start position of each part (after each dot, plus position 0)
    let mut part_starts: Vec<usize> = vec![0];
    for (i, c) in file_name.bytes().enumerate() {
        if c == b'.' {
            part_starts.push(i + 1);
        }
    }
    let num_parts = part_starts.len();

    if num_parts < n {
        return Vec::new();
    }

    let mut sequences = Vec::new();
    for start in 0..=(num_parts - n) {
        let start_pos = part_starts[start];
        let end_pos = if start + n < num_parts {
            // End just before the next part's dot
            part_starts[start + n] - 1
        } else {
            file_name.len()
        };

        if start_pos < end_pos {
            sequences.push(&file_name[start_pos..end_pos]);
        }
    }

    sequences
}

/// Check if a directory name is in the unwanted list.
#[must_use]
pub fn is_unwanted_directory(name: &str) -> bool {
    UNWANTED_DIRECTORIES.iter().any(|u| name.eq_ignore_ascii_case(u))
}
