//! Shared types for the semantic line breaks formatter.
//!
//! Contains the file kind mapping, comment syntax descriptions, violation and token types,
//! paragraph and region representations, and the formatting options used by the prose engine, the block splitters,
//! and the `slb` binary.

use std::borrow::Cow;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::Path;
use std::str::FromStr;

use clap::ValueEnum;

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
    "clippy",
    "rustfmt",
    "fmt:",
    "noqa",
    "type:",
    "pyright:",
    "pylint",
    "ruff:",
    "mypy:",
    "nosec",
    "safety",
    "nolint",
    "eslint",
    "prettier-ignore",
    "@ts-",
    "biome-ignore",
    "pragma",
    "shellcheck",
    "yamllint",
    "nosonar",
    "swiftlint",
    "ktlint",
    "cspell",
    "codespell",
    "allow(",
    "deny(",
    "spellchecker",
];

/// Words that are never capitalized when they start a new sentence after a semicolon split.
pub const DEFAULT_PRESERVE_LOWERCASE: &[&str] = &[
    "npm", "git", "cargo", "pip", "rustc", "rustfmt", "clippy", "ffmpeg", "ffprobe", "npx", "yarn", "pnpm", "brew",
    "apt", "curl", "wget", "ssh", "sudo", "bash", "zsh", "sh", "docker", "kubectl", "iOS", "macOS",
];

/// Kind of a file, deciding comment markers and string syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum FileKind {
    /// Rust source with `//`, `///`, and `//!` comments.
    Rust,
    /// C, C++, Java, Kotlin, Swift, C#, and similar languages with `//` and `/* */` comments.
    #[value(name = "c")]
    CLike,
    /// JavaScript and TypeScript with template literals.
    #[value(name = "javascript")]
    JavaScript,
    /// Go source with backtick raw strings.
    Go,
    /// Python source with `#` comments and docstrings.
    Python,
    /// Shell scripts with `#` comments.
    Shell,
    /// TOML files with `#` comments.
    Toml,
    /// YAML files with `#` comments that need a leading space.
    Yaml,
    /// Dockerfiles with `#` comments.
    Dockerfile,
    /// Makefiles with `#` comments.
    Makefile,
    /// Ruby source with `#` comments.
    Ruby,
    /// SQL files with `--` comments.
    Sql,
    /// Lua source with `--` comments.
    Lua,
    /// Markdown documents.
    Markdown,
}

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
}

/// Ranking of a break candidate. A higher rank is a better place to break a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// A plain word boundary, only used as a last resort.
    Word,
    /// Before a relative pronoun or a configured extra clause starter.
    ClauseTier4,
    /// Before a subordinating conjunction such as "because" or "while".
    ClauseTier3,
    /// After a comma or dash.
    Punctuation,
    /// Before a coordinating conjunction such as "but" or "or".
    ClauseTier2,
    /// Before "and".
    ClauseTier1,
    /// After a colon introducing an explanation or list.
    Colon,
    /// After the end of a sentence.
    Sentence,
    /// A break the formatter inserted itself, for example after a semicolon rewrite.
    Forced,
}

/// Kind of a prose token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An ordinary word.
    Word,
    /// A backtick delimited code span.
    Code,
    /// A URL.
    Url,
    /// A file system path.
    Path,
    /// A version number such as "1.2.3".
    Version,
    /// A plain number.
    Number,
    /// An identifier such as `snake_case`, `camelCase`, or `a::b`.
    Identifier,
    /// A Markdown link or image.
    Link,
    /// An inline HTML tag.
    Html,
    /// A standalone em dash, en dash, or double hyphen.
    Dash,
}

/// Intentional hard line break marker at the end of a Markdown line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardBreak {
    /// No hard break.
    #[default]
    None,
    /// Two or more trailing spaces.
    Spaces,
    /// A trailing backslash.
    Backslash,
}

/// A region of a text buffer, either copied verbatim or reflowed as a prose paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Region {
    /// Lines copied without changes, given as a zero based half open line index range.
    Verbatim {
        /// First line index.
        start: usize,
        /// One past the last line index.
        end: usize,
    },
    /// A paragraph of prose that may be reflowed.
    Paragraph(Paragraph),
}

/// Block comment syntax of a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCommentStyle {
    /// Opening delimiter, for example `/*`.
    pub open: &'static str,
    /// Closing delimiter, for example `*/`.
    pub close: &'static str,
    /// Marker for continuation lines inside the block, for example `*`.
    pub continuation: &'static str,
}

/// Comment syntax of a file kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentStyle {
    /// Line comment markers, longest first so `//!` and `///` win over `//`.
    pub line_markers: &'static [&'static str],
    /// Block comment delimiters when the language has them.
    pub block: Option<BlockCommentStyle>,
    /// Whether triple quoted docstrings are prose blocks.
    pub docstrings: bool,
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

