//! String and comment syntax of every file kind the semantic line breaks formatter understands.
//!
//! Holds the quote characters, raw and triple quoted string forms, comment markers,
//! and regex literal support of each language,
//! which is what the scanner needs to tell a comment apart from a string.

use cli_tools_common::simd::ByteSet;

use super::file_kind::FileKind;
use super::scanner::{Backtick, SingleQuote};

/// String and comment syntax of a language for the trailing comment scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringSyntax {
    /// Line comment marker.
    pub(super) line_marker: &'static str,
    /// Block comment delimiters.
    pub(super) block_comment: Option<(&'static str, &'static str)>,
    /// Whether block comments nest.
    pub(super) nested_block_comments: bool,
    /// Whether double quotes delimit strings.
    pub(super) double_quote: bool,
    /// Single quote behaviour.
    pub(super) single_quote: SingleQuote,
    /// Backtick behaviour.
    pub(super) backtick: Backtick,
    /// Whether triple quotes delimit multi-line strings.
    pub(super) triple_quotes: bool,
    /// Whether Rust style raw strings exist.
    pub(super) rust_raw_strings: bool,
    /// Whether backslash escapes quotes inside double quoted strings.
    pub(super) backslash_escapes: bool,
    /// Whether backslash escapes quotes inside single quoted strings.
    pub(super) single_quote_escapes: bool,
    /// Whether a doubled quote escapes itself inside a string.
    pub(super) doubled_quote_escape: bool,
    /// Whether plain double quoted strings may span lines.
    pub(super) multiline_strings: bool,
    /// Whether the comment marker only counts after whitespace.
    pub(super) marker_needs_leading_space: bool,
    /// Whether shell heredocs exist.
    pub(super) heredoc: bool,
    /// Whether YAML block scalars exist.
    pub(super) block_scalars: bool,
    /// The trigger bytes as a precomputed set, see [`StringSyntax::trigger_bytes`].
    pub(super) trigger_set: ByteSet,
    /// Bytes that can change the state of the scanner in code,
    /// which are the trigger bytes and the `r` that starts a Rust raw string.
    pub(super) normal_set: ByteSet,
    /// Bytes that can close, or for nesting languages open, a block comment.
    pub(super) block_comment_set: Option<ByteSet>,
}

impl StringSyntax {
    /// Bytes that can start a comment, string, heredoc, or regex literal in code.
    ///
    /// A line in code without any of them is code to its end, so the scanner can skip it.
    /// Bytes a language does not use are replaced with a slash, which is always part of the set.
    pub(super) const fn trigger_bytes(&self) -> [u8; 7] {
        let block_open = match self.block_comment {
            Some((open, _)) => first_byte_or_slash(open),
            None => b'/',
        };
        [
            first_byte_or_slash(self.line_marker),
            block_open,
            b'"',
            b'\'',
            if matches!(self.backtick, Backtick::None) {
                b'/'
            } else {
                b'`'
            },
            if self.heredoc { b'<' } else { b'/' },
            b'/',
        ]
    }

    /// The syntax with its byte sets computed from the other fields.
    const fn with_byte_sets(self) -> Self {
        let triggers = self.trigger_bytes();
        let [marker, block, double, single, backtick, heredoc, slash] = triggers;
        let raw = if self.rust_raw_strings { b'r' } else { slash };
        let block_comment_set = match self.block_comment {
            Some((open, close)) => {
                let close = first_byte_or_slash(close);
                let open = if self.nested_block_comments {
                    first_byte_or_slash(open)
                } else {
                    close
                };
                Some(ByteSet::new(&[close, open]))
            }
            None => None,
        };
        Self {
            trigger_set: ByteSet::new(&triggers),
            normal_set: ByteSet::new(&[marker, block, double, single, backtick, heredoc, slash, raw]),
            block_comment_set,
            ..self
        }
    }
}

/// First byte of the text, or a slash for empty text.
pub(super) const fn first_byte_or_slash(text: &str) -> u8 {
    match text.as_bytes().first() {
        Some(byte) => *byte,
        None => b'/',
    }
}

/// String syntax for languages the trailing comment scanner supports.
pub(super) const fn string_syntax(kind: FileKind) -> Option<StringSyntax> {
    let base = StringSyntax {
        line_marker: "//",
        block_comment: Some(("/*", "*/")),
        nested_block_comments: false,
        double_quote: true,
        single_quote: SingleQuote::CharLiteral,
        backtick: Backtick::None,
        triple_quotes: false,
        rust_raw_strings: false,
        backslash_escapes: true,
        single_quote_escapes: true,
        doubled_quote_escape: false,
        multiline_strings: false,
        marker_needs_leading_space: false,
        heredoc: false,
        block_scalars: false,
        trigger_set: ByteSet::new(b""),
        normal_set: ByteSet::new(b""),
        block_comment_set: None,
    };
    let syntax = match kind {
        FileKind::Rust => StringSyntax {
            nested_block_comments: true,
            rust_raw_strings: true,
            multiline_strings: true,
            ..base
        },
        FileKind::CLike => StringSyntax {
            triple_quotes: true,
            ..base
        },
        FileKind::JavaScript => StringSyntax {
            single_quote: SingleQuote::String,
            backtick: Backtick::Template,
            ..base
        },
        FileKind::Go => StringSyntax {
            backtick: Backtick::RawString,
            ..base
        },
        FileKind::Groovy => StringSyntax {
            single_quote: SingleQuote::String,
            triple_quotes: true,
            ..base
        },
        FileKind::Python => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            triple_quotes: true,
            ..base
        },
        FileKind::Shell => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            single_quote_escapes: false,
            marker_needs_leading_space: true,
            heredoc: true,
            ..base
        },
        FileKind::Toml => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            single_quote_escapes: false,
            triple_quotes: true,
            ..base
        },
        FileKind::Yaml => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            single_quote_escapes: false,
            doubled_quote_escape: true,
            marker_needs_leading_space: true,
            block_scalars: true,
            ..base
        },
        FileKind::Dockerfile
        | FileKind::Makefile
        | FileKind::CMake
        | FileKind::Ruby
        | FileKind::Sql
        | FileKind::Lua
        | FileKind::Markdown => return None,
    };
    Some(syntax.with_byte_sets())
}
