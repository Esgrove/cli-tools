//! Violation categories and the single violation record reported by the checker.

use std::borrow::Cow;
use std::fmt;

use clap::ValueEnum;

/// Rule category of a violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, ValueEnum)]
pub enum ViolationKind {
    /// A line exceeds the maximum width.
    #[value(name = "too-long")]
    LineTooLong,
    /// A line ends in the middle of a clause and continues on the next line.
    #[value(name = "mid-clause")]
    MidClauseBreak,
    /// Prose uses a semicolon to join clauses.
    #[value(name = "semicolon")]
    Semicolon,
    /// Prose uses an em dash or a double hyphen.
    #[value(name = "em-dash")]
    EmDash,
    /// A comment shares a line with code.
    #[value(name = "trailing")]
    TrailingComment,
    /// Prose shares a line with the quotes of a multi line Python docstring.
    #[value(name = "docstring")]
    DocstringQuotes,
}

/// A single violation of the prose style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// One based line number in the original text.
    pub line: usize,
    /// One based character column when known.
    pub column: Option<usize>,
    /// Rule category.
    pub kind: ViolationKind,
    /// Human readable explanation.
    ///
    /// Most messages are fixed text, so they are borrowed rather than allocated per violation.
    pub message: Cow<'static, str>,
    /// Whether fix mode can repair this violation.
    pub fixable: bool,
}

impl ViolationKind {
    /// Short name used in reports and in the rules option.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::LineTooLong => "too-long",
            Self::MidClauseBreak => "mid-clause",
            Self::Semicolon => "semicolon",
            Self::EmDash => "em-dash",
            Self::TrailingComment => "trailing",
            Self::DocstringQuotes => "docstring",
        }
    }
}

impl fmt::Display for ViolationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.name())
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.column {
            Some(column) => write!(formatter, "{}:{}: {}: {}", self.line, column, self.kind, self.message),
            None => write!(formatter, "{}: {}: {}", self.line, self.kind, self.message),
        }
    }
}

#[cfg(test)]
mod test_violation_kind {
    use super::*;

    #[test]
    fn names_and_display_match_the_rules_option() {
        let expected = [
            (ViolationKind::LineTooLong, "too-long"),
            (ViolationKind::MidClauseBreak, "mid-clause"),
            (ViolationKind::Semicolon, "semicolon"),
            (ViolationKind::EmDash, "em-dash"),
            (ViolationKind::TrailingComment, "trailing"),
            (ViolationKind::DocstringQuotes, "docstring"),
        ];
        for (kind, name) in expected {
            assert_eq!(kind.name(), name);
            assert_eq!(kind.to_string(), name);
        }
    }

    #[test]
    fn violations_display_with_and_without_a_column() {
        let violation = Violation {
            line: 12,
            column: None,
            kind: ViolationKind::Semicolon,
            message: "semicolon joins clauses".into(),
            fixable: false,
        };
        assert_eq!(violation.to_string(), "12: semicolon: semicolon joins clauses");

        let with_column = Violation {
            column: Some(5),
            ..violation
        };
        assert_eq!(with_column.to_string(), "12:5: semicolon: semicolon joins clauses");
    }
}