/// A prose token: a word or an unbreakable atom with surrounding punctuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'text> {
    /// Opening punctuation such as `(` or `"`.
    pub leading: Cow<'text, str>,
    /// The word or atom body.
    pub core: Cow<'text, str>,
    /// Closing punctuation such as `.` or `,)`.
    pub trailing: Cow<'text, str>,
    /// Token classification.
    pub kind: TokenKind,
    /// Index of the paragraph line this token came from.
    pub origin_line: usize,
    /// Whether the formatter inserted a forced break after this token.
    pub force_break_after: bool,
    /// Width in characters, kept in step by the methods that change the text.
    width: u32,
}

/// A run of prose lines sharing one prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    /// Zero based index of the first line in the text buffer.
    pub start_line: usize,
    /// One past the zero based index of the last line.
    pub end_line: usize,
    /// Prefix of the first line, for example `    /// - `.
    pub first_prefix: String,
    /// Prefix of every following line, for example `    ///   `.
    pub rest_prefix: String,
    /// Suffix appended to the last line, for example closing docstring quotes.
    pub last_suffix: String,
    /// Content lines with prefixes stripped and trailing whitespace removed.
    pub lines: Vec<String>,
    /// Hard break marker of each content line.
    pub hard_breaks: Vec<HardBreak>,
}

/// The lines a run is limited to.
///
/// An empty set selects every line,
/// so a run without a line selection needs no special handling at the places that consult it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineRanges {
    /// One based inclusive ranges, sorted and merged so no two of them touch or overlap.
    ranges: Vec<RangeInclusive<usize>>,
}

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
    /// Allow breaking at a plain word boundary when no clause boundary fits.
    pub allow_word_break: bool,
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
}

impl FileKind {
    /// Detect the file kind from the file extension or well known file name.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        match file_name {
            "Dockerfile" | "Containerfile" => return Some(Self::Dockerfile),
            "Makefile" | "makefile" | "GNUmakefile" => return Some(Self::Makefile),
            "Gemfile" | "Rakefile" => return Some(Self::Ruby),
            ".bashrc" | ".zshrc" | ".zshenv" | ".zprofile" | ".profile" | ".bash_profile" => return Some(Self::Shell),
            _ => {}
        }
        let extension = path.extension().and_then(|ext| ext.to_str())?.to_ascii_lowercase();
        Self::from_extension(&extension)
    }

    /// Detect the file kind from a file extension without the leading dot.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let kind = match extension.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" | "java" | "kt" | "kts" | "swift" | "cs"
            | "scala" | "dart" | "m" | "mm" => Self::CLike,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Self::JavaScript,
            "go" => Self::Go,
            "py" | "pyi" => Self::Python,
            "sh" | "bash" | "zsh" | "fish" => Self::Shell,
            "toml" => Self::Toml,
            "yml" | "yaml" => Self::Yaml,
            "dockerfile" => Self::Dockerfile,
            "mk" => Self::Makefile,
            "rb" => Self::Ruby,
            "sql" => Self::Sql,
            "lua" => Self::Lua,
            "md" | "markdown" => Self::Markdown,
            _ => return None,
        };
        Some(kind)
    }

    /// Comment syntax of this file kind.
    #[must_use]
    pub const fn comment_style(self) -> CommentStyle {
        const C_BLOCK: BlockCommentStyle = BlockCommentStyle {
            open: "/*",
            close: "*/",
            continuation: "*",
        };
        match self {
            Self::Rust => CommentStyle {
                line_markers: &["//!", "///", "//"],
                block: Some(C_BLOCK),
                docstrings: false,
            },
            Self::CLike | Self::JavaScript | Self::Go => CommentStyle {
                line_markers: &["///", "//"],
                block: Some(C_BLOCK),
                docstrings: false,
            },
            Self::Python => CommentStyle {
                line_markers: &["#"],
                block: None,
                docstrings: true,
            },
            Self::Shell | Self::Toml | Self::Yaml | Self::Dockerfile | Self::Makefile | Self::Ruby => CommentStyle {
                line_markers: &["#"],
                block: None,
                docstrings: false,
            },
            Self::Sql | Self::Lua => CommentStyle {
                line_markers: &["--"],
                block: None,
                docstrings: false,
            },
            Self::Markdown => CommentStyle {
                line_markers: &[],
                block: None,
                docstrings: false,
            },
        }
    }

    /// Whether the trailing comment scanner understands this language well enough to run.
    #[must_use]
    pub const fn supports_trailing_comment_check(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::CLike
                | Self::JavaScript
                | Self::Go
                | Self::Python
                | Self::Shell
                | Self::Toml
                | Self::Yaml
        )
    }
}

