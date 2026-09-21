//! Semantic line breaks formatter for prose in comments, docstrings, and Markdown.
//!
//! The `slb` binary uses this module to detect lines that violate the semantic line break style
//! and to reflow prose so that lines break at sentence and clause boundaries,
//! stay within the configured width, and avoid semicolons, em dashes, and trailing comments.
//! The public entry points are [`check`] and [`format`].

pub mod boundaries;
pub mod comments;
pub mod docstrings;
pub mod file_kind;
pub mod formatter;
pub mod line_breaks;
pub mod line_ranges;
pub mod list_runs;
pub mod looks_like_code;
pub mod markdown;
pub mod options;
pub mod paragraph;
pub mod project_config;
pub mod rank;
pub mod reflow;
pub mod regex_literals;
pub mod rewording;
pub mod scanner;
pub mod string_syntax;
pub mod token;
pub mod tokenizer;
pub mod violation;

#[cfg(test)]
pub(crate) mod test_helpers;

pub use file_kind::FileKind;
pub use formatter::{check, format};
pub use line_ranges::LineRanges;
pub use markdown::SkipNotice;
pub use options::{FormatOptions, FormatResult, RuleSet};
pub use paragraph::{HardBreak, Paragraph, Region};
pub use rank::Rank;
pub use token::{Token, TokenKind};
pub use violation::{Violation, ViolationKind};
