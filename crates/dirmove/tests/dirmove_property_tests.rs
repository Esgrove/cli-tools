//! Property tests for the `dirmove` name normalization, highlighting, and prefix stripping.
//!
//! These check that `normalize_name` is idempotent
//! and leaves no spaces, dots, or uppercase letters,
//! that `NormalizedFilename::find_match` returns a range on
//! character boundaries whose text normalizes to the searched name,
//! and that stripping ignored prefixes only ever removes text from the start of a name.
//! Set `PROPTEST_CASES` to run more cases than the default.

use cli_tools_dirmove::utils::normalize_name;
use cli_tools_dirmove::{NameFilter, NormalizedFilename};
use proptest::prelude::*;

/// File name text made of words, separators, and multi-byte letters.
fn file_name_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            ".", " ", "Show", "name", "S01E01", "İ", "ß", "Ä", "ö", "日本", "😀", "2024", "x", "DL", "Studio",
        ]),
        0..12,
    )
    .prop_map(|parts| parts.concat())
}

/// File name text with at least one letter that survives normalization, in a random position.
fn file_name_text_with_a_word() -> impl Strategy<Value = String> {
    (
        file_name_text(),
        prop::sample::select(vec!["Show", "ö", "日本", "İ"]),
        file_name_text(),
    )
        .prop_map(|(before, word, after)| format!("{before}{word}{after}"))
}

/// Default config, honouring `PROPTEST_CASES`, without failure files.
///
/// Integration tests have no `lib.rs` next to them for proptest to store regressions beside,
/// and a shrunk failure is turned into a unit test in the owning crate instead.
fn config_without_persistence() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config_without_persistence())]

    #[test]
    fn normalize_name_is_idempotent(name in file_name_text()) {
        let normalized = normalize_name(&name);
        prop_assert!(!normalized.contains([' ', '.']));
        prop_assert_eq!(normalized.to_lowercase(), normalized.as_str());
        prop_assert_eq!(normalize_name(&normalized), normalized);
    }

    #[test]
    fn find_match_range_covers_text_that_normalizes_to_the_needle(
        name in file_name_text_with_a_word(),
        start in any::<prop::sample::Index>(),
        length in 1_usize..12,
    ) {
        let normalized = normalize_name(&name);
        let characters: Vec<char> = normalized.chars().collect();
        let start = start.index(characters.len());
        let end = (start + length).min(characters.len());
        let needle: String = characters[start..end].iter().collect();

        let range = NormalizedFilename::new(&name).find_match(&needle);

        let range = range.expect("a substring of the normalized name should be found");
        prop_assert!(range.start < range.end);
        let matched = name.get(range.start..range.end).expect("the range should be on character boundaries");
        prop_assert!(normalize_name(matched).contains(&needle), "{matched:?} does not contain {needle:?}");
    }

    #[test]
    fn find_match_never_matches_an_empty_needle(name in file_name_text()) {
        prop_assert!(NormalizedFilename::new(&name).find_match("").is_none());
    }

    #[test]
    fn stripping_ignored_prefixes_only_removes_a_leading_part(name in file_name_text()) {
        let name_filter = NameFilter::new(["studio", "dl"], Vec::<String>::new());

        let stripped = name_filter.strip_ignored_prefixes(&name);
        prop_assert!(name.ends_with(stripped.as_ref()), "{name:?} -> {stripped:?}");

        let dot_stripped = name_filter.strip_ignored_dot_prefixes(&name);
        prop_assert!(name.ends_with(&dot_stripped), "{name:?} -> {dot_stripped:?}");
    }
}
