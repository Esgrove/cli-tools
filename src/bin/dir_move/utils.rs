use crate::types::{FileInfo, PrefixCandidate};
use regex::Regex;
use std::borrow::Cow;
use std::path::Path;
use std::sync::LazyLock;

/// Regex to match video resolutions like 1080p, 2160p, or 1920x1080.
pub static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4}p|\d{3,4}x\d{3,4})\b").expect("Invalid resolution regex"));

/// Common glue words to filter out from grouping names.
const GLUE_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "in", "of", "on", "or", "the", "to", "with",
];

/// Directory names that should be deleted when encountered.
const UNWANTED_DIRECTORIES: &[&str] = &[".unwanted"];

/// Recursively copy a directory and its contents.
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
/// For example, "Show.2024.S01E01.mkv" becomes "Show.S01E01.mkv".
/// For example, "Show.1080p.S01E01.mkv" becomes "Show.S01E01.mkv".
/// For example, "Show.and.Tell.mkv" becomes "Show.Tell.mkv".
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
/// Returns candidates in priority order: 3-part sequences, 2-part sequences, 1-part sequences.
/// Longer prefixes are preferred as they provide more specific grouping.
/// Also handles case variations and dot-separated vs concatenated forms.
///
/// Unlike prefix-only matching, this function extracts candidates from all positions
/// in the filename, allowing common group names that appear in the middle of filenames
/// to be detected.
///
/// The file extension (last part after the final dot) is excluded from candidate generation.
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
            let three_part_normalized = normalize_prefix(three_part);
            // Skip if we've already processed this normalized form
            if seen_normalized.contains(&three_part_normalized) {
                continue;
            }
            seen_normalized.insert(three_part_normalized.clone());

            let prefix_parts: Vec<&str> = three_part.split('.').collect();
            let match_count = all_files
                .iter()
                .filter(|f| {
                    prefix_matches_normalized(&f.filtered_name, &three_part_normalized)
                        && parts_are_contiguous_in_original(&f.original_name, &prefix_parts)
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
            let two_part_normalized = normalize_prefix(two_part);
            // Skip if we've already processed this normalized form
            if seen_normalized.contains(&two_part_normalized) {
                continue;
            }
            seen_normalized.insert(two_part_normalized.clone());

            let prefix_parts: Vec<&str> = two_part.split('.').collect();
            let match_count = all_files
                .iter()
                .filter(|f| {
                    prefix_matches_normalized(&f.filtered_name, &two_part_normalized)
                        && parts_are_contiguous_in_original(&f.original_name, &prefix_parts)
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
                .filter(|f| prefix_matches_normalized(&f.filtered_name, &single_part_normalized))
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
pub fn count_prefix_chars(prefix: &str) -> usize {
    prefix.chars().filter(|c| *c != '.').count()
}

/// Check if a filename contains the given normalized target anywhere in the name.
/// Checks all single parts and all contiguous 2-part and 3-part combinations to handle cases like:
/// - "PhotoLab.Image" matching "photolab" (single part exact)
/// - "PhotoLabTV.Image" matching "photolab" (single part starts with)
/// - "Photo.Lab.Image" matching "photolab" (2-part combined)
/// - "Something.Photo.Lab.Image" matching "photolab" (2-part combined, not at start)
/// - "Extra.PhotoLabTV.Image" matching "photolab" (single part starts with, not at start)
pub fn prefix_matches_normalized(file_name: &str, target_normalized: &str) -> bool {
    let parts: Vec<&str> = file_name.split('.').collect();

    // Check all single parts (exact match or starts with)
    for part in &parts {
        let part_lower = part.to_lowercase();
        if part_lower == *target_normalized || part_lower.starts_with(target_normalized) {
            return true;
        }
    }

    // Check all 2-part combinations (exact match or starts with)
    for window in parts.windows(2) {
        let two_combined = format!("{}{}", window[0], window[1]).to_lowercase();
        if two_combined == *target_normalized || two_combined.starts_with(target_normalized) {
            return true;
        }
    }

    // Check all 3-part combinations (exact match or starts with)
    for window in parts.windows(3) {
        let three_combined = format!("{}{}{}", window[0], window[1], window[2]).to_lowercase();
        if three_combined == *target_normalized || three_combined.starts_with(target_normalized) {
            return true;
        }
    }

    false
}

/// Check if a sequence of parts appears contiguously in the original filename.
/// This prevents false matches where filtering removes parts and makes
/// non-adjacent parts appear adjacent.
///
/// For example, if the prefix is "Site.Person" but the original filename is
/// "Site.2023.04.13.Person.video.mp4", this should return false because
/// "Site" and "Person" are not adjacent in the original.
///
/// Also handles concatenated forms: `prefix_parts` `["Photo", "Lab"]` matches
/// original part `PhotoLab` (single concatenated part).
///
/// Also handles extended forms: `prefix_parts` `["Joseph", "Example"]` matches
/// original part `JosephExampleTV` (starts with the prefix).
pub fn parts_are_contiguous_in_original(original_filename: &str, prefix_parts: &[&str]) -> bool {
    if prefix_parts.is_empty() {
        return true;
    }

    let original_parts: Vec<&str> = original_filename.split('.').collect();
    let prefix_combined = prefix_parts.join("").to_lowercase();

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
    // Also matches if the original part STARTS WITH the prefix (extended form)
    // e.g., prefix ["Joseph", "Example"] matches original part "JosephExampleTV"
    for original_part in &original_parts {
        let original_lower = original_part.to_lowercase();
        if original_lower == prefix_combined || original_lower.starts_with(&prefix_combined) {
            return true;
        }
    }

    // Also check if prefix parts combined match multiple contiguous original parts combined
    // e.g., prefix ["PhotoLab"] (single part) matches original ["Photo", "Lab"] (two parts)
    // Also matches if the combined parts START WITH the prefix
    for start_idx in 0..original_parts.len() {
        let mut combined = String::new();
        for part in original_parts.iter().skip(start_idx) {
            combined.push_str(part);
            let combined_lower = combined.to_lowercase();
            if combined_lower == prefix_combined || combined_lower.starts_with(&prefix_combined) {
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

/// Normalize a prefix for comparison by removing dots and lowercasing.
/// This allows "Show.TV" and "`ShowTV`" to be treated as equivalent.
pub fn normalize_prefix(prefix: &str) -> String {
    prefix.replace('.', "").to_lowercase()
}

/// Extract all N-part sequences from a filename as string slices.
/// For `A.B.C.D`, with n=2, returns `["A.B", "B.C", "C.D"]`.
/// This allows finding common group names that appear anywhere in filenames,
/// not just at the start.
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
pub fn is_unwanted_directory(name: &str) -> bool {
    UNWANTED_DIRECTORIES.iter().any(|u| name.eq_ignore_ascii_case(u))
}

#[cfg(test)]
mod test_contiguity {
    use super::*;

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
        // "StudioAlphaProductions" starts with "StudioAlpha", so it SHOULD match
        // prefix ["Studio", "Alpha"]. This allows grouping files like:
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
mod test_filtering {
    use super::*;

    #[test]
    fn removes_year() {
        let result = filter_numeric_resolution_and_glue_parts("Show.2024.Episode.mp4");
        assert_eq!(result, "Show.Episode.mp4");
    }

    #[test]
    fn removes_multiple_numeric() {
        let result = filter_numeric_resolution_and_glue_parts("Show.2024.01.Episode.mp4");
        assert_eq!(result, "Show.Episode.mp4");
    }

    #[test]
    fn keeps_mixed_alphanumeric() {
        let result = filter_numeric_resolution_and_glue_parts("Show.S01E02.Episode.mp4");
        assert_eq!(result, "Show.S01E02.Episode.mp4");
    }

    #[test]
    fn no_numeric_parts() {
        let result = filter_numeric_resolution_and_glue_parts("Show.Name.Episode.mp4");
        assert_eq!(result, "Show.Name.Episode.mp4");
    }

    #[test]
    fn all_numeric_except_extension() {
        let result = filter_numeric_resolution_and_glue_parts("2024.01.15.mp4");
        assert_eq!(result, "mp4");
    }

    #[test]
    fn empty_string() {
        let result = filter_numeric_resolution_and_glue_parts("");
        assert_eq!(result, "");
    }

    #[test]
    fn single_part() {
        let result = filter_numeric_resolution_and_glue_parts("file.mp4");
        assert_eq!(result, "file.mp4");
    }

    #[test]
    fn removes_1080p() {
        let result = filter_numeric_resolution_and_glue_parts("Movie.Name.1080p.BluRay.mp4");
        assert_eq!(result, "Movie.Name.BluRay.mp4");
    }

    #[test]
    fn removes_2160p() {
        let result = filter_numeric_resolution_and_glue_parts("Movie.Name.2160p.UHD.mp4");
        assert_eq!(result, "Movie.Name.UHD.mp4");
    }

    #[test]
    fn removes_720p() {
        let result = filter_numeric_resolution_and_glue_parts("Movie.Name.720p.WEB.mp4");
        assert_eq!(result, "Movie.Name.WEB.mp4");
    }

    #[test]
    fn removes_dimension_format() {
        let result = filter_numeric_resolution_and_glue_parts("Video.1920x1080.Sample.mp4");
        assert_eq!(result, "Video.Sample.mp4");
    }

    #[test]
    fn removes_smaller_dimension() {
        let result = filter_numeric_resolution_and_glue_parts("Video.640x480.Old.mp4");
        assert_eq!(result, "Video.Old.mp4");
    }

    #[test]
    fn case_insensitive_resolution() {
        let result = filter_numeric_resolution_and_glue_parts("Movie.Name.1080P.BluRay.mp4");
        assert_eq!(result, "Movie.Name.BluRay.mp4");
    }

    #[test]
    fn removes_and_glue_word() {
        let result = filter_numeric_resolution_and_glue_parts("Show.and.Tell.mp4");
        assert_eq!(result, "Show.Tell.mp4");
    }

    #[test]
    fn removes_the_glue_word() {
        let result = filter_numeric_resolution_and_glue_parts("The.Movie.Name.mp4");
        assert_eq!(result, "Movie.Name.mp4");
    }

    #[test]
    fn removes_multiple_glue_words() {
        let result = filter_numeric_resolution_and_glue_parts("The.Show.and.The.Tell.mp4");
        assert_eq!(result, "Show.Tell.mp4");
    }

    #[test]
    fn glue_words_case_insensitive() {
        let result = filter_numeric_resolution_and_glue_parts("THE.Show.AND.Tell.mp4");
        assert_eq!(result, "Show.Tell.mp4");
    }

    #[test]
    fn removes_all_glue_words() {
        let result = filter_numeric_resolution_and_glue_parts("a.an.the.and.of.mp4");
        assert_eq!(result, "mp4");
    }

    #[test]
    fn complex_filtering_year_resolution_glue() {
        let result = filter_numeric_resolution_and_glue_parts("The.Movie.2024.1080p.and.More.mp4");
        assert_eq!(result, "Movie.More.mp4");
    }

    #[test]
    fn preserves_episode_codes() {
        let result = filter_numeric_resolution_and_glue_parts("Show.S01E01.2024.1080p.mp4");
        assert_eq!(result, "Show.S01E01.mp4");
    }

    #[test]
    fn keeps_4k_not_matched_by_resolution_regex() {
        let result = filter_numeric_resolution_and_glue_parts("Movie.4K.HDR.mp4");
        assert_eq!(result, "Movie.4K.HDR.mp4");
    }

    #[test]
    fn multiple_resolutions() {
        let result = filter_numeric_resolution_and_glue_parts("Movie.1080p.2160p.720p.mp4");
        assert_eq!(result, "Movie.mp4");
    }

    #[test]
    fn only_extension_left() {
        let result = filter_numeric_resolution_and_glue_parts("2024.1080p.the.mp4");
        assert_eq!(result, "mp4");
    }

    #[test]
    fn no_dots_in_name() {
        let result = filter_numeric_resolution_and_glue_parts("filename");
        assert_eq!(result, "filename");
    }

    #[test]
    fn consecutive_dots() {
        let result = filter_numeric_resolution_and_glue_parts("Show..Name..mp4");
        assert_eq!(result, "Show..Name..mp4");
    }

    #[test]
    fn generic_movie_name() {
        let result =
            filter_numeric_resolution_and_glue_parts("The.Action.Film.1999.Remastered.2160p.UHD.BluRay.x265.mp4");
        assert_eq!(result, "Action.Film.Remastered.UHD.BluRay.x265.mp4");
    }

    #[test]
    fn generic_tv_show() {
        let result = filter_numeric_resolution_and_glue_parts("Drama.Series.S05E16.2013.1080p.BluRay.mp4");
        assert_eq!(result, "Drama.Series.S05E16.BluRay.mp4");
    }
}

#[cfg(test)]
/// Tests for prefix candidate finding with ORIGINAL filenames.
/// These tests use the original test filenames from before position-agnostic matching
/// was implemented. Assertions have been updated to reflect the new behavior where
/// group names can be found at any position in the filename.
mod test_prefix_candidates {
    use super::*;
    use crate::dir_move::test_helpers::*;

    #[test]
    fn single_file_no_match() {
        // Original test: LongName only appears in one file, no group formed
        let files = make_test_files(&["LongName.v1.mp4", "Other.v2.mp4"]);
        let candidates = find_prefix_candidates("LongName.v1.mp4", &files, 2, 1);
        assert!(candidates.is_empty());
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
        // With min_group_size=3: only prefixes with 3+ files qualify
        // 3-part "Some.Name.Thing" has 2 files < 3, so excluded
        // 2-part "Some.Name" has 3 files >= 3, 1-part "Some" has 3 files >= 3
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
        assert!(candidates.is_empty());
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
        let files: Vec<FileInfo<'_>> = Vec::new();
        let candidates = find_prefix_candidates("Some.Name.mp4", &files, 2, 1);
        assert!(candidates.is_empty());
    }

    #[test]
    fn file_not_in_list() {
        let files = make_test_files(&["Other.Name.mp4", "Different.File.mp4"]);
        let candidates = find_prefix_candidates("Some.Name.mp4", &files, 2, 1);
        assert!(candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
        let showtv = candidates.iter().find(|c| c.prefix.to_lowercase() == "showtv");
        assert!(showtv.is_some());
        assert_eq!(showtv.unwrap().match_count, 3);
    }

    #[test]
    fn normalize_prefix_removes_dots_and_lowercases() {
        assert_eq!(normalize_prefix("PhotoLab"), "photolab");
        assert_eq!(normalize_prefix("Photo.Lab"), "photolab");
        assert_eq!(normalize_prefix("photo.lab"), "photolab");
        assert_eq!(normalize_prefix("Album.Name.Here"), "albumnamehere");
        assert_eq!(normalize_prefix("StudioTV"), "studiotv");
        assert_eq!(normalize_prefix("Studio.TV"), "studiotv");
        assert_eq!(normalize_prefix("studio.tv"), "studiotv");
        assert_eq!(normalize_prefix("Show.Name.Here"), "shownamehere");
    }

    #[test]
    fn prefix_matches_normalized_single_part() {
        assert!(prefix_matches_normalized("PhotoLab.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("PHOTOLAB.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("ShowTV.Episode.mp4", "showtv"));
        assert!(prefix_matches_normalized("SHOWTV.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_two_parts() {
        assert!(prefix_matches_normalized("Photo.Lab.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("photo.lab.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("Show.TV.Episode.mp4", "showtv"));
        assert!(prefix_matches_normalized("show.tv.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_three_parts() {
        assert!(prefix_matches_normalized("Ph.oto.Lab.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("Sh.ow.TV.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_no_match() {
        // These don't match because no part starts with the prefix
        assert!(!prefix_matches_normalized("Other.Album.jpg", "photolab"));
        assert!(!prefix_matches_normalized("Other.Show.mp4", "showtv"));
        // These don't match because "XPhotoLab" doesn't start with "photolab"
        assert!(!prefix_matches_normalized("XPhotoLab.Image.jpg", "photolab"));
        assert!(!prefix_matches_normalized("XShowTV.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_starts_with() {
        // These match because a part STARTS WITH the prefix
        // This allows grouping JosephExampleTV with JosephExample files
        assert!(prefix_matches_normalized("PhotoLabX.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("ShowTVX.Episode.mp4", "showtv"));
        assert!(prefix_matches_normalized("PhotoLabProductions.Image.jpg", "photolab"));
        assert!(prefix_matches_normalized("ShowTVNetwork.Episode.mp4", "showtv"));
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
        assert!(candidates.is_empty());

        // With min_group_size=2, "Vacation.Photos" qualifies
        let candidates = find_prefix_candidates("Vacation.Photos.Image1.jpg", &files, 2, 1);
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
        assert!(candidates.iter().any(|c| c.match_count == 3));

        // min_group_size=4, "Beach.Summer" with 3 files < 4, so excluded
        let candidates = find_prefix_candidates("Beach.Summer.Photo1.jpg", &files, 4, 1);
        assert!(candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(!candidates.is_empty());
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
        assert!(candidates.is_empty());

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
        assert!(!candidates.is_empty());
        // Should find 2-part prefix with all 4 files
        let two_part = candidates.iter().find(|c| c.prefix == "Wedding.Photos");
        assert!(two_part.is_some());
        assert_eq!(two_part.unwrap().match_count, 4);
    }

    #[test]
    fn identical_single_word_prefix_grouped() {
        let files = make_test_files(&["Concert.Image1.jpg", "Concert.Image2.jpg", "Concert.Image3.jpg"]);
        let candidates = find_prefix_candidates("Concert.Image1.jpg", &files, 2, 1);
        assert!(!candidates.is_empty());
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
        assert!(candidates.is_empty());

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
        assert!(result.is_empty());
    }

    #[test]
    fn zero_parts_returns_empty() {
        let result = get_all_n_part_sequences("A.B.C", 0);
        assert!(result.is_empty());
    }

    #[test]
    fn no_dots_single_part() {
        let result = get_all_n_part_sequences("SingleWord", 1);
        assert_eq!(result, vec!["SingleWord"]);
    }

    #[test]
    fn no_dots_multi_part_returns_empty() {
        let result = get_all_n_part_sequences("SingleWord", 2);
        assert!(result.is_empty());
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
    use super::*;
    use crate::dir_move::test_helpers::*;

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
        let photolab_candidate = candidates.iter().find(|c| normalize_prefix(&c.prefix) == "photolab");
        assert!(
            photolab_candidate.is_some(),
            "Expected to find PhotoLab-related candidate, got: {candidates:?}"
        );
    }

    #[test]
    fn prefix_matches_normalized_finds_anywhere() {
        // Test that prefix_matches_normalized checks all positions
        assert!(prefix_matches_normalized("Extra.StudioName.video.mp4", "studioname"));
        assert!(prefix_matches_normalized("More.Extra.StudioName.mp4", "studioname"));
        assert!(prefix_matches_normalized("StudioName.video.mp4", "studioname"));
    }

    #[test]
    fn prefix_matches_normalized_two_part_anywhere() {
        // Two-part prefix matching anywhere
        assert!(prefix_matches_normalized("Extra.Photo.Lab.video.mp4", "photolab"));
        assert!(prefix_matches_normalized("Photo.Lab.video.mp4", "photolab"));
        assert!(prefix_matches_normalized("More.Photo.Lab.mp4", "photolab"));
    }

    #[test]
    fn prefix_matches_normalized_three_part_anywhere() {
        // Three-part prefix matching anywhere
        assert!(prefix_matches_normalized(
            "Extra.Alpha.Beta.Gamma.video.mp4",
            "alphabetagamma"
        ));
        assert!(prefix_matches_normalized(
            "Alpha.Beta.Gamma.video.mp4",
            "alphabetagamma"
        ));
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
    use super::*;
    use crate::dir_move::test_helpers::*;

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
        // A word that looks like an extension but isn't at the extension position
        // should still be considered (e.g., "mp4" as part of a name)
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