impl Rank {
    /// The next lower ranking.
    ///
    /// Used to penalize a break candidate whose line does not fit the budget,
    /// so an overflowing break has to be clearly better than one that fits.
    #[must_use]
    pub const fn lowered(self) -> Self {
        match self {
            Self::Word | Self::ClauseTier4 => Self::Word,
            Self::ClauseTier3 => Self::ClauseTier4,
            Self::Punctuation => Self::ClauseTier3,
            Self::ClauseTier2 => Self::Punctuation,
            Self::ClauseTier1 => Self::ClauseTier2,
            Self::Colon => Self::ClauseTier1,
            Self::Sentence => Self::Colon,
            Self::Forced => Self::Sentence,
        }
    }
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
        }
    }
}

impl HardBreak {
    /// Marker text appended to the end of a line.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Spaces => "  ",
            Self::Backslash => "\\",
        }
    }
}

impl<'text> Token<'text> {
    /// Create a token from the parts of one chunk of a line.
    #[must_use]
    pub fn new(
        leading: Cow<'text, str>,
        core: Cow<'text, str>,
        trailing: Cow<'text, str>,
        kind: TokenKind,
        origin_line: usize,
    ) -> Self {
        let width = token_width(&leading, &core, &trailing);
        Self {
            leading,
            core,
            trailing,
            kind,
            origin_line,
            force_break_after: false,
            width,
        }
    }

    /// Create a plain word token.
    #[must_use]
    pub fn word(core: &'text str, origin_line: usize) -> Self {
        Self::new(
            Cow::Borrowed(""),
            Cow::Borrowed(core),
            Cow::Borrowed(""),
            TokenKind::Word,
            origin_line,
        )
    }

    /// Replace the core text, for example with a capitalized word.
    pub fn set_core(&mut self, core: String) {
        self.core = Cow::Owned(core);
        self.width = token_width(&self.leading, &self.core, &self.trailing);
    }

    /// Add closing punctuation to the token.
    pub fn push_trailing(&mut self, character: char) {
        self.trailing.to_mut().push(character);
        self.width = token_width(&self.leading, &self.core, &self.trailing);
    }

    /// Replace the last character of the closing punctuation.
    pub fn replace_trailing_end(&mut self, character: char) {
        let trailing = self.trailing.to_mut();
        trailing.pop();
        trailing.push(character);
        self.width = token_width(&self.leading, &self.core, &self.trailing);
    }

    /// Full text of the token including surrounding punctuation.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}{}{}", self.leading, self.core, self.trailing)
    }

    /// Full text of the token, borrowing the core when there is no surrounding punctuation.
    #[must_use]
    pub fn text_cow(&self) -> Cow<'_, str> {
        if self.leading.is_empty() && self.trailing.is_empty() {
            Cow::Borrowed(&self.core)
        } else {
            Cow::Owned(self.text())
        }
    }

    /// Characters of the token in order, including surrounding punctuation.
    pub fn characters(&self) -> impl Iterator<Item = char> + '_ {
        self.leading
            .chars()
            .chain(self.core.chars())
            .chain(self.trailing.chars())
    }

    /// Width of the token in characters.
    ///
    /// The width is worked out once when the token is built,
    /// since it is asked for several times per token on every pass.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width as usize
    }

    /// Whether the token is an unbreakable atom whose inner punctuation carries no meaning.
    #[must_use]
    pub const fn is_atom(&self) -> bool {
        !matches!(self.kind, TokenKind::Word | TokenKind::Dash)
    }

    /// Whether the token ends with sentence punctuation, ignoring closing brackets and quotes.
    #[must_use]
    pub fn ends_sentence_punctuation(&self) -> bool {
        let trimmed = self.trailing.trim_end_matches(is_closer);
        trimmed.ends_with(['.', '!', '?'])
    }

    /// Whether the token ends with clause punctuation such as a comma, colon, semicolon, or dash.
    #[must_use]
    pub fn ends_clause_punctuation(&self) -> bool {
        if self.kind == TokenKind::Dash {
            return true;
        }
        let trimmed = self.trailing.trim_end_matches(is_closer);
        trimmed.ends_with([',', ':', ';', '—', '–'])
    }

    /// Whether the token ends with an opening bracket or quote.
    #[must_use]
    pub fn ends_with_opener(&self) -> bool {
        self.trailing
            .chars()
            .next_back()
            .or_else(|| self.core.chars().next_back())
            .or_else(|| self.leading.chars().next_back())
            .is_some_and(is_opener)
    }

    /// First visible character of the token.
    #[must_use]
    pub fn first_char(&self) -> Option<char> {
        self.leading.chars().next().or_else(|| self.core.chars().next())
    }
}

