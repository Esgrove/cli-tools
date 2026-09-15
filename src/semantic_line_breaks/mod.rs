//! Semantic line breaks formatter for prose in comments, docstrings, and Markdown.
//!
//! The `slb` binary uses this module to detect lines that violate the semantic line break style
//! and to reflow prose so that lines break at sentence and clause boundaries,
//! stay within the configured width, and avoid semicolons, em dashes, and trailing comments.
//! The public entry points are [`check`] and [`format`].

pub mod boundaries;
pub mod comments;
pub mod formatter;
pub mod line_breaks;
pub mod markdown;
pub mod project_config;
pub mod reflow;
pub mod regex_literals;
pub mod rewording;
pub mod scanner;
pub mod string_syntax;
pub mod tokenizer;
pub mod types;

#[cfg(test)]
pub(crate) mod test_helpers;

pub use formatter::{check, format};
pub use types::{
    FileKind, FormatOptions, FormatResult, HardBreak, Paragraph, Rank, Region, RuleSet, Token, TokenKind, Violation,
    ViolationKind,
};
