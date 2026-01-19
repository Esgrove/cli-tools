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

/// Find prefix candidates for a file, prioritizing earlier positions in the filename.
/// Returns candidates in priority order: 3-part prefix, 2-part prefix, 1-part prefix.
/// Longer prefixes are preferred as they provide more specific grouping.
/// Also handles case variations and dot-separated vs concatenated forms.
pub fn find_prefix_candidates<'a>(
    file_name: &'a str,
    all_files: &[FileInfo<'_>],
    min_group_size: usize,
    min_prefix_chars: usize,
) -> Vec<PrefixCandidate<'a>> {
    let Some(first_part) = file_name.split('.').next().filter(|p| !p.is_empty()) else {
        return Vec::new();
    };

    let mut candidates: Vec<PrefixCandidate<'a>> = Vec::new();

    // Check 3-part prefix if it meets minimum character count (excluding dots)
    if let Some(three_part) = get_n_part_prefix(file_name, 3) {
        let char_count = count_prefix_chars(three_part);
        if char_count >= min_prefix_chars {
            let three_part_normalized = normalize_prefix(three_part);
            let prefix_parts: Vec<&str> = three_part.split('.').collect();
            let match_count = all_files
                .iter()
                .filter(|f| {
                    prefix_matches_normalized(&f.filtered_name, &three_part_normalized)
                        && parts_are_contiguous_in_original(&f.original_name, &prefix_parts)
                })
                .count();
            if match_count >= min_group_size {
                candidates.push(PrefixCandidate::new(Cow::Borrowed(three_part), match_count, 3));
            }
        }
    }

    // Check 2-part prefix if it meets minimum character count (excluding dots)
    if let Some(two_part) = get_n_part_prefix(file_name, 2) {
        let char_count = count_prefix_chars(two_part);
        if char_count >= min_prefix_chars {
            let two_part_normalized = normalize_prefix(two_part);
            let prefix_parts: Vec<&str> = two_part.split('.').collect();
            let match_count = all_files
                .iter()
                .filter(|f| {
                    prefix_matches_normalized(&f.filtered_name, &two_part_normalized)
                        && parts_are_contiguous_in_original(&f.original_name, &prefix_parts)
                })
                .count();
            if match_count >= min_group_size {
                candidates.push(PrefixCandidate::new(Cow::Borrowed(two_part), match_count, 2));
            }
        }
    }

    // Check 1-part prefix if it meets minimum character count
    // to avoid false matches with short names like "alex", "name", etc.
    if first_part.chars().count() >= min_prefix_chars {
        let first_part_normalized = first_part.to_lowercase();
        let match_count = all_files
            .iter()
            .filter(|f| prefix_matches_normalized(&f.filtered_name, &first_part_normalized))
            .count();
        if match_count >= min_group_size {
            candidates.push(PrefixCandidate::new(Cow::Borrowed(first_part), match_count, 1));
        }
    }

    candidates
}

/// Count the number of characters in a prefix, excluding dots.
/// Uses `chars().count()` to properly handle Unicode characters.
pub fn count_prefix_chars(prefix: &str) -> usize {
    prefix.chars().filter(|c| *c != '.').count()
}

