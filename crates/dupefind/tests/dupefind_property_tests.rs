//! Property tests for the file name normalization used to group duplicates.
//!
//! Generated stems mix words, resolutions, codec tags, separators, and Unicode,
//! and generated prefixes come from the same words so they match often.
//! Set `PROPTEST_CASES` to run more cases than the default.

use cli_tools_dupefind::{normalize_stem, strip_ignored_prefixes};
use proptest::prelude::*;

/// Stem fragments, including resolution and codec tags that normalization removes.
const FRAGMENTS: &[&str] = &[
    "Movie",
    "show",
    "SITE",
    "Name",
    "x265",
    "x264",
    "HEVC",
    "h264",
    "1080p",
    "720p",
    "2160p",
    "1920x1080",
    "4k",
    "2024",
    ".",
    ".",
    "..",
    "_",
    "-",
    " ",
    "é",
    "İ",
    "😀",
    "\u{a0}",
];

/// Stem built from fragments.
fn stem() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(FRAGMENTS), 0..14).prop_map(|parts| parts.concat())
}

/// Prefixes of one or two dot separated words, sometimes empty.
fn prefixes() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![
            prop::sample::select(vec!["movie", "SITE", "show.name", "", "x265"]).prop_map(str::to_string),
            "[a-z]{1,4}",
        ],
        0..4,
    )
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
    fn normalize_stem_is_idempotent(stem in stem()) {
        let once = normalize_stem(&stem);
        prop_assert_eq!(normalize_stem(&once), once, "{:?}", stem);
    }

    #[test]
    fn normalize_stem_is_lowercase_and_not_empty_for_non_empty_input(stem in stem()) {
        let normalized = normalize_stem(&stem);
        prop_assert_eq!(normalized.to_lowercase(), normalized.as_str());
        if !stem.is_empty() {
            prop_assert!(!normalized.is_empty(), "{:?}", stem);
        }
    }

    #[test]
    fn normalize_stem_never_panics(stem in "\\PC{0,80}") {
        let _ = normalize_stem(&stem);
    }

    #[test]
    fn stripping_prefixes_leaves_a_suffix(stem in stem(), prefixes in prefixes()) {
        let stripped = strip_ignored_prefixes(&stem, &prefixes);
        prop_assert!(stem.ends_with(&stripped), "{:?} became {:?}", stem, stripped);
    }

    #[test]
    fn stripping_prefixes_is_idempotent(stem in stem(), prefixes in prefixes()) {
        let once = strip_ignored_prefixes(&stem, &prefixes);
        prop_assert_eq!(strip_ignored_prefixes(&once, &prefixes), once);
    }
}
