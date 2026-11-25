//! Fuzz the `dots` name formatter.
//!
//! The first byte toggles case conversion, the year first date order, and a prefix and suffix,
//! and the rest is the name, decoded lossily.
//! Formatting must not panic and must never produce repeated, leading, or trailing dots.

#![no_main]

use cli_tools_dots::{DotFormat, DotRenameConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [switches, name @ ..] = data else {
        return;
    };
    let name = String::from_utf8_lossy(name);
    let bit = |index: u8| switches & (1 << index) != 0;
    let config = DotRenameConfig {
        convert_case: bit(0),
        date_starts_with_year: bit(1),
        prefix: bit(2).then(|| "Show".to_string()),
        suffix: bit(3).then(|| "Extra".to_string()),
        move_date_after_prefix: if bit(4) { vec!["Show".to_string()] } else { Vec::new() },
        ..DotRenameConfig::default()
    };
    let formatter = DotFormat::new(&config);

    let formatted = formatter.format_name(&name);
    assert!(!formatted.contains(".."), "{name:?} became {formatted:?}");
    assert!(!formatted.starts_with('.'), "{name:?} became {formatted:?}");
    assert!(!formatted.ends_with('.'), "{name:?} became {formatted:?}");
    let _ = formatter.format_directory_name(&name);
});
