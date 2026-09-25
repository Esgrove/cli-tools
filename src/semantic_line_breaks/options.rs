//! Formatting rule set and the options controlling checking and formatting.

use super::line_ranges::LineRanges;
use super::markdown::SkipNotice;
use super::violation::{Violation, ViolationKind};

/// Default maximum line width when no other source provides one.
pub const DEFAULT_MAX_WIDTH: usize = 120;

/// Default width of a tab character when measuring prefixes.
pub const DEFAULT_TAB_WIDTH: usize = 4;

/// Abbreviations that never end a sentence, lowercase and including the trailing period.
pub const DEFAULT_ABBREVIATIONS: &[&str] = &[
    "e.g.", "i.e.", "etc.", "vs.", "cf.", "approx.", "ca.", "no.", "fig.", "eq.", "dr.", "mr.", "ms.", "mrs.", "st.",
    "jr.", "sr.", "inc.", "ltd.", "co.", "prof.", "et al.", "al.", "resp.", "min.", "max.", "avg.", "incl.", "excl.",
];

/// Trailing comments starting with these prefixes are directives for other tools and are left alone.
pub const DEFAULT_DIRECTIVE_PREFIXES: &[&str] = &[
    "@ts-",
    "allow(",
    "biome-ignore",
    "clippy",
    "codespell",
    "cspell",
    "deny(",
    "eslint",
    "fmt:",
    "ktlint",
    "mypy:",
    "nolint",
    "noqa",
    "nosec",
    "nosonar",
    "pragma",
    "prettier-ignore",
    "pylint",
    "pyright:",
    "ruff:",
    "rustfmt",
    "safety",
    "shellcheck",
    "spellchecker",
    "swiftlint",
    "type:",
    "yamllint",
];

/// Words that are never capitalized when they start a new sentence after a semicolon split.
pub const DEFAULT_PRESERVE_LOWERCASE: &[&str] = &[
    "npm", "git", "cargo", "pip", "rustc", "rustfmt", "clippy", "ffmpeg", "ffprobe", "npx", "yarn", "pnpm", "brew",
    "apt", "curl", "wget", "ssh", "sudo", "bash", "zsh", "sh", "docker", "kubectl", "iOS", "macOS",
];

/// Which rules are enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleSet {
    /// Report and split over-long lines.
    pub line_too_long: bool,
    /// Report and join mid-clause breaks.
    pub mid_clause_break: bool,
    /// Report and rewrite semicolons.
    pub semicolon: bool,
    /// Report and rewrite em dashes.
    pub em_dash: bool,
    /// Report and move trailing comments.
    pub trailing_comment: bool,
    /// Report and move the quotes of a multi line Python docstring onto their own lines.
    pub docstring_quotes: bool,
}

/// Options controlling checking and formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    /// Maximum line width including indentation and comment markers.
    pub max_width: usize,
    /// Width of a tab character in prefixes.
    pub tab_width: usize,
    /// Pack consecutive short sentences onto one line while they fit.
    pub join_sentences: bool,
    /// Allow plain word boundaries even when a semantic boundary also fits.
    pub allow_word_break: bool,
    /// Treat the maximum width as a hard cap instead of allowing a small overflow past it.
    pub strict: bool,
    /// Enabled rules.
    pub rules: RuleSet,
    /// Lines to check and fix, empty for the whole text.
    ///
    /// A paragraph or comment block is processed whenever any of its lines is selected,
    /// since reflow joins and splits a paragraph as one unit.
    pub line_ranges: LineRanges,
    /// Abbreviations that never end a sentence, lowercase with the trailing period.
    pub abbreviations: Vec<String>,
    /// Extra clause starter words in addition to the built-in tiers.
    pub clause_starters: Vec<String>,
    /// Trailing comment prefixes that are left alone.
    pub directive_prefixes: Vec<String>,
    /// Words never capitalized after a semicolon split.
    pub preserve_lowercase: Vec<String>,
}

/// Result of formatting one text buffer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FormatResult {
    /// Violations found in the original text.
    pub violations: Vec<Violation>,
    /// The fixed text, or `None` when nothing changed.
    pub fixed_text: Option<String>,
    /// Where the selected lines ended up in the fixed text, empty when the whole text was processed.
    ///
    /// Reflowing a paragraph changes how many lines it needs,
    /// so the selection has to be mapped through the fix before the fixed text can be checked again.
    pub fixed_line_ranges: LineRanges,
    /// Paragraphs a heuristic kept verbatim, worth telling the user about but not a violation.
    pub skips: Vec<SkipNotice>,
}

impl RuleSet {
    /// All rules enabled.
    pub const ALL: Self = Self {
        line_too_long: true,
        mid_clause_break: true,
        semicolon: true,
        em_dash: true,
        trailing_comment: true,
        docstring_quotes: true,
    };

    /// Rules enabled when the caller asks for no particular set.
    ///
    /// Trailing comment fixing is left out.
    /// It rewrites code lines and the result often reads worse than the original,
    /// so it has to be asked for.
    pub const DEFAULT: Self = Self {
        line_too_long: true,
        mid_clause_break: true,
        semicolon: true,
        em_dash: true,
        trailing_comment: false,
        docstring_quotes: true,
    };

    /// No rules enabled.
    pub const NONE: Self = Self {
        line_too_long: false,
        mid_clause_break: false,
        semicolon: false,
        em_dash: false,
        trailing_comment: false,
        docstring_quotes: false,
    };