/// Check if a filename's prefix matches the given normalized target.
/// Checks 1-part, 2-part, and 3-part prefixes to handle cases like:
/// - "PhotoLab.Image" matching "photolab" (1-part exact)
/// - "PhotoLabTV.Image" matching "photolab" (1-part starts with)
/// - "Photo.Lab.Image" matching "photolab" (2-part combined)
/// - "Photo.Lab.TV.Image" matching "photolab" (2-part combined, starts with)
pub fn prefix_matches_normalized(file_name: &str, target_normalized: &str) -> bool {
    let parts: Vec<&str> = file_name.split('.').collect();

    // Check 1-part prefix (exact match or starts with)
    if let Some(&first) = parts.first() {
        let first_lower = first.to_lowercase();
        if first_lower == *target_normalized || first_lower.starts_with(target_normalized) {
            return true;
        }
    }

    // Check 2-part prefix combined (exact match or starts with)
    if parts.len() >= 2 {
        let two_combined = format!("{}{}", parts[0], parts[1]).to_lowercase();
        if two_combined == *target_normalized || two_combined.starts_with(target_normalized) {
            return true;
        }
    }

    // Check 3-part prefix combined (exact match or starts with)
    if parts.len() >= 3 {
        let three_combined = format!("{}{}{}", parts[0], parts[1], parts[2]).to_lowercase();
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

/// Extract a prefix consisting of the first n dot-separated parts.
/// Returns None if there aren't enough parts.
pub fn get_n_part_prefix(file_name: &str, n: usize) -> Option<&str> {
    let mut dots_found = 0;
    let mut nth_dot_pos = 0;

    for (i, c) in file_name.bytes().enumerate() {
        if c == b'.' {
            dots_found += 1;
            if dots_found == n {
                nth_dot_pos = i;
            } else if dots_found > n {
                // Found more than n dots, return prefix up to nth dot
                return Some(&file_name[..nth_dot_pos]);
            }
        }
    }

    // If we found exactly n dots, that's n+1 parts which is enough
    if dots_found >= n && nth_dot_pos > 0 {
        return Some(&file_name[..nth_dot_pos]);
    }

    // Not enough parts
    None
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
mod test_prefix_extraction {
    use super::*;

    #[test]
    fn three_parts_from_long_name() {
        assert_eq!(get_n_part_prefix("Some.Name.Thing.v1.mp4", 3), Some("Some.Name.Thing"));
    }

    #[test]
    fn two_parts_from_name() {
        assert_eq!(get_n_part_prefix("Some.Name.Thing.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn not_enough_parts_for_three() {
        assert_eq!(get_n_part_prefix("Some.Name.mp4", 3), None);
    }

    #[test]
    fn not_enough_parts_for_two() {
        assert_eq!(get_n_part_prefix("Some.mp4", 2), None);
    }

    #[test]
    fn exact_parts_for_two() {
        assert_eq!(get_n_part_prefix("Some.Name.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn single_part_name() {
        assert_eq!(get_n_part_prefix("file.mp4", 1), Some("file"));
    }

    #[test]
    fn empty_string() {
        assert_eq!(get_n_part_prefix("", 1), None);
    }

    #[test]
    fn no_extension() {
        assert_eq!(get_n_part_prefix("Some.Name", 1), Some("Some"));
    }

    #[test]
    fn many_parts() {
        assert_eq!(get_n_part_prefix("A.B.C.D.E.F.mp4", 3), Some("A.B.C"));
    }

    #[test]
    fn with_numbers_in_name() {
        assert_eq!(get_n_part_prefix("Show.2024.S01E01.mp4", 2), Some("Show.2024"));
    }

    #[test]
    fn with_special_characters() {
        assert_eq!(get_n_part_prefix("Show-Name.Part.One.mp4", 2), Some("Show-Name.Part"));
    }

    #[test]
    fn with_underscores() {
        assert_eq!(
            get_n_part_prefix("Show_Name.Part_One.Episode.mp4", 2),
            Some("Show_Name.Part_One")
        );
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
mod test_prefix_candidates {
    use super::*;
    use crate::dir_move::test_helpers::*;

    #[test]
    fn single_file_no_match() {
        let files = make_test_files(&["LongName.v1.mp4", "Other.v2.mp4"]);
        let candidates = find_prefix_candidates("LongName.v1.mp4", &files, 2, 1);
        assert!(candidates.is_empty());
    }

    #[test]
    fn simple_prefix_multiple_files() {
        let files = make_test_files(&["LongName.v1.mp4", "LongName.v2.mp4", "Other.v2.mp4"]);
        let candidates = find_prefix_candidates("LongName.v1.mp4", &files, 2, 1);
        assert_eq!(candidates, vec![candidate("LongName", 2, 1)]);
    }

    #[test]
    fn prioritizes_longer_prefix() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Thing.v3.mp4",
        ]);
        let candidates = find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 2, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Some.Name.Thing", 3, 3));
        assert_eq!(candidates[1], candidate("Some.Name", 3, 2));
        assert_eq!(candidates[2], candidate("Some", 3, 1));
    }

    #[test]
    fn mixed_prefixes_different_third_parts() {
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        let candidates = find_prefix_candidates("Some.Name.Thing.v1.mp4", &files, 2, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Some.Name.Thing", 2, 3));
        assert_eq!(candidates[1], candidate("Some.Name", 3, 2));
        assert_eq!(candidates[2], candidate("Some", 3, 1));
    }

    #[test]
    fn fallback_to_two_part_when_no_three_part_matches() {
        let files = make_test_files(&["Some.Name.Thing.mp4", "Some.Name.Other.mp4", "Some.Name.More.mp4"]);
        let candidates = find_prefix_candidates("Some.Name.Thing.mp4", &files, 2, 1);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Some.Name", 3, 2));
        assert_eq!(candidates[1], candidate("Some", 3, 1));
    }

    #[test]
    fn single_word_fallback() {
        let files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);
        let candidates = find_prefix_candidates("ABC.2023.Thing.mp4", &files, 3, 1);
        assert_eq!(candidates, vec![candidate("ABC", 3, 1)]);
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Some.Name", 3, 2));
        assert_eq!(candidates[1], candidate("Some", 3, 1));
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Show.Name", 3, 2));
        assert_eq!(candidates[1], candidate("Show", 5, 1));
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
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Unique.Name.v1", 1, 3));
        assert_eq!(candidates[1], candidate("Unique.Name", 1, 2));
        assert_eq!(candidates[2], candidate("Unique", 1, 1));
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Series.Episode", 10, 2));
        assert_eq!(candidates[1], candidate("Series", 10, 1));
    }

    #[test]
    fn case_insensitive_prefix_matching() {
        let files = make_test_files(&["Show.Name.v1.mp4", "show.name.v2.mp4", "SHOW.NAME.v3.mp4"]);
        let candidates = find_prefix_candidates("Show.Name.v1.mp4", &files, 2, 1);
        // Case-insensitive matching should group all three files
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Show.Name", 3, 2));
        assert_eq!(candidates[1], candidate("Show", 3, 1));
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
        assert_eq!(photolab.unwrap().match_count, 4); // count should be 4

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
        // These don't match because the first part doesn't start with the prefix
        assert!(!prefix_matches_normalized("Other.Album.jpg", "photolab"));
        assert!(!prefix_matches_normalized("Other.Show.mp4", "showtv"));
        // These don't match because the prefix appears in the middle, not at start
        assert!(!prefix_matches_normalized("XPhotoLab.Image.jpg", "photolab"));
        assert!(!prefix_matches_normalized("XShowTV.Episode.mp4", "showtv"));
    }

    #[test]
    fn prefix_matches_normalized_starts_with() {
        // These match because the first part STARTS WITH the prefix
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
        assert_eq!(candidates[0], candidate("Vacation.Photos", 2, 2));
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
        // Should offer longer prefixes first: 3-part, then 2-part, then 1-part
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Album.Name.Set1", 2, 3));
        assert_eq!(candidates[1], candidate("Album.Name", 3, 2));
        assert_eq!(candidates[2], candidate("Album", 4, 1));
    }

    #[test]
    fn min_group_size_one_matches_single_file() {
        let files = make_test_files(&["Unique.Name.File.jpg", "Other.Name.File.jpg"]);
        // With min_group_size=1, threshold is min(1, 2) = 1
        // All prefixes with at least 1 match qualify
        let candidates = find_prefix_candidates("Unique.Name.File.jpg", &files, 1, 1);
        assert!(!candidates.is_empty());
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Unique.Name.File", 1, 3));
        assert_eq!(candidates[1], candidate("Unique.Name", 1, 2));
        assert_eq!(candidates[2], candidate("Unique", 1, 1));
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Gallery.Photos", 5, 2));
        assert_eq!(candidates[1], candidate("Gallery", 5, 1));
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Drama.Series", 5, 2));
        assert_eq!(candidates[1], candidate("Drama", 5, 1));
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Studio.Action", 2, 2));
        assert_eq!(candidates[1], candidate("Studio", 4, 1));
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
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Drama.Series.Name", 4, 3));
        assert_eq!(candidates[1], candidate("Drama.Series", 4, 2));
        assert_eq!(candidates[2], candidate("Drama", 4, 1));
    }

    #[test]
    fn long_name_with_only_year_after_prefix() {
        let files = make_test_files(&[
            "Action.Movie.Title.2019.Directors.Cut.mp4",
            "Action.Movie.Title.2020.Extended.Edition.mp4",
            "Action.Movie.Title.2021.Remastered.mp4",
        ]);
        let candidates = find_prefix_candidates("Action.Movie.Title.2019.Directors.Cut.mp4", &files, 2, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Action.Movie.Title", 3, 3));
        assert_eq!(candidates[1], candidate("Action.Movie", 3, 2));
        assert_eq!(candidates[2], candidate("Action", 3, 1));
    }

    #[test]
    fn long_name_with_date_after_prefix() {
        let files = make_test_files(&[
            "Daily.News.Show.2024.01.15.Morning.Report.mp4",
            "Daily.News.Show.2024.01.16.Evening.Edition.mp4",
            "Daily.News.Show.2024.01.17.Special.Coverage.mp4",
        ]);
        let candidates = find_prefix_candidates("Daily.News.Show.2024.01.15.Morning.Report.mp4", &files, 3, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Daily.News.Show", 3, 3));
        assert_eq!(candidates[1], candidate("Daily.News", 3, 2));
        assert_eq!(candidates[2], candidate("Daily", 3, 1));
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
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Epic.Adventure.Saga", 3, 3));
        assert_eq!(candidates[1], candidate("Epic.Adventure", 4, 2));
        assert_eq!(candidates[2], candidate("Epic", 4, 1));
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
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Anthology.Series.Collection", 4, 3));
        assert_eq!(candidates[1], candidate("Anthology.Series", 4, 2));
        assert_eq!(candidates[2], candidate("Anthology", 4, 1));
    }

    #[test]
    fn long_name_documentary_with_regions() {
        let files = make_test_files(&[
            "Nature.Wildlife.Documentary.Africa.Savanna.2019.4K.mp4",
            "Nature.Wildlife.Documentary.Asia.Jungle.2020.4K.mp4",
            "Nature.Wildlife.Documentary.Europe.Alps.2021.4K.mp4",
        ]);
        let candidates = find_prefix_candidates("Nature.Wildlife.Documentary.Africa.Savanna.2019.4K.mp4", &files, 3, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Nature.Wildlife.Documentary", 3, 3));
        assert_eq!(candidates[1], candidate("Nature.Wildlife", 3, 2));
        assert_eq!(candidates[2], candidate("Nature", 3, 1));
    }

    #[test]
    fn long_name_with_version_and_year() {
        let files = make_test_files(&[
            "Software.Tutorial.Guide.v2.2023.Intro.Basics.mp4",
            "Software.Tutorial.Guide.v2.2023.Advanced.Topics.mp4",
            "Software.Tutorial.Guide.v2.2023.Expert.Masterclass.mp4",
        ]);
        let candidates = find_prefix_candidates("Software.Tutorial.Guide.v2.2023.Intro.Basics.mp4", &files, 3, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Software.Tutorial.Guide", 3, 3));
        assert_eq!(candidates[1], candidate("Software.Tutorial", 3, 2));
        assert_eq!(candidates[2], candidate("Software", 3, 1));
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
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("ABC.Thing", 3, 2));
        assert_eq!(candidates[1], candidate("ABC", 3, 1));
    }

    #[test]
    fn mixed_years_without_filtering() {
        let unfiltered_files = make_test_files(&["ABC.2023.Thing.mp4", "ABC.2024.Other.mp4", "ABC.2025.More.mp4"]);
        let candidates = find_prefix_candidates("ABC.2023.Thing.mp4", &unfiltered_files, 3, 1);
        assert_eq!(candidates, vec![candidate("ABC", 3, 1)]);
    }

    #[test]
    fn tv_show_generic_scenario() {
        let filtered_files = make_test_files(&[
            "Series.Name.S01E01.1080p.mp4",
            "Series.Name.S01E02.1080p.mp4",
            "Series.Name.S01E03.1080p.mp4",
        ]);
        let candidates = find_prefix_candidates("Series.Name.S01E01.1080p.mp4", &filtered_files, 3, 1);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Series.Name", 3, 2));
        assert_eq!(candidates[1], candidate("Series", 3, 1));
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
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Studio.Franchise.Title", 2, 3));
        assert_eq!(candidates[1], candidate("Studio.Franchise", 3, 2));
        assert_eq!(candidates[2], candidate("Studio", 3, 1));
    }

    #[test]
    fn long_name_decade_in_title() {
        let files = make_test_files(&[
            "Retro.Eighties.Collection.Vol1.Greatest.Hits.mp4",
            "Retro.Eighties.Collection.Vol2.Classic.Cuts.mp4",
            "Retro.Eighties.Collection.Vol3.Deep.Tracks.mp4",
        ]);
        let candidates = find_prefix_candidates("Retro.Eighties.Collection.Vol1.Greatest.Hits.mp4", &files, 3, 1);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Retro.Eighties.Collection", 3, 3));
        assert_eq!(candidates[1], candidate("Retro.Eighties", 3, 2));
        assert_eq!(candidates[2], candidate("Retro", 3, 1));
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
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], candidate("Alpha.Beta.Gamma", 3, 3));
        assert_eq!(candidates[1], candidate("Alpha.Beta", 4, 2));
        assert_eq!(candidates[2], candidate("Alpha", 4, 1));
    }

    #[test]
    fn only_two_files_high_threshold() {
        let files = make_test_files(&["Show.Name.v1.mp4", "Show.Name.v2.mp4"]);
        // With min_group_size=5, "Show.Name" has 2 files < 5, so excluded
        let candidates = find_prefix_candidates("Show.Name.v1.mp4", &files, 5, 1);
        assert!(candidates.is_empty());

        // With min_group_size=2, "Show.Name" qualifies
        let candidates = find_prefix_candidates("Show.Name.v1.mp4", &files, 2, 1);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], candidate("Show.Name", 2, 2));
        assert_eq!(candidates[1], candidate("Show", 2, 1));
    }
}
