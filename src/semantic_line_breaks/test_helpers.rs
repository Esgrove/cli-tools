//! Shared test helpers for the semantic line breaks modules.
//!
//! Holds the builders the prose tests use to tokenize a line, build a paragraph, and reflow it,
//! so the modules split out of the prose engine can share them.

use super::boundaries::find_boundaries;
use super::reflow::{ReflowOutcome, reflow_paragraph};
use super::tokenizer::tokenize_line;
use super::types::{FormatOptions, HardBreak, Paragraph, Rank, Token, TokenKind, ViolationKind};

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
        lines: lines.iter().map(|line| (*line).to_string()).collect(),
        hard_breaks: vec![HardBreak::None; lines.len()],
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
