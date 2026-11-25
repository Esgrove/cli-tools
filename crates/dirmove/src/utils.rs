//! File name normalization for `dirmove`.
//!
//! `normalize_name` gives the form names are compared in,
//! and `filter_numeric_resolution_and_glue_parts` drops the name parts that never belong in a group name.

use cli_tools_common::RE_RESOLUTION;

/// Common glue words to filter out from grouping names.
const GLUE_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "in", "of", "on", "or", "the", "to", "with",
];

/// Normalize a name for comparison by lowercasing and removing spaces and dots.
/// This allows matching variations like "Jane Doe", "`JaneDoe`", and "Jane.Doe".
#[allow(clippy::doc_markdown)]
#[must_use]
pub fn normalize_name(name: &str) -> String {
    name.to_lowercase().replace([' ', '.'], "")
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
            if part.chars().all(|character| character.is_ascii_digit()) {
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

#[cfg(test)]
mod test_normalize_name {
    use super::*;

    #[test]
    fn normalize_name_function() {
        assert_eq!(normalize_name("Hello World"), "helloworld");
        assert_eq!(normalize_name("UPPER CASE"), "uppercase");
        assert_eq!(normalize_name("NoSpaces"), "nospaces");
        assert_eq!(normalize_name("  extra  spaces  "), "extraspaces");
        assert_eq!(normalize_name("Hello.World"), "helloworld");
        assert_eq!(normalize_name("Name.With.Dots"), "namewithdots");
        assert_eq!(normalize_name("Mixed.Dots And Spaces"), "mixeddotsandspaces");
        assert_eq!(normalize_name("  leading trailing  "), "leadingtrailing");
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
