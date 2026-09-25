//! Shared test helpers for the semantic line breaks modules.
//!
//! Holds the builders the prose tests use to tokenize a line, build a paragraph, and reflow it,
//! so the modules split out of the prose engine can share them.
//! Also reads the integration fixtures, so every module that hardcodes words or characters
//! can check that each of them is exercised by at least one fixture.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::boundaries::find_boundaries;
use super::options::FormatOptions;
use super::paragraph::{HardBreak, Paragraph};
use super::rank::Rank;
use super::reflow::{ReflowOutcome, reflow_paragraph};
use super::token::{Token, TokenKind};
use super::tokenizer::tokenize_line;
use super::violation::ViolationKind;

/// Tokenize one line with dash normalization enabled.
pub fn tokens(text: &str) -> Vec<Token<'_>> {
    tokenize_line(text, 0, true)
}

/// Token kinds of a tokenized line.
pub fn kinds(text: &str) -> Vec<TokenKind> {
    tokens(text).iter().map(|token| token.kind).collect()
}

/// Build a paragraph with the same prefix on every line.
pub fn paragraph(lines: &[&str], prefix: &str) -> Paragraph {
    Paragraph {
        start_line: 0,
        end_line: lines.len(),
        first_prefix: prefix.to_string(),
        rest_prefix: prefix.to_string(),
        last_suffix: String::new(),
        list_item: false,
        lines: lines.iter().map(|line| (*line).to_string()).collect(),
        hard_breaks: vec![HardBreak::None; lines.len()],
    }
}

/// Build a list item paragraph with the given marker prefix and the matching continuation indent.
pub fn list_item(lines: &[&str], marker: &str) -> Paragraph {
    Paragraph {
        first_prefix: marker.to_string(),
        rest_prefix: " ".repeat(marker.chars().count()),
        list_item: true,
        ..paragraph(lines, "")
    }
}

/// Reflow lines with the given width and default options.
pub fn reflow(lines: &[&str], width: usize) -> ReflowOutcome {
    reflow_paragraph(&paragraph(lines, ""), &FormatOptions::with_width(width), true)
}

/// Violation kinds and fixability of an outcome.
pub fn summary(outcome: &ReflowOutcome) -> Vec<(ViolationKind, bool)> {
    outcome
        .violations
        .iter()
        .map(|violation| (violation.kind, violation.fixable))
        .collect()
}

/// Rank of the boundary before the token at `index`.
pub fn rank_before(text: &str, index: usize) -> Option<Rank> {
    find_boundaries(&tokens(text), &FormatOptions::default())
        .into_iter()
        .find(|boundary| boundary.before == index)
        .map(|boundary| boundary.rank)
}

/// Text of every input fixture the integration tests format, lowercased and joined with newlines.
///
/// A hardcoded word or character that none of the fixtures holds is never exercised end to end,
/// so the coverage tests of the modules that own such lists check them against this text.
pub fn fixture_inputs() -> &'static str {
    static INPUTS: LazyLock<String> = LazyLock::new(|| {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("slb");
        let mut paths: Vec<PathBuf> = fs::read_dir(&directory)
            .expect("the fixture directory should exist")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".in."))
            })
            .collect();
        paths.sort();
        paths
            .iter()
            .map(|path| fs::read_to_string(path).expect("the fixture should be readable"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase()
    });
    &INPUTS
}

/// Assert that every word appears as a whole word somewhere in the input fixtures, ignoring case.
pub fn assert_fixtures_contain(words: &[&str], list_name: &str) {
    let inputs = fixture_inputs();
    let missing: Vec<&str> = words
        .iter()
        .copied()
        .filter(|word| !contains_whole_word(inputs, &word.to_lowercase()))
        .collect();
    assert!(
        missing.is_empty(),
        "{list_name} entries missing from the slb integration fixtures: {missing:?}"
    );
}

/// Assert that every word starts a line of some input fixture, ignoring case and indentation.
pub fn assert_fixture_lines_start_with(words: &[&str], list_name: &str) {
    let missing: Vec<&str> = words
        .iter()
        .copied()
        .filter(|word| {
            let word = word.to_lowercase();
            !fixture_inputs()
                .lines()
                .any(|line| starts_with_whole_word(line.trim_start(), &word))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "{list_name} entries that start no line of the slb integration fixtures: {missing:?}"
    );
}

/// Assert that every word ends a line of some input fixture, ignoring case and trailing whitespace.
pub fn assert_fixture_lines_end_with(words: &[&str], list_name: &str) {
    let missing: Vec<&str> = words
        .iter()
        .copied()
        .filter(|word| {
            let word = word.to_lowercase();
            !fixture_inputs().lines().any(|line| {
                let line = line.trim_end();
                line.strip_suffix(word.as_str())
                    .is_some_and(|before| !before.chars().next_back().is_some_and(char::is_alphanumeric))
            })
        })
        .collect();
    assert!(
        missing.is_empty(),
        "{list_name} entries that end no line of the slb integration fixtures: {missing:?}"
    );
}

/// Assert that every character appears somewhere in the input fixtures.
pub fn assert_fixtures_contain_characters(characters: &[char], set_name: &str) {
    let inputs = fixture_inputs();
    let missing: Vec<char> = characters
        .iter()
        .copied()
        .filter(|character| !inputs.contains(*character))
        .collect();
    assert!(
        missing.is_empty(),
        "{set_name} characters missing from the slb integration fixtures: {missing:?}"
    );
}

/// Whether the needle occurs in the text with no letter or digit glued to either end of it.
///
/// An end of the needle that is punctuation already delimits itself, so only a letter or digit end is checked.
fn contains_whole_word(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(start, _)| {
        let before = text.get(..start).and_then(|head| head.chars().next_back());
        let after = text.get(start + needle.len()..).and_then(|tail| tail.chars().next());
        delimited(needle, before, after)
    })
}

/// Whether the line starts with the needle with no letter or digit glued to its end.
fn starts_with_whole_word(line: &str, needle: &str) -> bool {
    line.strip_prefix(needle)
        .is_some_and(|rest| delimited(needle, None, rest.chars().next()))
}

/// Whether the characters around a match leave a needle that ends in a letter or digit standing on its own.
fn delimited(needle: &str, before: Option<char>, after: Option<char>) -> bool {
    let glued = |neighbour: Option<char>, end: Option<char>| {
        end.is_some_and(char::is_alphanumeric) && neighbour.is_some_and(char::is_alphanumeric)
    };
    !glued(before, needle.chars().next()) && !glued(after, needle.chars().next_back())
}
