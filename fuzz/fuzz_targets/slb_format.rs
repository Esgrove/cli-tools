//! Fuzz `slb` checking and formatting.
//!
//! The first three bytes choose the file kind, the line width, and the rules and layout switches,
//! and the rest is the text, decoded lossily so every input reaches the formatter.
//! Formatting must not panic and must report the same violations as checking at every width.
//! From 40 columns up, where lines hold more than a word or two,
//! formatting must also reach a fixed point within two runs and leave no fixable violation there,
//! matching the property tests in `crates/slb/tests/slb_property_tests.rs`.

#![no_main]

use clap::ValueEnum;
use cli_tools_slb::{FileKind, FormatOptions, RuleSet, check, format};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [kind_byte, width_byte, switches, text @ ..] = data else {
        return;
    };
    let kinds = FileKind::value_variants();
    let Some(&kind) = kinds.get(usize::from(*kind_byte) % kinds.len()) else {
        return;
    };
    let text = String::from_utf8_lossy(text);
    let bit = |index: u8| switches & (1 << index) != 0;
    let options = FormatOptions {
        max_width: usize::from(*width_byte),
        join_sentences: bit(6),
        allow_word_break: bit(7),
        rules: RuleSet {
            line_too_long: bit(0),
            mid_clause_break: bit(1),
            semicolon: bit(2),
            em_dash: bit(3),
            trailing_comment: bit(4),
            docstring_quotes: bit(5),
        },
        ..FormatOptions::default()
    };

    let formatted = format(&text, kind, &options);
    assert_eq!(check(&text, kind, &options).violations, formatted.violations);
    if options.max_width < 40 {
        return;
    }
    let once = formatted.fixed_text.unwrap_or_else(|| text.to_string());
    let twice = format(&once, kind, &options).fixed_text.unwrap_or_else(|| once.clone());
    let third = format(&twice, kind, &options);
    assert_eq!(
        third.fixed_text, None,
        "formatting {text:?} did not settle within two runs"
    );
    assert!(
        third.violations.iter().all(|violation| !violation.fixable),
        "fixable violations remain after formatting {text:?}"
    );
});
