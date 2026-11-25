//! Fuzz the shared date reordering and separator collapsing helpers.
//!
//! The input is decoded lossily into a file name.
//! Date reordering must not panic, and separator collapsing must match a plain character loop
//! and be idempotent.

#![no_main]

use cli_tools_common::collapse_repeated_separators;
use cli_tools_common::date::Date;
use libfuzzer_sys::fuzz_target;

/// Whether the character is a separator for `collapse_repeated_separators`.
fn is_separator(character: char) -> bool {
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

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data);
    for year_first in [false, true] {
        for swap_year in [false, true] {
            let _ = Date::reorder_filename_date(&name, year_first, swap_year, false);
        }
    }
    let _ = Date::reorder_directory_date(&name);
    let _ = Date::replace_file_date_with_directory_date(&name);

    let collapsed = collapse_repeated_separators(&name);
    assert_eq!(collapsed, collapse_reference(&name), "{name:?}");
    assert_eq!(collapse_repeated_separators(&collapsed), collapsed, "{name:?}");
});