impl Paragraph {
    /// Prefix used for the given content line index.
    #[must_use]
    pub fn prefix_for(&self, line_index: usize) -> &str {
        if line_index == 0 {
            &self.first_prefix
        } else {
            &self.rest_prefix
        }
    }
}

impl LineRanges {
    /// Build a set from the given ranges, dropping empty ones and merging what overlaps or touches.
    #[must_use]
    pub fn new(ranges: impl IntoIterator<Item = RangeInclusive<usize>>) -> Self {
        let mut sorted: Vec<RangeInclusive<usize>> = ranges
            .into_iter()
            .filter(|range| *range.start() > 0 && range.start() <= range.end())
            .collect();
        sorted.sort_by_key(|range| (*range.start(), *range.end()));
        let mut merged: Vec<RangeInclusive<usize>> = Vec::with_capacity(sorted.len());
        for range in sorted {
            match merged.last_mut() {
                // Touching ranges are merged too, so 1-3 and 4-5 become one range instead of two.
                Some(last) if *range.start() <= last.end().saturating_add(1) => {
                    if range.end() > last.end() {
                        *last = *last.start()..=*range.end();
                    }
                }
                _ => merged.push(range),
            }
        }
        Self { ranges: merged }
    }

    /// Build a set from one flag per line, where the first flag is line one.
    #[must_use]
    pub fn from_flags(flags: &[bool]) -> Self {
        let mut ranges = Vec::new();
        let mut start: Option<usize> = None;
        for (index, selected) in flags.iter().enumerate() {
            match (selected, start) {
                (true, None) => start = Some(index + 1),
                (false, Some(first)) => {
                    ranges.push(first..=index);
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            ranges.push(first..=flags.len());
        }
        Self { ranges }
    }

    /// Whether the set selects every line because no range was given.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// The ranges of the set, sorted and merged.
    pub fn iter(&self) -> impl Iterator<Item = &RangeInclusive<usize>> {
        self.ranges.iter()
    }

    /// Whether the given one based line is selected.
    #[must_use]
    pub fn contains_line(&self, line: usize) -> bool {
        self.is_empty() || self.ranges.iter().any(|range| range.contains(&line))
    }

    /// Whether any selected line falls inside the given zero based half open line range.
    ///
    /// The bounds are the ones [`Region`] and [`Paragraph`] carry,
    /// so a block can be tested against the selection without converting them first.
    #[must_use]
    pub fn intersects(&self, start_line: usize, end_line: usize) -> bool {
        if self.is_empty() {
            return true;
        }
        if start_line >= end_line {
            return false;
        }
        let first = start_line + 1;
        self.ranges
            .iter()
            .any(|range| *range.start() <= end_line && first <= *range.end())
    }

    /// The union of this set and another one.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        Self::new(self.ranges.iter().chain(other.ranges.iter()).cloned())
    }
}

impl FromStr for LineRanges {
    type Err = String;

    /// Parse a comma separated list of one based lines and line ranges, for example "10-25,40".
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut ranges = Vec::new();
        for part in text.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("Empty line range in '{text}'"));
            }
            let (first, last) = match part.split_once('-') {
                Some((first, last)) => (first.trim(), last.trim()),
                None => (part, part),
            };
            let start = parse_line_number(first)?;
            let end = parse_line_number(last)?;
            if start > end {
                return Err(format!("Line range '{part}' ends before it starts"));
            }
            ranges.push(start..=end);
        }
        Ok(Self::new(ranges))
    }
}

impl RuleSet {
    /// All rules enabled.
    pub const ALL: Self = Self {
        line_too_long: true,
        mid_clause_break: true,
        semicolon: true,
        em_dash: true,
        trailing_comment: true,
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
    };

    /// No rules enabled.
    pub const NONE: Self = Self {
        line_too_long: false,
        mid_clause_break: false,
        semicolon: false,
        em_dash: false,
        trailing_comment: false,
    };

    /// Whether the given kind of violation is enabled.
    #[must_use]
    pub const fn is_enabled(self, kind: ViolationKind) -> bool {
        match kind {
            ViolationKind::LineTooLong => self.line_too_long,
            ViolationKind::MidClauseBreak => self.mid_clause_break,
            ViolationKind::Semicolon => self.semicolon,
            ViolationKind::EmDash => self.em_dash,
            ViolationKind::TrailingComment => self.trailing_comment,
        }
    }

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
            }
        }
        rules
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
            rules: RuleSet::DEFAULT,
            line_ranges: LineRanges::default(),
            abbreviations: crate::strings_from(DEFAULT_ABBREVIATIONS),
            clause_starters: Vec::new(),
            directive_prefixes: crate::strings_from(DEFAULT_DIRECTIVE_PREFIXES),
            preserve_lowercase: crate::strings_from(DEFAULT_PRESERVE_LOWERCASE),
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

