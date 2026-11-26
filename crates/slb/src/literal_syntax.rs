//! Recognition of line-oriented literals in shell scripts and Makefiles.
//!
//! These constructs protect lines that can look like comments but contribute to a runtime value.

use std::sync::LazyLock;

use regex::Regex;

/// Whitespace the language permits before a heredoc terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeredocIndent {
    /// The delimiter must begin in column one.
    Exact,
    /// Strip leading tabs, as in shell `<<-`.
    Tabs,
    /// Strip leading whitespace, as in Ruby `<<-` and `<<~`.
    Whitespace,
}

/// A heredoc delimiter after removing its quotes, together with its indentation rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heredoc {
    word: String,
    indent: HeredocIndent,
}

/// Matches one complete, simply quoted or bare heredoc delimiter.
static RE_HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^<<(?P<indent>[-~]?)[ \t]*(?:'(?P<single>[^']*)'|"(?P<double>[^"\\$`]*)"|(?P<bare>[\w.:-]+))(?:$|[ \t;|&<>()])"#)
        .expect("Invalid heredoc regex")
});

/// A Make directive that opens a variable definition, optionally with assignment modifiers.
static RE_MAKE_DEFINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:override|export|private)[ \t]+)*define[ \t]+[^\s=:#]").expect("Invalid Make define regex")
});

impl Heredoc {
    /// Recognize a heredoc delimiter and select the indentation rule for the language.
    pub(super) fn parse(rest: &str, ruby: bool) -> Option<Self> {
        let parts = RE_HEREDOC.captures(rest)?;
        let indent = match (parts.name("indent")?.as_str(), ruby) {
            ("-" | "~", true) => HeredocIndent::Whitespace,
            ("-", false) => HeredocIndent::Tabs,
            ("", _) => HeredocIndent::Exact,
            _ => return None,
        };
        let word = parts
            .name("single")
            .or_else(|| parts.name("double"))
            .or_else(|| parts.name("bare"))?;
        Some(Self {
            word: word.as_str().to_owned(),
            indent,
        })
    }

    /// Whether a line is the actual terminator, respecting its whitespace exactly.
    pub(super) fn closes(&self, line: &str) -> bool {
        let candidate = match self.indent {
            HeredocIndent::Exact => line,
            HeredocIndent::Tabs => line.trim_start_matches('\t'),
            HeredocIndent::Whitespace => line.trim_start(),
        };
        candidate == self.word
    }
}

/// Whether a Make line is part of a definition and must stay verbatim.
///
/// GNU Make counts nested directives, but a tab-prefixed line is part of the value, not a directive.
pub fn inside_make_definition(line: &str, depth: &mut usize) -> bool {
    let already_inside = *depth > 0;
    if !line.starts_with('\t') {
        let trimmed = line.trim_start();
        if RE_MAKE_DEFINE.is_match(trimmed) {
            *depth += 1;
        } else if trimmed.strip_prefix("endef").is_some_and(|rest| {
            let rest = rest.trim_start();
            rest.is_empty() || rest.starts_with('#')
        }) {
            *depth = depth.saturating_sub(1);
        }
    }
    already_inside || *depth > 0
}