    /// Enable exactly the given kinds.
    #[must_use]
    pub fn from_kinds(kinds: &[ViolationKind]) -> Self {
        let mut rules = Self::NONE;
        for kind in kinds {
            match kind {
                ViolationKind::LineTooLong => rules.line_too_long = true,
                ViolationKind::MidClauseBreak => rules.mid_clause_break = true,
                ViolationKind::Semicolon => rules.semicolon = true,
                ViolationKind::EmDash => rules.em_dash = true,
                ViolationKind::TrailingComment => rules.trailing_comment = true,
                ViolationKind::DocstringQuotes => rules.docstring_quotes = true,
            }
        }
        rules
    }

    /// Whether the given kind of violation is enabled.
    #[must_use]
    pub const fn is_enabled(self, kind: ViolationKind) -> bool {
        match kind {
            ViolationKind::LineTooLong => self.line_too_long,
            ViolationKind::MidClauseBreak => self.mid_clause_break,
            ViolationKind::Semicolon => self.semicolon,
            ViolationKind::EmDash => self.em_dash,
            ViolationKind::TrailingComment => self.trailing_comment,
            ViolationKind::DocstringQuotes => self.docstring_quotes,
        }
    }
}

impl Default for RuleSet {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl FormatOptions {
    /// Options with the given maximum width and all other values at their defaults.
    #[must_use]
    pub fn with_width(max_width: usize) -> Self {
        Self {
            max_width,
            ..Self::default()
        }
    }
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_MAX_WIDTH,
            tab_width: DEFAULT_TAB_WIDTH,
            join_sentences: false,
            allow_word_break: false,
            strict: false,
            rules: RuleSet::DEFAULT,
            line_ranges: LineRanges::default(),
            abbreviations: crate::strings_from(DEFAULT_ABBREVIATIONS),
            clause_starters: Vec::new(),
            directive_prefixes: crate::strings_from(DEFAULT_DIRECTIVE_PREFIXES),
            preserve_lowercase: crate::strings_from(DEFAULT_PRESERVE_LOWERCASE),
        }
    }
}

#[cfg(test)]
mod test_rule_set {
    use super::*;

    #[test]
    fn from_kinds_enables_only_given_rules() {
        let rules = RuleSet::from_kinds(&[ViolationKind::Semicolon, ViolationKind::TrailingComment]);
        assert!(rules.semicolon);
        assert!(rules.trailing_comment);
        assert!(!rules.line_too_long);
        assert!(!rules.mid_clause_break);
        assert!(!rules.em_dash);
        assert!(!rules.docstring_quotes);
        assert!(rules.is_enabled(ViolationKind::Semicolon));
        assert!(!rules.is_enabled(ViolationKind::EmDash));
    }
}

#[cfg(test)]
mod test_format_options {
    use super::*;

    #[test]
    fn options_with_width_keep_the_other_defaults() {
        let options = FormatOptions::with_width(72);
        let defaults = FormatOptions::default();
        assert_eq!(options.max_width, 72);
        assert_eq!(options.tab_width, DEFAULT_TAB_WIDTH);
        assert_eq!(options.rules, RuleSet::DEFAULT);
        assert_eq!(options.abbreviations, defaults.abbreviations);
        assert_eq!(options.directive_prefixes, defaults.directive_prefixes);
        assert_eq!(options.preserve_lowercase, defaults.preserve_lowercase);
        assert!(options.clause_starters.is_empty());
        assert!(!options.join_sentences);
        assert!(!options.allow_word_break);
        assert_eq!(defaults.max_width, DEFAULT_MAX_WIDTH);
    }

    #[test]
    fn the_rule_set_defaults_to_every_rule_except_trailing_comments() {
        assert_eq!(RuleSet::default(), RuleSet::DEFAULT);
        assert_eq!(RuleSet::from_kinds(&[]), RuleSet::NONE);
        for kind in [
            ViolationKind::LineTooLong,
            ViolationKind::MidClauseBreak,
            ViolationKind::Semicolon,
            ViolationKind::EmDash,
            ViolationKind::TrailingComment,
            ViolationKind::DocstringQuotes,
        ] {
            assert!(RuleSet::ALL.is_enabled(kind), "{kind} should be enabled in ALL");
            assert!(!RuleSet::NONE.is_enabled(kind), "{kind} should be disabled in NONE");
            assert!(RuleSet::from_kinds(&[kind]).is_enabled(kind));
        }
    }

    #[test]
    fn the_default_rule_set_differs_from_all_rules_only_in_trailing_comments() {
        const { assert!(!RuleSet::DEFAULT.trailing_comment) };
        assert_eq!(
            RuleSet {
                trailing_comment: true,
                ..RuleSet::DEFAULT
            },
            RuleSet::ALL
        );
    }
}

#[cfg(test)]
mod test_fixture_coverage {
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn every_default_abbreviation_appears_in_a_fixture() {
        assert_fixtures_contain(DEFAULT_ABBREVIATIONS, "DEFAULT_ABBREVIATIONS");
    }

    #[test]
    fn every_default_directive_prefix_appears_in_a_fixture() {
        assert_fixtures_contain(DEFAULT_DIRECTIVE_PREFIXES, "DEFAULT_DIRECTIVE_PREFIXES");
    }

    #[test]
    fn every_word_that_stays_lowercase_follows_a_dash_in_a_fixture() {
        let after_dash: Vec<String> = DEFAULT_PRESERVE_LOWERCASE
            .iter()
            .map(|word| format!("— {word}"))
            .collect();
        let after_dash: Vec<&str> = after_dash.iter().map(String::as_str).collect();
        assert_fixtures_contain(&after_dash, "DEFAULT_PRESERVE_LOWERCASE");
    }
}