/// Parse one side of a line range, rejecting anything that is not a line number of at least one.
fn parse_line_number(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("Line numbers start at 1".to_string()),
        Ok(number) => Ok(number),
        Err(_) => Err(format!("Invalid line number '{text}'")),
    }
}

/// Width of the text in characters, counting bytes when every one of them is ASCII.
#[must_use]
pub fn text_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.chars().count()
    }
}

/// Width of the three parts of a token in characters.
fn token_width(leading: &str, core: &str, trailing: &str) -> u32 {
    let width = text_width(leading) + text_width(core) + text_width(trailing);
    u32::try_from(width).unwrap_or(u32::MAX)
}

/// Whether the character closes a bracket or quote.
#[must_use]
pub const fn is_closer(character: char) -> bool {
    matches!(
        character,
        ')' | ']' | '}' | '"' | '\'' | '”' | '’' | '»' | '`' | '*' | '_'
    )
}

/// Whether the character opens a bracket or quote.
#[must_use]
pub const fn is_opener(character: char) -> bool {
    matches!(character, '(' | '[' | '{' | '"' | '\'' | '“' | '‘' | '«')
}

#[cfg(test)]
mod test_file_kind {
    use super::*;

    #[test]
    fn detects_kind_from_extension_and_name() {
        assert_eq!(FileKind::from_path(Path::new("src/lib.rs")), Some(FileKind::Rust));
        assert_eq!(FileKind::from_path(Path::new("a/b.PY")), Some(FileKind::Python));
        assert_eq!(FileKind::from_path(Path::new("x.tsx")), Some(FileKind::JavaScript));
        assert_eq!(FileKind::from_path(Path::new("Dockerfile")), Some(FileKind::Dockerfile));
        assert_eq!(FileKind::from_path(Path::new("Makefile")), Some(FileKind::Makefile));
        assert_eq!(FileKind::from_path(Path::new("README.md")), Some(FileKind::Markdown));
        assert_eq!(FileKind::from_path(Path::new("archive.zip")), None);
        assert_eq!(FileKind::from_path(Path::new("LICENSE")), None);
    }

    #[test]
    fn comment_style_orders_markers_longest_first() {
        let style = FileKind::Rust.comment_style();
        assert_eq!(style.line_markers, &["//!", "///", "//"]);
        assert!(style.block.is_some());
        assert!(FileKind::Python.comment_style().docstrings);
        assert!(FileKind::Markdown.comment_style().line_markers.is_empty());
    }

    #[test]
    fn trailing_comment_support_matches_tiers() {
        assert!(FileKind::Rust.supports_trailing_comment_check());
        assert!(FileKind::Yaml.supports_trailing_comment_check());
        assert!(!FileKind::Dockerfile.supports_trailing_comment_check());
        assert!(!FileKind::Markdown.supports_trailing_comment_check());
        assert!(!FileKind::Lua.supports_trailing_comment_check());
    }
}

#[cfg(test)]
mod test_token_builders {
    use super::*;

    /// Build a token with the given punctuation around a word.
    pub fn wrapped(leading: &'static str, core: &'static str, trailing: &'static str) -> Token<'static> {
        Token::new(
            Cow::Borrowed(leading),
            Cow::Borrowed(core),
            Cow::Borrowed(trailing),
            TokenKind::Word,
            0,
        )
    }
}

#[cfg(test)]
mod test_token {
    use super::*;

    #[test]
    fn width_and_text_include_punctuation() {
        let token = Token::new(
            Cow::Borrowed("("),
            Cow::Borrowed("ääkkönen"),
            Cow::Borrowed(")."),
            TokenKind::Word,
            0,
        );
        assert_eq!(token.text(), "(ääkkönen).");
        assert_eq!(token.width(), 11);
        assert_eq!(
            token.width(),
            token.characters().count(),
            "the cached width has to match"
        );
        assert!(token.ends_sentence_punctuation());
        assert!(!token.ends_clause_punctuation());
    }

    #[test]
    fn clause_punctuation_ignores_closers() {
        let mut token = Token::word("value", 0);
        token.push_trailing(',');
        token.push_trailing('"');
        assert_eq!(token.trailing, ",\"");
        assert!(token.ends_clause_punctuation());
        assert!(!super::test_token_builders::wrapped("", "value", "\")").ends_clause_punctuation());
    }

