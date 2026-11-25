//! Property tests for the shared text helpers.
//!
//! These check `collapse_repeated_separators` against a plain character loop,
//! and the date reordering helpers for panics and for output that is already in the target format.
//! Set `PROPTEST_CASES` to run more cases than the default.

use cli_tools_common::collapse_repeated_separators;
use cli_tools_common::date::Date;
use proptest::prelude::*;

/// Whether the character is a separator for `collapse_repeated_separators`.
const fn is_separator(character: char) -> bool {
    matches!(character, '.' | '-' | '_') || character.is_whitespace()
}

/// Reference implementation that keeps the first separator of every run and trims separators from both ends.
fn collapse_reference(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut previous_was_separator = false;
    for character in text.chars() {
        let separator = is_separator(character);
        if !(separator && previous_was_separator) {
            collapsed.push(character);
        }
        previous_was_separator = separator;
    }
    collapsed.trim_matches(is_separator).to_string()
}

/// Filename text made of words, separators, and Unicode whitespace.
fn separator_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            prop::sample::select(vec![".", "-", "_", " ", "\t", "\u{a0}", "\u{3000}", "\n"]),
            prop::sample::select(vec!["Movie", "x265", "é", "😀", "a", "2024"]),
        ],
        0..30,
    )
    .prop_map(|parts| parts.concat())
}

/// One or two digit number as text, sometimes zero padded.
fn short_number(maximum: u32) -> impl Strategy<Value = String> {
    (1..=maximum, any::<bool>()).prop_map(|(value, padded)| {
        if padded {
            format!("{value:02}")
        } else {
            value.to_string()
        }
    })
}

/// Filename with a date in one of the supported layouts surrounded by text.
fn dated_filename() -> impl Strategy<Value = String> {
    let date = prop_oneof![
        (short_number(31), short_number(12), 1990..2030_i32)
            .prop_map(|(day, month, year)| format!("{day}.{month}.{year}")),
        (short_number(12), 13..=31_u32, 1990..2030_i32).prop_map(|(month, day, year)| format!("{month}.{day}.{year}")),
        (short_number(31), short_number(12), 0..100_u32)
            .prop_map(|(day, month, year)| format!("{day}.{month}.{year:02}")),
        (0..100_u32, short_number(12), short_number(31))
            .prop_map(|(year, month, day)| format!("{year:02}.{month}.{day}")),
        (1990..2030_i32, short_number(12), short_number(28))
            .prop_map(|(year, month, day)| format!("{year}.{month}.{day}")),
    ];
    let text = "[A-Za-z]{0,6}";
    let separator = prop::sample::select(vec![".", " ", "_", "-", ""]);
    (text, separator.clone(), date, separator, text)
        .prop_map(|(before, left, date, right, after)| format!("{before}{left}{date}{right}{after}"))
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
    fn collapse_matches_reference(text in separator_text()) {
        prop_assert_eq!(collapse_repeated_separators(&text), collapse_reference(&text));
    }

    #[test]
    fn collapse_matches_reference_for_any_text(text in "\\PC{0,80}") {
        prop_assert_eq!(collapse_repeated_separators(&text), collapse_reference(&text));
    }

    #[test]
    fn collapse_is_idempotent(text in separator_text()) {
        let once = collapse_repeated_separators(&text);
        prop_assert_eq!(collapse_repeated_separators(&once), once);
    }

    #[test]
    fn reorder_filename_date_never_panics(text in "\\PC{0,60}", year_first: bool, swap_year: bool) {
        let _ = Date::reorder_filename_date(&text, year_first, swap_year, false);
    }

    #[test]
    fn reorder_filename_date_output_is_already_ordered(name in dated_filename(), year_first: bool) {
        if let Some(reordered) = Date::reorder_filename_date(&name, year_first, false, false) {
            prop_assert_eq!(
                Date::reorder_filename_date(&reordered, year_first, false, false),
                None,
                "{} became {}",
                name,
                reordered
            );
        }
    }

    #[test]
    fn reorder_directory_date_starts_with_a_dash_date(name in dated_filename()) {
        if let Some(reordered) = Date::reorder_directory_date(&name) {
            let date = reordered.get(..10).unwrap_or_default();
            prop_assert_eq!(date.split('-').count(), 3, "{} became {}", name, reordered);
        }
    }

    #[test]
    fn reorder_directory_date_never_panics(text in "\\PC{0,60}") {
        let _ = Date::reorder_directory_date(&text);
    }
}
