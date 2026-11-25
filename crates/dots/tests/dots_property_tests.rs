//! Property tests for the dot formatting pipeline.
//!
//! Generated file names mix words, separators, brackets, dates, resolutions, Unicode digits,
//! and characters whose lowercase form has a different byte length.
//! The properties check that formatting never panics, is idempotent,
//! and never produces repeated, leading, or trailing dots.
//! Set `PROPTEST_CASES` to run more cases than the default.

use cli_tools_dots::{DotFormat, DotRenameConfig};
use proptest::prelude::*;

/// Name fragments that exercise the individual formatting steps.
const FRAGMENTS: &[&str] = &[
    "Movie",
    "the",
    "And",
    "x265",
    "1080p",
    "1920x1080",
    "2021",
    "2021.05.06",
    "25.05.30",
    "Jan",
    "March",
    "3",
    "12",
    "[",
    "]",
    "(",
    ")",
    "{",
    "}",
    ".",
    "..",
    " ",
    "_",
    "-",
    "!",
    "&",
    "'",
    ",",
    "www.example.com",
    "é",
    "İ",
    "\u{212A}",
    "\u{663}",
    "\u{ff13}",
    "😀",
    "\u{a0}",
    "\t",
];

/// Fragments without digits, so a name holds no date unless one is added on purpose.
const WORDS: &[&str] = &[
    "Movie",
    "the",
    "And",
    "Jan",
    "March",
    "[",
    "]",
    "(",
    ")",
    "{",
    "}",
    ".",
    "..",
    " ",
    "_",
    "-",
    "!",
    "&",
    "'",
    ",",
    "www.example.com",
    "é",
    "İ",
    "\u{212A}",
    "😀",
    "\u{a0}",
    "\t",
];

/// Date and number fragments, of which a name gets at most one.
const DATES: &[&str] = &[
    "x265",
    "1080p",
    "1920x1080",
    "2021",
    "2021.05.06",
    "25.05.30",
    "Jan.3.2020",
    "3",
    "12",
    "\u{663}",
    "\u{ff13}",
];

/// File stem built from fragments, since `format_name` takes names without the extension.
fn file_name() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(FRAGMENTS), 1..16).prop_map(|parts| parts.concat())
}

/// File stem with at most one date or number.
///
/// Date reordering handles one date per run by design,
/// so a name with several dates gains one more reordered date on each run and is left out of idempotence checks.
fn file_name_with_one_date() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(WORDS), 0..8),
        prop::option::of(prop::sample::select(DATES)),
        prop::collection::vec(prop::sample::select(WORDS), 0..8),
    )
        .prop_map(|(before, date, after)| format!("{}{}{}", before.concat(), date.unwrap_or_default(), after.concat()))
}

/// Configs covering the options that reorder or prefix name parts.
fn config() -> impl Strategy<Value = DotRenameConfig> {
    (
        any::<bool>(),
        any::<bool>(),
        prop::option::of(prop::sample::select(vec!["Show", "Movie", "İnfo"])),
        prop::option::of(prop::sample::select(vec!["Extra", "2021"])),
        prop::sample::select(vec![vec![], vec!["Movie".to_string()], vec!["Movie.2021".to_string()]]),
    )
        .prop_map(
            |(convert_case, date_starts_with_year, prefix, suffix, move_date_after_prefix)| DotRenameConfig {
                convert_case,
                date_starts_with_year,
                prefix: prefix.map(str::to_string),
                suffix: suffix.map(str::to_string),
                move_date_after_prefix,
                ..Default::default()
            },
        )
}

/// Assert the shape every formatted name must have.
fn assert_well_formed(input: &str, formatted: &str) -> Result<(), TestCaseError> {
    prop_assert!(!formatted.contains(".."), "{input:?} became {formatted:?}");
    prop_assert!(!formatted.starts_with('.'), "{input:?} became {formatted:?}");
    prop_assert!(!formatted.ends_with('.'), "{input:?} became {formatted:?}");
    Ok(())
}

/// Whether every cased character is uppercase.
///
/// Titlecasing treats an all-capitals name as shouting and recases it,
/// so a name like "a MKV" formats to "A.MKV" and then to "A.Mkv".
/// The idempotence properties pass over those names and leave that behavior to the example tests.
fn is_all_capitals(name: &str) -> bool {
    name.chars().any(char::is_alphabetic) && !name.chars().any(char::is_lowercase)
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
    fn format_name_never_panics_on_any_text(name in "\\PC{0,60}", config in config()) {
        let formatter = DotFormat::new(&config);
        let _ = formatter.format_name(&name);
        let _ = formatter.format_directory_name(&name);
    }

    #[test]
    fn format_name_is_well_formed(name in file_name(), config in config()) {
        let formatter = DotFormat::new(&config);
        let formatted = formatter.format_name(&name);
        assert_well_formed(&name, &formatted)?;
    }

    #[test]
    fn format_name_is_idempotent(name in file_name_with_one_date()) {
        let config = DotRenameConfig::default();
        let formatter = DotFormat::new(&config);
        let once = formatter.format_name(&name);
        if is_all_capitals(&once) {
            return Ok(());
        }
        let twice = formatter.format_name(&once);
        prop_assert_eq!(&twice, &once, "{:?}", name);
    }

    #[test]
    fn format_directory_name_is_idempotent(name in file_name_with_one_date()) {
        let config = DotRenameConfig::default();
        let formatter = DotFormat::new(&config);
        let once = formatter.format_directory_name(&name);
        if is_all_capitals(&once) {
            return Ok(());
        }
        let twice = formatter.format_directory_name(&once);
        prop_assert_eq!(&twice, &once, "{:?}", name);
    }
}