    #[test]
    fn the_cached_width_follows_every_change() {
        let mut token = Token::word("value", 0);
        assert_eq!(token.width(), 5);
        token.push_trailing('.');
        assert_eq!(token.width(), 6);
        assert_eq!(token.width(), token.characters().count());
        token.set_core("ääkkönen".to_string());
        assert_eq!(token.width(), 9);
        assert_eq!(token.width(), token.characters().count());
        token.replace_trailing_end('!');
        assert_eq!(token.width(), token.characters().count());
    }
}

#[cfg(test)]
mod test_rank {
    use super::*;

    #[test]
    fn lowering_moves_one_step_down_and_stops_at_word() {
        assert_eq!(Rank::Forced.lowered(), Rank::Sentence);
        assert_eq!(Rank::Sentence.lowered(), Rank::Colon);
        assert_eq!(Rank::Colon.lowered(), Rank::ClauseTier1);
        assert_eq!(Rank::ClauseTier1.lowered(), Rank::ClauseTier2);
        assert_eq!(Rank::ClauseTier2.lowered(), Rank::Punctuation);
        assert_eq!(Rank::Punctuation.lowered(), Rank::ClauseTier3);
        assert_eq!(Rank::ClauseTier3.lowered(), Rank::ClauseTier4);
        assert_eq!(Rank::ClauseTier4.lowered(), Rank::Word);
        assert_eq!(Rank::Word.lowered(), Rank::Word);
    }

    #[test]
    fn the_ranking_order_is_word_to_forced() {
        assert!(Rank::Word < Rank::ClauseTier4);
        assert!(Rank::ClauseTier4 < Rank::ClauseTier3);
        assert!(Rank::ClauseTier3 < Rank::Punctuation);
        assert!(Rank::Punctuation < Rank::ClauseTier2);
        assert!(Rank::ClauseTier2 < Rank::ClauseTier1);
        assert!(Rank::ClauseTier1 < Rank::Colon);
        assert!(Rank::Colon < Rank::Sentence);
        assert!(Rank::Sentence < Rank::Forced);
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
        assert!(rules.is_enabled(ViolationKind::Semicolon));
        assert!(!rules.is_enabled(ViolationKind::EmDash));
    }
}

#[cfg(test)]
mod test_file_kind_extensions {
    use super::*;

    #[test]
    fn every_supported_extension_maps_to_a_kind() {
        let expected = [
            ("rs", FileKind::Rust),
            ("cpp", FileKind::CLike),
            ("java", FileKind::CLike),
            ("kt", FileKind::CLike),
            ("swift", FileKind::CLike),
            ("cs", FileKind::CLike),
            ("scala", FileKind::CLike),
            ("dart", FileKind::CLike),
            ("mm", FileKind::CLike),
            ("ts", FileKind::JavaScript),
            ("mjs", FileKind::JavaScript),
            ("go", FileKind::Go),
            ("py", FileKind::Python),
            ("pyi", FileKind::Python),
            ("sh", FileKind::Shell),
            ("zsh", FileKind::Shell),
            ("fish", FileKind::Shell),
            ("toml", FileKind::Toml),
            ("yaml", FileKind::Yaml),
            ("yml", FileKind::Yaml),
            ("dockerfile", FileKind::Dockerfile),
            ("mk", FileKind::Makefile),
            ("rb", FileKind::Ruby),
            ("sql", FileKind::Sql),
            ("lua", FileKind::Lua),
            ("markdown", FileKind::Markdown),
        ];
        for (extension, kind) in expected {
            assert_eq!(FileKind::from_extension(extension), Some(kind), "extension {extension}");
            assert_eq!(
                FileKind::from_extension(&extension.to_ascii_uppercase()),
                Some(kind),
                "uppercase extension {extension}"
            );
        }
        assert_eq!(FileKind::from_extension("zip"), None);
        assert_eq!(FileKind::from_extension(""), None);
    }

    #[test]
    fn well_known_file_names_map_to_a_kind() {
        let expected = [
            ("Dockerfile", FileKind::Dockerfile),
            ("Containerfile", FileKind::Dockerfile),
            ("Makefile", FileKind::Makefile),
            ("makefile", FileKind::Makefile),
            ("GNUmakefile", FileKind::Makefile),
            ("Gemfile", FileKind::Ruby),
            ("Rakefile", FileKind::Ruby),
            (".bashrc", FileKind::Shell),
            (".zshrc", FileKind::Shell),
            (".zshenv", FileKind::Shell),
            (".zprofile", FileKind::Shell),
            (".profile", FileKind::Shell),
            (".bash_profile", FileKind::Shell),
        ];
        for (name, kind) in expected {
            assert_eq!(FileKind::from_path(Path::new(name)), Some(kind), "file name {name}");
            assert_eq!(
                FileKind::from_path(Path::new("some/directory").join(name).as_path()),
                Some(kind),
                "nested file name {name}"
            );
        }
    }

