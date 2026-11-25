//! Filename prefix stripping and duplicate name normalization.

use std::sync::LazyLock;

use regex::Regex;

use cli_tools_common::RE_RESOLUTION;

/// Common codec patterns removed during normalization.
pub const CODEC_PATTERNS: &[&str] = &["x264", "x265", "h264", "h265"];

/// Regex matching codec patterns removed during normalization.
static RE_CODEC: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(r"(?i)\b({})\b", CODEC_PATTERNS.join("|"));
    Regex::new(&pattern).expect("Invalid codec regex")
});

/// Strip configured dot delimited prefixes from the start of a filename or stem.
///
/// Matching ignores case and is repeated, so multiple configured prefixes can be removed.
/// Prefixes only match complete dot delimited components.
#[must_use]
pub fn strip_ignored_prefixes(value: &str, prefix_ignores: &[String]) -> String {
    let mut result = value;

    loop {
        let matching_remainder = prefix_ignores.iter().find_map(|prefix| {
            if prefix.is_empty() {
                return None;
            }

            let prefix_parts: Vec<&str> = prefix.split('.').collect();
            let mut value_parts = result.splitn(prefix_parts.len() + 1, '.');
            let matches = prefix_parts.iter().all(|expected| {
                value_parts
                    .next()
                    .is_some_and(|actual| actual.to_lowercase() == expected.to_lowercase())
            });
            matches.then(|| value_parts.next()).flatten()
        });

        let Some(remainder) = matching_remainder else {
            break;
        };
        result = remainder;
    }

    result.to_string()
}

/// Normalize a file stem by removing resolution and codec patterns.
///
/// Converts to lowercase, strips resolution and codec tags, then cleans up repeated separators.
/// Trimming separators can expose a tag that an underscore hid from the word boundary, as in "_x265.movie",
/// so the removal repeats until nothing changes, which keeps the result the same for every spelling.
/// Falls back to the lowercased original stem if normalization would produce an empty string.
#[must_use]
pub fn normalize_stem(stem: &str) -> String {
    let lowercase = stem.to_lowercase();
    let mut normalized = remove_tags(&lowercase);
    if normalized.is_empty() {
        return lowercase;
    }
    // Every pass that changes the text makes it shorter, so this ends.
    loop {
        let next = remove_tags(&normalized);
        if next == normalized || next.is_empty() {
            return normalized;
        }
        normalized = next;
    }
}

/// Remove resolution and codec tags from lowercase text and collapse the separators left behind.
fn remove_tags(text: &str) -> String {
    let without_resolution = RE_RESOLUTION.replace_all(text, "");
    let without_codec = RE_CODEC.replace_all(&without_resolution, "");
    cli_tools_common::collapse_repeated_separators(&without_codec)
}

#[cfg(test)]
mod test_strip_ignored_prefixes {
    use super::*;

    #[test]
    fn strips_repeated_prefixes_case_insensitively() {
        let prefixes = vec!["first".to_string(), "second".to_string()];

        let stripped = strip_ignored_prefixes("FIRST.Second.Movie.Name", &prefixes);

        assert_eq!(stripped, "Movie.Name");
    }

    #[test]
    fn strips_prefixes_containing_multiple_dot_delimited_parts() {
        let prefixes = vec!["other.prefix".to_string()];

        assert_eq!(
            strip_ignored_prefixes("Other.Prefix.Movie.Name", &prefixes),
            "Movie.Name"
        );
    }

    #[test]
    fn requires_a_complete_dot_delimited_prefix() {
        let prefixes = vec!["pre".to_string()];

        assert_eq!(strip_ignored_prefixes("prefix.movie", &prefixes), "prefix.movie");
    }

    #[test]
    fn handles_unicode_prefix_lengths_without_panicking() {
        let prefixes = vec!["é".to_string()];

        assert_eq!(strip_ignored_prefixes("€.movie", &prefixes), "€.movie");
        assert_eq!(strip_ignored_prefixes("é.movie", &prefixes), "movie");
        assert_eq!(strip_ignored_prefixes("É.movie", &prefixes), "movie");
    }

    #[test]
    fn ignores_empty_configured_prefixes() {
        let prefixes = vec![String::new(), "release".to_string()];

        assert_eq!(strip_ignored_prefixes("Release.Movie.Name", &prefixes), "Movie.Name");
    }
}

#[cfg(test)]
mod test_normalize_stem {
    use super::*;

    #[test]
    fn removes_resolution_and_codec_tags() {
        assert_eq!(normalize_stem("Movie.Name.1080p.x265"), "movie.name");
        assert_eq!(normalize_stem("Movie.Name.1920x1080.H264"), "movie.name");
    }

    #[test]
    fn cleans_and_trims_repeated_separators() {
        assert_eq!(normalize_stem("Movie....Name..1080p"), "movie.name");
        assert_eq!(normalize_stem(" movie   name "), "movie name");
        assert_eq!(normalize_stem("-movie-title-"), "movie-title");
    }

    #[test]
    fn preserves_meaningful_name_content() {
        let normalized = normalize_stem("Some.Movie.2024.1080p.x265");

        assert_eq!(normalized, "some.movie.2024");
    }

    #[test]
    fn handles_tags_case_insensitively() {
        assert_eq!(normalize_stem("VIDEO.1080P.X265"), "video");
    }

    #[test]
    fn falls_back_when_everything_is_removed() {
        assert_eq!(normalize_stem("1080p"), "1080p");
        assert_eq!(normalize_stem("X265"), "x265");
    }
}

#[cfg(test)]
mod test_normalize_stem_fixpoint {
    use super::*;

    #[test]
    fn a_tag_exposed_by_trimming_is_removed_too() {
        assert_eq!(normalize_stem("_x265.MovieMovie"), "moviemovie");
        assert_eq!(normalize_stem("_x265.Movie"), normalize_stem("x265.Movie"));
        assert_eq!(normalize_stem("-1080p.Show"), "show");
    }

    #[test]
    fn a_stem_of_only_tags_falls_back_to_the_lowercase_stem() {
        assert_eq!(normalize_stem("_x265"), "x265");
        assert_eq!(normalize_stem("x265"), "x265");
    }
}
