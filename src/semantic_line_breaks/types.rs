//! Shared types for the semantic line breaks formatter.
//!
//! Contains the file kind mapping, comment syntax descriptions, violation and token types,
//! paragraph and region representations, and the formatting options used by the prose engine, the block splitters,
//! and the `slb` binary.

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

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
    /// After a comma, colon, or dash.
    Punctuation,
    /// Before a coordinating conjunction such as "but" or "or".
    ClauseTier2,
    /// Before "and".
    ClauseTier1,
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
    pub message: String,
    /// Whether fix mode can repair this violation.
    pub fixable: bool,
}

/// A prose token: a word or an unbreakable atom with surrounding punctuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Opening punctuation such as `(` or `"`.
    pub leading: String,
    /// The word or atom body.
    pub core: String,
    /// Closing punctuation such as `.` or `,)`.
    pub trailing: String,
    /// Token classification.
    pub kind: TokenKind,
    /// Index of the paragraph line this token came from.
    pub origin_line: usize,
    /// Whether the formatter inserted a forced break after this token.
    pub force_break_after: bool,
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
            Self::Sentence => Self::ClauseTier1,
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

impl Token {
    /// Create a plain word token.
    #[must_use]
    pub fn word(core: &str, origin_line: usize) -> Self {
        Self {
            leading: String::new(),
            core: core.to_string(),
            trailing: String::new(),
            kind: TokenKind::Word,
            origin_line,
            force_break_after: false,
        }
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
    #[must_use]
    pub fn width(&self) -> usize {
        self.leading.chars().count() + self.core.chars().count() + self.trailing.chars().count()
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

impl RuleSet {
    /// All rules enabled.
    pub const ALL: Self = Self {
        line_too_long: true,
        mid_clause_break: true,
        semicolon: true,
        em_dash: true,
        trailing_comment: true,
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
        Self::ALL
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
            rules: RuleSet::ALL,
            abbreviations: to_strings(DEFAULT_ABBREVIATIONS),
            clause_starters: Vec::new(),
            directive_prefixes: to_strings(DEFAULT_DIRECTIVE_PREFIXES),
            preserve_lowercase: to_strings(DEFAULT_PRESERVE_LOWERCASE),
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

/// Convert a static string slice list into owned strings.
fn to_strings(values: &[&str]) -> Vec<String> {
    values.iter().map(std::string::ToString::to_string).collect()
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
mod test_token {
    use super::*;

    #[test]
    fn width_and_text_include_punctuation() {
        let token = Token {
            leading: "(".to_string(),
            core: "ääkkönen".to_string(),
            trailing: ").".to_string(),
            kind: TokenKind::Word,
            origin_line: 0,
            force_break_after: false,
        };
        assert_eq!(token.text(), "(ääkkönen).");
        assert_eq!(token.width(), 11);
        assert!(token.ends_sentence_punctuation());
        assert!(!token.ends_clause_punctuation());
    }

    #[test]
    fn clause_punctuation_ignores_closers() {
        let mut token = Token::word("value", 0);
        token.trailing = ",\"".to_string();
        assert!(token.ends_clause_punctuation());
        token.trailing = "\")".to_string();
        assert!(!token.ends_clause_punctuation());
    }
}

#[cfg(test)]
mod test_rank {
    use super::*;

    #[test]
    fn lowering_moves_one_step_down_and_stops_at_word() {
        assert_eq!(Rank::Forced.lowered(), Rank::Sentence);
        assert_eq!(Rank::Sentence.lowered(), Rank::ClauseTier1);
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
        assert!(Rank::ClauseTier1 < Rank::Sentence);
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
            message: "semicolon joins clauses".to_string(),
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

        let mut wrapped = Token::word("value", 0);
        wrapped.leading = "(".to_string();
        wrapped.trailing = ").".to_string();
        assert!(matches!(wrapped.text_cow(), Cow::Owned(_)));
        assert_eq!(wrapped.text_cow(), "(value).");
    }

    #[test]
    fn characters_yield_the_whole_token_in_order() {
        let mut token = Token::word("value", 0);
        token.leading = "[".to_string();
        token.trailing = "],".to_string();
        assert_eq!(token.characters().collect::<String>(), "[value],");
        assert_eq!(token.width(), 8);
    }

    #[test]
    fn first_char_falls_back_to_the_core() {
        let bare = Token::word("word", 3);
        assert_eq!(bare.first_char(), Some('w'));
        assert_eq!(bare.origin_line, 3);

        let mut wrapped = Token::word("word", 0);
        wrapped.leading = "\"".to_string();
        assert_eq!(wrapped.first_char(), Some('"'));

        let empty = Token::word("", 0);
        assert_eq!(empty.first_char(), None);
    }

    #[test]
    fn ends_with_opener_checks_the_last_character_of_the_token() {
        let mut token = Token::word("call", 0);
        assert!(!token.ends_with_opener());
        token.trailing = "(".to_string();
        assert!(token.ends_with_opener());

        let mut core_only = Token::word("(", 0);
        assert!(core_only.ends_with_opener());
        core_only.core = String::new();
        core_only.leading = "[".to_string();
        assert!(core_only.ends_with_opener());
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
            let token = Token {
                kind,
                ..Token::word("x", 0)
            };
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
        assert_eq!(options.rules, RuleSet::ALL);
        assert_eq!(options.abbreviations, defaults.abbreviations);
        assert_eq!(options.directive_prefixes, defaults.directive_prefixes);
        assert_eq!(options.preserve_lowercase, defaults.preserve_lowercase);
        assert!(options.clause_starters.is_empty());
        assert!(!options.join_sentences);
        assert!(!options.allow_word_break);
        assert_eq!(defaults.max_width, DEFAULT_MAX_WIDTH);
    }

    #[test]
    fn the_rule_set_defaults_to_all_rules() {
        assert_eq!(RuleSet::default(), RuleSet::ALL);
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