    #[test]
    fn comment_styles_cover_every_kind() {
        let kinds = [
            FileKind::Rust,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
            FileKind::Dockerfile,
            FileKind::Makefile,
            FileKind::Ruby,
            FileKind::Sql,
            FileKind::Lua,
            FileKind::Markdown,
        ];
        for kind in kinds {
            let style = kind.comment_style();
            let has_syntax = !style.line_markers.is_empty() || style.block.is_some() || style.docstrings;
            assert_eq!(
                has_syntax,
                kind != FileKind::Markdown,
                "{kind:?} should only lack comment syntax for Markdown"
            );
        }
        assert_eq!(FileKind::Sql.comment_style().line_markers, &["--"]);
        assert_eq!(FileKind::Lua.comment_style().line_markers, &["--"]);
        assert_eq!(FileKind::Go.comment_style().line_markers, &["///", "//"]);
        assert!(FileKind::CLike.comment_style().block.is_some());
        assert!(FileKind::Yaml.comment_style().block.is_none());
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

#[cfg(test)]
mod test_token_helpers {
    use super::*;

    #[test]
    fn text_cow_borrows_a_bare_core() {
        let bare = Token::word("value", 0);
        assert!(matches!(bare.text_cow(), Cow::Borrowed("value")));
        assert_eq!(bare.text(), "value");

        let wrapped = super::test_token_builders::wrapped("(", "value", ").");
        assert!(matches!(wrapped.text_cow(), Cow::Owned(_)));
        assert_eq!(wrapped.text_cow(), "(value).");
    }

    #[test]
    fn characters_yield_the_whole_token_in_order() {
        let token = super::test_token_builders::wrapped("[", "value", "],");
        assert_eq!(token.characters().collect::<String>(), "[value],");
        assert_eq!(token.width(), 8);
    }

    #[test]
    fn first_char_falls_back_to_the_core() {
        let bare = Token::word("word", 3);
        assert_eq!(bare.first_char(), Some('w'));
        assert_eq!(bare.origin_line, 3);

        let wrapped = super::test_token_builders::wrapped("\"", "word", "");
        assert_eq!(wrapped.first_char(), Some('"'));

        let empty = Token::word("", 0);
        assert_eq!(empty.first_char(), None);
    }

    #[test]
    fn ends_with_opener_checks_the_last_character_of_the_token() {
        let token = Token::word("call", 0);
        assert!(!token.ends_with_opener());
        assert!(super::test_token_builders::wrapped("", "call", "(").ends_with_opener());

        assert!(Token::word("(", 0).ends_with_opener());
        assert!(super::test_token_builders::wrapped("[", "", "").ends_with_opener());
    }

    #[test]
    fn atoms_are_every_kind_except_words_and_dashes() {
        let kinds = [
            (TokenKind::Word, false),
            (TokenKind::Dash, false),
            (TokenKind::Code, true),
            (TokenKind::Url, true),
            (TokenKind::Path, true),
            (TokenKind::Version, true),
            (TokenKind::Number, true),
            (TokenKind::Identifier, true),
            (TokenKind::Link, true),
            (TokenKind::Html, true),
        ];
        for (kind, is_atom) in kinds {
            let token = Token::new(Cow::Borrowed(""), Cow::Borrowed("x"), Cow::Borrowed(""), kind, 0);
            assert_eq!(token.is_atom(), is_atom, "{kind:?}");
        }
    }
}

#[cfg(test)]
mod test_options_and_prefixes {
    use super::*;

    #[test]
    fn hard_break_markers_round_trip() {
        assert_eq!(HardBreak::None.marker(), "");
        assert_eq!(HardBreak::Spaces.marker(), "  ");
        assert_eq!(HardBreak::Backslash.marker(), "\\");
        assert_eq!(HardBreak::default(), HardBreak::None);
    }

    #[test]
    fn the_first_line_uses_the_first_prefix() {
        let paragraph = Paragraph {
            start_line: 0,
            end_line: 2,
            first_prefix: "/// - ".to_string(),
            rest_prefix: "///   ".to_string(),
            last_suffix: String::new(),
            lines: vec!["one".to_string(), "two".to_string()],
            hard_breaks: vec![HardBreak::None; 2],
        };
        assert_eq!(paragraph.prefix_for(0), "/// - ");
        assert_eq!(paragraph.prefix_for(1), "///   ");
        assert_eq!(paragraph.prefix_for(9), "///   ");
    }

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

    #[test]
    fn openers_and_closers_are_recognized() {
        for character in ['(', '[', '{', '"', '\'', '“', '‘', '«'] {
            assert!(is_opener(character), "{character} should open");
        }
        for character in [')', ']', '}', '"', '\'', '”', '’', '»', '`', '*', '_'] {
            assert!(is_closer(character), "{character} should close");
        }
        assert!(!is_opener('a'));
        assert!(!is_closer('a'));
    }
}

#[cfg(test)]
mod test_line_ranges {
    use super::*;

    fn ranges(text: &str) -> LineRanges {
        text.parse::<LineRanges>().expect("the ranges should parse")
    }

    fn as_pairs(ranges: &LineRanges) -> Vec<(usize, usize)> {
        ranges.iter().map(|range| (*range.start(), *range.end())).collect()
    }

    #[test]
    fn a_single_line_becomes_a_range_of_one() {
        assert_eq!(as_pairs(&ranges("10")), vec![(10, 10)]);
    }

    #[test]
    fn a_range_keeps_both_ends() {
        assert_eq!(as_pairs(&ranges("10-25")), vec![(10, 25)]);
    }

    #[test]
    fn a_comma_separated_list_is_parsed() {
        assert_eq!(as_pairs(&ranges("40,10-25,4")), vec![(4, 4), (10, 25), (40, 40)]);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(as_pairs(&ranges(" 10 - 25 , 40 ")), vec![(10, 25), (40, 40)]);
    }

    #[test]
    fn overlapping_ranges_are_merged() {
        assert_eq!(as_pairs(&ranges("1-3,2-6")), vec![(1, 6)]);
    }

    #[test]
    fn touching_ranges_are_merged() {
        assert_eq!(as_pairs(&ranges("1-3,4-5")), vec![(1, 5)]);
    }

    #[test]
    fn a_contained_range_does_not_shorten_the_one_holding_it() {
        assert_eq!(as_pairs(&ranges("1-10,3-4")), vec![(1, 10)]);
    }

    #[test]
    fn line_zero_is_rejected() {
        assert!("0".parse::<LineRanges>().is_err());
        assert!("0-5".parse::<LineRanges>().is_err());
    }

    #[test]
    fn a_reversed_range_is_rejected() {
        assert!("25-10".parse::<LineRanges>().is_err());
    }

    #[test]
    fn text_that_is_not_a_line_number_is_rejected() {
        assert!("abc".parse::<LineRanges>().is_err());
        assert!("".parse::<LineRanges>().is_err());
        assert!("10,".parse::<LineRanges>().is_err());
        assert!("-5".parse::<LineRanges>().is_err());
    }

    #[test]
    fn an_empty_set_selects_every_line() {
        let empty = LineRanges::default();
        assert!(empty.is_empty());
        assert!(empty.contains_line(1));
        assert!(empty.contains_line(1000));
        assert!(empty.intersects(0, 1));
        assert!(empty.intersects(500, 600));
    }

    #[test]
    fn contains_line_covers_both_ends_of_a_range() {
        let selection = ranges("10-12");
        assert!(!selection.contains_line(9));
        assert!(selection.contains_line(10));
        assert!(selection.contains_line(12));
        assert!(!selection.contains_line(13));
    }

    #[test]
    fn intersects_takes_zero_based_half_open_bounds() {
        let selection = ranges("10-12");
        // Lines 10 to 12 are the zero based indices 9 to 11.
        assert!(!selection.intersects(0, 9));
        assert!(selection.intersects(9, 10));
        assert!(selection.intersects(11, 12));
        assert!(!selection.intersects(12, 20));
        // A block reaching into the selection from either side overlaps it.
        assert!(selection.intersects(0, 10));
        assert!(selection.intersects(11, 30));
    }

    #[test]
    fn an_empty_block_intersects_nothing() {
        assert!(!ranges("10-12").intersects(10, 10));
    }

    #[test]
    fn flags_become_the_runs_they_mark() {
        let flags = [false, true, true, false, true];
        assert_eq!(as_pairs(&LineRanges::from_flags(&flags)), vec![(2, 3), (5, 5)]);
        assert_eq!(as_pairs(&LineRanges::from_flags(&[true, true])), vec![(1, 2)]);
        assert!(LineRanges::from_flags(&[]).is_empty());
        assert!(LineRanges::from_flags(&[false, false]).is_empty());
    }

    #[test]
    fn a_union_merges_both_sides() {
        assert_eq!(as_pairs(&ranges("1-3").union(&ranges("4-6"))), vec![(1, 6)]);
        assert_eq!(as_pairs(&ranges("1-3").union(&ranges("10"))), vec![(1, 3), (10, 10)]);
        assert_eq!(as_pairs(&ranges("5").union(&LineRanges::default())), vec![(5, 5)]);
    }
}
