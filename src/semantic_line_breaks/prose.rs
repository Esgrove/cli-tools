//! Prose engine for the semantic line breaks formatter.
//!
//! Implements tokenization of a prose line into words and unbreakable atoms,
//! sentence and clause boundary detection, mid-clause break detection between consecutive lines,
//! rewording of semicolons and em dashes, and the reflow algorithm
//! that joins hard-wrapped lines and re-breaks them at semantic boundaries.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use super::types::{
    FormatOptions, HardBreak, Paragraph, Rank, Token, TokenKind, Violation, ViolationKind, is_closer, is_opener,
};

/// Number of characters a line may exceed the maximum width by before it is considered too long.
///
/// The width is a soft target.
/// Tolerating a small overflow allows breaking before a conjunction
/// when that reads better than a strict break at a comma.
pub const SOFT_OVERFLOW: usize = 10;

/// Minimum fill of the budget, in percent, for a preferred clause word break to be considered.
const MIN_FILL_PERCENT: usize = 50;

/// Smallest usable budget. Paragraphs with a narrower budget are left alone.
const MIN_BUDGET: usize = 10;

/// Minimum number of tokens on a line before a missing terminal punctuation counts as a mid-clause break.
const MIN_TOKENS_FOR_MID_CLAUSE: usize = 3;

/// Clause words that are the most preferred break points.
const CLAUSE_TIER_1: &[&str] = &["and"];

/// Coordinating conjunctions, preferred over punctuation.
const CLAUSE_TIER_2: &[&str] = &["but", "or", "nor", "yet", "so"];

/// Subordinating conjunctions and connectors, preferred after punctuation.
const CLAUSE_TIER_3: &[&str] = &[
    "because",
    "since",
    "although",
    "though",
    "while",
    "whereas",
    "if",
    "unless",
    "until",
    "when",
    "whenever",
    "where",
    "after",
    "before",
    "rather",
    "instead",
    "except",
    "then",
    "otherwise",
    "however",
    "therefore",
    "thus",
    "e.g",
    "i.e",
];

/// Two word connectors, treated like tier 3.
const CLAUSE_PAIRS: &[&str] = &["for example", "such as", "as well as", "in order to"];

/// Relative pronouns and weak connectors, the least preferred clause words.
const CLAUSE_TIER_4: &[&str] = &["which", "that", "as"];

/// Words that clearly leave a clause unfinished when they end a line.
const DANGLING_WORDS: &[&str] = &[
    "the", "a", "an", "of", "to", "in", "on", "for", "with", "by", "from", "at", "into", "and", "or", "but", "is",
    "are", "be", "was", "were", "that", "which", "this", "these", "those", "its", "their", "as", "not", "no", "any",
    "all", "each", "every", "same", "than", "very", "more", "most",
];

/// Matches a URL such as `https://example.com/path` or `www.example.com`.
static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:[a-z][a-z0-9+.-]*://\S+|www\.\S+)$").expect("Invalid URL regex"));

/// Matches a file name with an extension such as `lib.rs`.
static RE_FILE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.-]+\.[A-Za-z0-9]{1,5}$").expect("Invalid file name regex"));

/// Matches a version number such as `1.2.3` or `v2.0-beta`.
static RE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?\d+(?:\.\d+){1,3}(?:[-+][\w.]+)?$").expect("Invalid version regex"));

/// Matches a plain number such as `42`, `3.14`, or `50%`.
static RE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[+-]?\d[\d_,]*(?:\.\d+)?%?$").expect("Invalid number regex"));

/// Matches identifiers: `snake_case`, `path::segments`, `camelCase`, `SCREAMING_CASE`,
/// and calls like `foo()`.
static RE_IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\w*_\w*|\w+(?:::\w+)+|[a-z]+[A-Z]\w*|[\w:.]+\(.*\))$").expect("Invalid identifier regex")
});

/// Matches dotted letter abbreviations such as `e.g` or `i.e` that would otherwise look like file names.
static RE_LETTER_ABBREVIATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[A-Za-z]\.)+[A-Za-z]$").expect("Invalid abbreviation regex"));

/// Matches a plain lowercase word that can safely be capitalized.
static RE_LOWERCASE_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z']*$").expect("Invalid lowercase word regex"));

/// Matches a token that would start a Markdown list item or heading if placed at a line start.
static RE_MARKDOWN_STRUCTURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[-+*>|]|#+|\d+[.)]|```.*|~~~.*)$").expect("Invalid Markdown structure regex"));

/// A candidate break position between two tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// Index of the token that starts the new line.
    pub before: usize,
    /// Quality of the break.
    pub rank: Rank,
}

/// A run of tokens that is reflowed as one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Tokens of the segment in order.
    pub tokens: Vec<Token>,
    /// Hard break marker at the end of the segment.
    pub hard_break: HardBreak,
    /// Whether joining or rewording changed the segment, forcing a re-split.
    pub modified: bool,
    /// Index of the single paragraph line the segment came from, when it covers exactly one line.
    pub source_line: Option<usize>,
}

/// Result of reflowing a paragraph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReflowOutcome {
    /// New content lines with hard break markers, or `None` when the paragraph is unchanged.
    pub lines: Option<Vec<String>>,
    /// Violations found in the paragraph.
    pub violations: Vec<Violation>,
}

/// Tokenize one content line into words and atoms.
///
/// When `normalize_dashes` is set, em dashes attached to words are separated into standalone dash tokens
/// so the em dash rewrite can handle them uniformly.
#[must_use]
pub fn tokenize_line(content: &str, origin_line: usize, normalize_dashes: bool) -> Vec<Token> {
    let normalized: Cow<'_, str> = if normalize_dashes && content.contains('—') {
        Cow::Owned(content.replace('—', " — "))
    } else {
        Cow::Borrowed(content)
    };
    let chars: Vec<char> = normalized.chars().collect();
    let mut tokens = Vec::new();
    let mut position = 0;
    while position < chars.len() {
        while chars.get(position).is_some_and(|character| character.is_whitespace()) {
            position += 1;
        }
        if position >= chars.len() {
            break;
        }
        let chunk_end = chunk_end(&chars, position);
        let (token, end) = read_token(&chars, position, chunk_end, origin_line);
        tokens.push(token);
        position = end.max(position + 1);
    }
    tokens
}

/// Whether the token at `index` ends a sentence and the following token starts a new one.
#[must_use]
pub fn is_sentence_end(tokens: &[Token], index: usize, options: &FormatOptions) -> bool {
    let Some(token) = tokens.get(index) else {
        return false;
    };
    let Some(next) = tokens.get(index + 1) else {
        return false;
    };
    is_sentence_boundary(token, next, options)
}

/// Whether `token` ends a sentence that `next` continues after.
#[must_use]
pub fn is_sentence_boundary(token: &Token, next: &Token, options: &FormatOptions) -> bool {
    ends_sentence(token, options) && starts_sentence(next)
}

/// Whether the token ends a sentence, ignoring what follows it.
#[must_use]
pub fn ends_sentence(token: &Token, options: &FormatOptions) -> bool {
    if !token.ends_sentence_punctuation() {
        return false;
    }
    if token.trailing.contains("..") || token.trailing.contains('…') {
        return false;
    }
    if matches!(token.kind, TokenKind::Word | TokenKind::Path | TokenKind::Identifier) {
        if is_abbreviation(&token.core, options) {
            return false;
        }
        let mut core_chars = token.core.chars();
        if let (Some(first), None) = (core_chars.next(), core_chars.next())
            && first.is_uppercase()
        {
            return false;
        }
    }
    true
}

/// Whether the token can start a new sentence.
fn starts_sentence(token: &Token) -> bool {
    if matches!(
        token.kind,
        TokenKind::Code | TokenKind::Link | TokenKind::Url | TokenKind::Html
    ) {
        return true;
    }
    token.first_char().is_some_and(|first| {
        first.is_uppercase()
            || first.is_ascii_digit()
            || matches!(first, '"' | '“' | '\'' | '‘' | '(' | '[' | '*' | '_')
    })
}

/// Rank of the clause word at `index`, or `None` when the token is not a clause starter.
#[must_use]
pub fn clause_rank(tokens: &[Token], index: usize, options: &FormatOptions) -> Option<Rank> {
    let token = tokens.get(index)?;
    if token.kind != TokenKind::Word {
        return None;
    }
    let word = token.core.as_str();
    if let Some(next) = tokens.get(index + 1)
        && CLAUSE_PAIRS.iter().any(|pair| is_word_pair(pair, word, &next.core))
    {
        return Some(Rank::ClauseTier3);
    }
    if contains_word(CLAUSE_TIER_1, word) {
        Some(Rank::ClauseTier1)
    } else if contains_word(CLAUSE_TIER_2, word) {
        Some(Rank::ClauseTier2)
    } else if contains_word(CLAUSE_TIER_3, word) {
        Some(Rank::ClauseTier3)
    } else if contains_word(CLAUSE_TIER_4, word)
        || options
            .clause_starters
            .iter()
            .any(|starter| starter.eq_ignore_ascii_case(word))
    {
        Some(Rank::ClauseTier4)
    } else {
        None
    }
}

/// Find all break candidates between the tokens, ranked by quality.
#[must_use]
pub fn find_boundaries(tokens: &[Token], options: &FormatOptions) -> Vec<Boundary> {
    let mut boundaries = Vec::new();
    let mut depth = 0usize;
    for index in 1..tokens.len() {
        let (Some(previous), Some(current)) = (tokens.get(index - 1), tokens.get(index)) else {
            continue;
        };
        depth = bracket_depth_after(previous, depth);
        if starts_markdown_structure(current) {
            continue;
        }
        let clause = if index >= 2 && clause_rank(tokens, index - 1, options).is_none() {
            clause_rank(tokens, index, options)
        } else {
            None
        };
        let mut rank = if previous.force_break_after {
            Rank::Forced
        } else if is_sentence_end(tokens, index - 1, options) {
            Rank::Sentence
        } else if previous.ends_clause_punctuation() {
            clause.map_or(Rank::Punctuation, |clause| clause.max(Rank::Punctuation))
        } else {
            clause.unwrap_or(Rank::Word)
        };
        if depth > 0
            && matches!(
                rank,
                Rank::Punctuation | Rank::ClauseTier1 | Rank::ClauseTier2 | Rank::ClauseTier3 | Rank::ClauseTier4
            )
        {
            rank = Rank::Word;
        } else if depth == 0 && rank == Rank::Word {
            let closes_group = previous.trailing.contains([')', ']']);
            let opens_group = current.leading.contains(['(', '[']);
            if closes_group || opens_group {
                rank = Rank::ClauseTier4;
            }
        }
        boundaries.push(Boundary { before: index, rank });
    }
    boundaries
}

/// Bracket nesting depth after the token, given the depth before it.
fn bracket_depth_after(token: &Token, depth: usize) -> usize {
    bracket_characters(token).fold(depth, |depth, character| match character {
        '(' | '[' => depth + 1,
        ')' | ']' => depth.saturating_sub(1),
        _ => depth,
    })
}

/// Characters of the token that affect bracket nesting, skipping the inner text of an atom.
fn bracket_characters(token: &Token) -> impl Iterator<Item = char> + '_ {
    let core = if token.is_atom() { "" } else { token.core.as_str() };
    token.leading.chars().chain(core.chars()).chain(token.trailing.chars())
}

/// Whether the break between two consecutive lines falls in the middle of a clause.
#[must_use]
pub fn is_mid_clause_break(line_a: &[Token], line_b: &[Token], hard_break: HardBreak, options: &FormatOptions) -> bool {
    if hard_break != HardBreak::None || line_a.len() < MIN_TOKENS_FOR_MID_CLAUSE {
        return false;
    }
    let (Some(last), Some(first)) = (line_a.last(), line_b.first()) else {
        return false;
    };
    if last.ends_sentence_punctuation() || last.ends_clause_punctuation() || last.ends_with_opener() {
        return false;
    }
    if last.trailing.contains([')', ']']) || first.leading.contains(['(', '[']) {
        return false;
    }
    if clause_rank(line_b, 0, options).is_some() {
        return false;
    }
    if first.text().ends_with(':') || starts_markdown_structure(first) {
        return false;
    }
    let last_is_dangling = last.kind == TokenKind::Word && contains_word(DANGLING_WORDS, &last.core);
    let first_is_uppercase = first.core.chars().next().is_some_and(char::is_uppercase);
    if first_is_uppercase && !last_is_dangling && clause_rank(line_a, line_a.len() - 1, options).is_none() {
        return false;
    }
    true
}

/// Reflow a paragraph: join mid-clause breaks, reword semicolons and dashes, and re-break long lines.
#[must_use]
pub fn reflow_paragraph(paragraph: &Paragraph, options: &FormatOptions) -> ReflowOutcome {
    let line_offset = paragraph.start_line + 1;
    let first_budget = options
        .max_width
        .saturating_sub(prefix_width(&paragraph.first_prefix, options.tab_width));
    let rest_budget = options
        .max_width
        .saturating_sub(prefix_width(&paragraph.rest_prefix, options.tab_width));
    let hard_limit = options.max_width + SOFT_OVERFLOW;

    if first_budget.min(rest_budget) <= MIN_BUDGET {
        let violations = if options.rules.line_too_long {
            over_long_line_violations(paragraph, options, hard_limit, false)
        } else {
            Vec::new()
        };
        return ReflowOutcome {
            lines: None,
            violations,
        };
    }

    let line_tokens: Vec<Vec<Token>> = paragraph
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| tokenize_line(line, index, options.rules.em_dash))
        .collect();
    let (mut segments, mut violations) = build_segments(paragraph, line_tokens, options);
    reword_segments(&mut segments, options, line_offset, &mut violations);
    merge_changed_sentences(&mut segments, options);

    let mut output: Vec<(String, HardBreak)> = Vec::new();
    let mut is_first_line = true;
    for segment in segments {
        let should_split = segment.modified || options.rules.line_too_long;
        let source_line = if segment.modified { None } else { segment.source_line };
        let mut emitted_any = false;
        let pieces = if should_split {
            split_tokens(
                segment.tokens,
                first_budget,
                rest_budget,
                &mut is_first_line,
                options,
                line_offset,
                &mut violations,
            )
        } else {
            is_first_line = false;
            vec![segment.tokens]
        };
        let unchanged_line = if pieces.len() == 1 { source_line } else { None };
        for piece in pieces {
            let content = unchanged_line
                .and_then(|line| paragraph.lines.get(line).cloned())
                .unwrap_or_else(|| join_tokens(&piece));
            output.push((content, HardBreak::None));
            emitted_any = true;
        }
        if emitted_any && let Some(last) = output.last_mut() {
            last.1 = segment.hard_break;
        }
    }

    let changed = output.len() != paragraph.lines.len()
        || output
            .iter()
            .zip(paragraph.lines.iter().zip(&paragraph.hard_breaks))
            .any(|((content, hard_break), (line, original_break))| content != line || hard_break != original_break);

    if options.rules.line_too_long {
        violations.extend(over_long_line_violations(paragraph, options, hard_limit, changed));
    }
    violations.sort_by_key(|violation| (violation.line, violation.kind));
    violations.dedup_by(|second, first| {
        first.line == second.line
            && first.kind == ViolationKind::LineTooLong
            && second.kind == ViolationKind::LineTooLong
    });

    let lines = changed.then(|| {
        output
            .into_iter()
            .map(|(content, hard_break)| format!("{content}{}", hard_break.marker()))
            .collect()
    });
    ReflowOutcome { lines, violations }
}

/// Width of a prefix in characters, counting tabs as `tab_width`.
#[must_use]
pub fn prefix_width(prefix: &str, tab_width: usize) -> usize {
    prefix
        .chars()
        .map(|character| if character == '\t' { tab_width } else { 1 })
        .sum()
}

/// Total width of the tokens joined with single spaces.
#[must_use]
pub fn tokens_width(tokens: &[Token]) -> usize {
    if tokens.is_empty() {
        return 0;
    }
    tokens.iter().map(Token::width).sum::<usize>() + tokens.len() - 1
}

/// Join the tokens with single spaces.
#[must_use]
pub fn join_tokens(tokens: &[Token]) -> String {
    let mut result = String::with_capacity(tokens_width(tokens));
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            result.push(' ');
        }
        result.push_str(&token.leading);
        result.push_str(&token.core);
        result.push_str(&token.trailing);
    }
    result
}

/// Uppercase the first character of a word.
#[must_use]
pub fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut result: String = first.to_uppercase().collect();
        result.push_str(chars.as_str());
        result
    })
}

/// Index one past the last non-whitespace character of the chunk starting at `start`.
fn chunk_end(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while chars.get(end).is_some_and(|character| !character.is_whitespace()) {
        end += 1;
    }
    end
}

/// Read one token starting at `start`, returning the token and the index where it ends.
fn read_token(chars: &[char], start: usize, word_end: usize, origin_line: usize) -> (Token, usize) {
    let mut leading_end = start;
    while leading_end < word_end {
        let Some(&character) = chars.get(leading_end) else {
            break;
        };
        let is_emphasis = matches!(character, '*' | '_')
            && chars
                .get(leading_end + 1..word_end)
                .is_some_and(|rest| rest.contains(&character));
        if atom_at(chars, leading_end).is_some() {
            break;
        }
        if is_opener(character) || is_emphasis {
            leading_end += 1;
        } else {
            break;
        }
    }
    if leading_end >= word_end {
        leading_end = start;
    }

    let (atom_end, atom_kind) = atom_at(chars, leading_end).map_or((None, None), |(end, kind)| (Some(end), Some(kind)));
    let leading: String = chars.get(start..leading_end).unwrap_or_default().iter().collect();

    if let (Some(atom_end), Some(kind)) = (atom_end, atom_kind) {
        let tail_end = chunk_end(chars, atom_end);
        let tail: &[char] = chars.get(atom_end..tail_end).unwrap_or_default();
        let mut trailing_start = tail.len();
        while trailing_start > 0
            && tail
                .get(trailing_start - 1)
                .is_some_and(|character| is_trailing_closer(*character))
        {
            trailing_start -= 1;
        }
        let mut core: String = chars.get(leading_end..atom_end).unwrap_or_default().iter().collect();
        core.extend(tail.get(..trailing_start).unwrap_or_default());
        let trailing: String = tail.get(trailing_start..).unwrap_or_default().iter().collect();
        let token = Token {
            leading,
            core,
            trailing,
            kind,
            origin_line,
            force_break_after: false,
        };
        return (token, tail_end);
    }

    let chunk: &[char] = chars.get(leading_end..word_end).unwrap_or_default();
    let mut core_end = chunk.len();
    while core_end > 1
        && chunk
            .get(core_end - 1)
            .is_some_and(|character| is_trailing_closer(*character))
    {
        let closes_bracket = chunk
            .get(core_end - 1)
            .is_some_and(|character| matches!(character, ')' | ']'));
        if closes_bracket && has_unclosed_bracket(chunk.get(..core_end - 1).unwrap_or_default()) {
            break;
        }
        core_end -= 1;
    }
    let core: String = chunk.get(..core_end).unwrap_or_default().iter().collect();
    let trailing: String = chunk.get(core_end..).unwrap_or_default().iter().collect();
    let kind = classify_core(&core, leading.is_empty() && trailing.is_empty());
    let token = Token {
        leading,
        core,
        trailing,
        kind,
        origin_line,
        force_break_after: false,
    };
    (token, word_end)
}

/// Whether the characters contain more opening than closing brackets.
fn has_unclosed_bracket(chars: &[char]) -> bool {
    let depth = chars.iter().fold(0i64, |depth, character| match character {
        '(' | '[' => depth + 1,
        ')' | ']' => depth - 1,
        _ => depth,
    });
    depth > 0
}

/// Whether the character may be peeled off the end of a chunk as closing punctuation.
const fn is_trailing_closer(character: char) -> bool {
    is_closer(character) || matches!(character, '.' | ',' | ';' | ':' | '!' | '?' | '…' | '—' | '–')
}

/// Detect an unbreakable atom starting at `start`, returning its end index and kind.
fn atom_at(chars: &[char], start: usize) -> Option<(usize, TokenKind)> {
    let &first = chars.get(start)?;
    match first {
        '`' => backtick_span_end(chars, start).map(|end| (end, TokenKind::Code)),
        '[' => link_end(chars, start).map(|end| (end, TokenKind::Link)),
        '!' if chars.get(start + 1) == Some(&'[') => link_end(chars, start + 1).map(|end| (end, TokenKind::Link)),
        '<' => {
            let next = chars.get(start + 1)?;
            if next.is_ascii_alphabetic() || matches!(next, '/' | '!') {
                let end = chars
                    .iter()
                    .enumerate()
                    .skip(start + 1)
                    .find(|(_, character)| **character == '>')
                    .map(|(index, _)| index + 1)?;
                Some((end, TokenKind::Html))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// End index of a backtick code span starting at `start`, matching runs of equal length.
fn backtick_span_end(chars: &[char], start: usize) -> Option<usize> {
    let mut run = 0;
    while chars.get(start + run) == Some(&'`') {
        run += 1;
    }
    let mut index = start + run;
    while index < chars.len() {
        if chars.get(index) == Some(&'`') {
            let mut closing = 0;
            while chars.get(index + closing) == Some(&'`') {
                closing += 1;
            }
            if closing == run {
                return Some(index + closing);
            }
            index += closing;
        } else {
            index += 1;
        }
    }
    None
}

/// End index of a Markdown link or image starting at the `[` at `start`.
fn link_end(chars: &[char], start: usize) -> Option<usize> {
    let close_bracket = matching_close(chars, start, '[', ']')?;
    match chars.get(close_bracket + 1) {
        Some('(') => matching_close(chars, close_bracket + 1, '(', ')').map(|end| end + 1),
        Some('[') => matching_close(chars, close_bracket + 1, '[', ']').map(|end| end + 1),
        _ => None,
    }
}

/// Index of the bracket closing the one at `start`, counting nesting.
fn matching_close(chars: &[char], start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (index, &character) in chars.iter().enumerate().skip(start) {
        if character == open {
            depth += 1;
        } else if character == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Classify the core text of a chunk that is not a delimited atom.
fn classify_core(core: &str, bare: bool) -> TokenKind {
    if bare && matches!(core, "—" | "–" | "--") {
        return TokenKind::Dash;
    }
    let has_dot = core.contains('.');
    let has_colon = core.contains(':');
    let starts_with_number =
        core.starts_with(|character: char| character.is_ascii_digit() || character == '+' || character == '-');
    if has_dot && RE_LETTER_ABBREVIATION.is_match(core) {
        return TokenKind::Word;
    }
    if (has_colon || has_dot) && RE_URL.is_match(core) {
        return TokenKind::Url;
    }
    if starts_with_number && RE_NUMBER.is_match(core) {
        return TokenKind::Number;
    }
    if has_dot && (starts_with_number || core.starts_with('v')) && RE_VERSION.is_match(core) {
        return TokenKind::Version;
    }
    if core.contains('/') && !core.contains("//") || has_dot && RE_FILE_NAME.is_match(core) {
        return TokenKind::Path;
    }
    if (core.contains('_') || has_colon || core.contains('(') || core.as_bytes().iter().any(u8::is_ascii_uppercase))
        && RE_IDENTIFIER.is_match(core)
    {
        return TokenKind::Identifier;
    }
    TokenKind::Word
}

/// Whether the word list contains the word, ignoring ASCII case.
fn contains_word(words: &[&str], word: &str) -> bool {
    words.iter().any(|candidate| candidate.eq_ignore_ascii_case(word))
}

/// Whether the two words form the given two word phrase, ignoring ASCII case.
fn is_word_pair(pair: &str, first: &str, second: &str) -> bool {
    pair.split_once(' ')
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(first) && right.eq_ignore_ascii_case(second))
}

/// Whether the word is a configured abbreviation that never ends a sentence.
fn is_abbreviation(core: &str, options: &FormatOptions) -> bool {
    if core.is_ascii() {
        return options.abbreviations.iter().any(|abbreviation| {
            abbreviation
                .strip_suffix('.')
                .is_some_and(|word| word.eq_ignore_ascii_case(core))
        });
    }
    let lowercase = format!("{}.", core.to_lowercase());
    options.abbreviations.contains(&lowercase)
}

/// Whether placing the token at a line start would change Markdown structure.
fn starts_markdown_structure(token: &Token) -> bool {
    let text = token.text_cow();
    let can_be_structure = text
        .starts_with(|character: char| matches!(character, '-' | '+' | '*' | '>' | '|' | '#' | '`' | '~' | '0'..='9'));
    can_be_structure && (RE_MARKDOWN_STRUCTURE.is_match(&text) || text.starts_with('#'))
        || text.chars().all(is_trailing_closer)
}

/// Group paragraph lines into segments, joining lines that end mid-clause.
fn build_segments(
    paragraph: &Paragraph,
    line_tokens: Vec<Vec<Token>>,
    options: &FormatOptions,
) -> (Vec<Segment>, Vec<Violation>) {
    let mut segments = Vec::new();
    let mut violations = Vec::new();
    let mut current: Option<Segment> = None;
    for (index, tokens) in line_tokens.into_iter().enumerate() {
        let hard_break = paragraph.hard_breaks.get(index).copied().unwrap_or_default();
        let Some(mut segment) = current.take() else {
            current = Some(Segment {
                tokens,
                hard_break,
                modified: false,
                source_line: Some(index),
            });
            continue;
        };
        let previous_break = paragraph.hard_breaks.get(index - 1).copied().unwrap_or_default();
        let mid_clause =
            options.rules.mid_clause_break && is_mid_clause_break(&segment.tokens, &tokens, previous_break, options);
        if mid_clause {
            violations.push(Violation {
                line: paragraph.start_line + index,
                column: None,
                kind: ViolationKind::MidClauseBreak,
                message: "line breaks in the middle of a clause".to_string(),
                fixable: true,
            });
        }
        let join = mid_clause || (options.join_sentences && previous_break == HardBreak::None);
        if join {
            segment.tokens.extend(tokens);
            segment.hard_break = hard_break;
            segment.modified = true;
            segment.source_line = None;
            current = Some(segment);
        } else {
            segments.push(segment);
            current = Some(Segment {
                tokens,
                hard_break,
                modified: false,
                source_line: Some(index),
            });
        }
    }
    if let Some(segment) = current {
        segments.push(segment);
    }
    (segments, violations)
}

/// Apply the semicolon and em dash rewrites to all segments.
fn reword_segments(
    segments: &mut [Segment],
    options: &FormatOptions,
    line_offset: usize,
    violations: &mut Vec<Violation>,
) {
    if options.rules.em_dash {
        for segment in segments.iter_mut() {
            reword_dashes(segment, line_offset, violations);
        }
    }
    if options.rules.semicolon {
        reword_semicolons(segments, options, line_offset, violations);
    }
}

/// Replace standalone dashes with commas.
fn reword_dashes(segment: &mut Segment, line_offset: usize, violations: &mut Vec<Violation>) {
    if !segment.tokens.iter().any(|token| token.kind == TokenKind::Dash) {
        return;
    }
    let count = segment.tokens.len();
    let mut result: Vec<Token> = Vec::with_capacity(count);
    for (index, token) in segment.tokens.drain(..).enumerate() {
        if token.kind != TokenKind::Dash {
            result.push(token);
            continue;
        }
        let line = line_offset + token.origin_line;
        if index == 0 || index + 1 == count {
            violations.push(Violation {
                line,
                column: None,
                kind: ViolationKind::EmDash,
                message: "dash at the edge of a sentence cannot be rewritten automatically".to_string(),
                fixable: false,
            });
            result.push(token);
            continue;
        }
        if let Some(previous) = result.last_mut()
            && !previous.ends_clause_punctuation()
            && !previous.ends_sentence_punctuation()
        {
            previous.trailing.push(',');
        }
        violations.push(Violation {
            line,
            column: None,
            kind: ViolationKind::EmDash,
            message: "dash replaced with a comma".to_string(),
            fixable: true,
        });
        segment.modified = true;
    }
    segment.tokens = result;
}

/// Split clauses joined with a semicolon into separate sentences.
fn reword_semicolons(
    segments: &mut [Segment],
    options: &FormatOptions,
    line_offset: usize,
    violations: &mut Vec<Violation>,
) {
    for segment_index in 0..segments.len() {
        let token_count = segments.get(segment_index).map_or(0, |segment| segment.tokens.len());
        for token_index in 0..token_count {
            let Some(segment) = segments.get(segment_index) else {
                continue;
            };
            let Some(token) = segment.tokens.get(token_index) else {
                continue;
            };
            if !token.trailing.ends_with(';') {
                continue;
            }
            let line = line_offset + token.origin_line;
            let within_segment = token_index + 1 < segment.tokens.len();
            let crosses_segment = !within_segment && segment.hard_break == HardBreak::None;
            let next = if within_segment {
                segment.tokens.get(token_index + 1)
            } else if crosses_segment {
                segments.get(segment_index + 1).and_then(|next| next.tokens.first())
            } else {
                None
            };
            let refusal = semicolon_refusal(&segment.tokens, token_index, next, options);
            if let Some(reason) = refusal {
                violations.push(Violation {
                    line,
                    column: None,
                    kind: ViolationKind::Semicolon,
                    message: format!("semicolon joins clauses, {reason}"),
                    fixable: false,
                });
                continue;
            }
            let capitalized = next.and_then(|next| capitalize_token(next, options));
            if let Some(segment) = segments.get_mut(segment_index)
                && let Some(token) = segment.tokens.get_mut(token_index)
            {
                token.trailing.pop();
                token.trailing.push('.');
                token.force_break_after = within_segment;
                segment.modified = true;
            }
            let (next_segment, next_index) = if within_segment {
                (segment_index, token_index + 1)
            } else {
                (segment_index + 1, 0)
            };
            if let Some(new_core) = capitalized
                && let Some(next_segment) = segments.get_mut(next_segment)
                && let Some(next_token) = next_segment.tokens.get_mut(next_index)
            {
                next_token.core = new_core;
                next_segment.modified = true;
            }
            violations.push(Violation {
                line,
                column: None,
                kind: ViolationKind::Semicolon,
                message: "semicolon replaced with a period and a new sentence".to_string(),
                fixable: true,
            });
        }
    }
}

/// Reason the semicolon after `tokens[index]` cannot be rewritten, or `None` when it can.
fn semicolon_refusal(
    tokens: &[Token],
    index: usize,
    next: Option<&Token>,
    options: &FormatOptions,
) -> Option<&'static str> {
    let token = tokens.get(index)?;
    let Some(next) = next else {
        return Some("nothing follows it");
    };
    let depth = tokens.iter().take(index + 1).fold(0i64, |depth, token| {
        bracket_characters(token).fold(depth, |depth, character| match character {
            '(' | '[' => depth + 1,
            ')' | ']' => depth - 1,
            _ => depth,
        })
    });
    if depth > 0 {
        return Some("it is inside brackets");
    }
    if looks_like_code(token) || looks_like_code(next) {
        return Some("the text looks like code");
    }
    if capitalize_token(next, options).is_none() && !can_start_sentence_unchanged(next) {
        return Some("the next word cannot be capitalized");
    }
    None
}

/// Whether a token is code-like enough that rewording it would be wrong.
fn looks_like_code(token: &Token) -> bool {
    match token.kind {
        TokenKind::Identifier => true,
        TokenKind::Word => {
            let core = &token.core;
            core.contains(['(', ')', '=', '{', '}']) || core.contains("::") || core.contains("->")
        }
        _ => false,
    }
}

/// Whether the token can start a sentence without changing its text.
fn can_start_sentence_unchanged(token: &Token) -> bool {
    if matches!(
        token.kind,
        TokenKind::Code | TokenKind::Url | TokenKind::Link | TokenKind::Html | TokenKind::Number | TokenKind::Version
    ) {
        return true;
    }
    token
        .core
        .chars()
        .next()
        .is_some_and(|first| first.is_uppercase() || first.is_ascii_digit())
}

/// Capitalized core for a token that starts a new sentence, or `None` when no change is needed or possible.
fn capitalize_token(token: &Token, options: &FormatOptions) -> Option<String> {
    if token.kind != TokenKind::Word || !RE_LOWERCASE_WORD.is_match(&token.core) {
        return None;
    }
    if options
        .preserve_lowercase
        .iter()
        .any(|word| word.eq_ignore_ascii_case(&token.core))
    {
        return None;
    }
    Some(capitalize(&token.core))
}

/// Join the segments around a change when the line break between them falls inside a sentence.
///
/// Reflowing a sentence means the old break in the middle of it is no longer meaningful,
/// so the whole sentence is re-broken at its best boundary instead.
/// Breaks at sentence ends and hard breaks are always kept,
/// so sentences that already have their own line stay on it.
fn merge_changed_sentences(segments: &mut Vec<Segment>, options: &FormatOptions) {
    let mut index = 0;
    while index + 1 < segments.len() {
        let Some(first) = segments.get(index) else { break };
        let Some(second) = segments.get(index + 1) else { break };
        let inside_sentence = first.hard_break == HardBreak::None
            && !second.tokens.is_empty()
            && first.tokens.last().is_some_and(|last| !ends_sentence(last, options));
        if inside_sentence && (first.modified || second.modified) {
            let second = segments.remove(index + 1);
            if let Some(first) = segments.get_mut(index) {
                first.tokens.extend(second.tokens);
                first.hard_break = second.hard_break;
                first.modified = true;
                first.source_line = None;
            }
        } else {
            index += 1;
        }
    }
}

/// Break a run of tokens into lines that fit the budgets, preferring semantic boundaries.
fn split_tokens(
    tokens: Vec<Token>,
    first_budget: usize,
    rest_budget: usize,
    is_first_line: &mut bool,
    options: &FormatOptions,
    line_offset: usize,
    violations: &mut Vec<Violation>,
) -> Vec<Vec<Token>> {
    let mut pieces = Vec::new();
    let mut rest = tokens;
    while !rest.is_empty() {
        let budget = if *is_first_line { first_budget } else { rest_budget };
        *is_first_line = false;
        let width = tokens_width(&rest);
        if width <= budget {
            pieces.push(rest);
            break;
        }
        let boundaries = find_boundaries(&rest, options);
        let widths = cumulative_widths(&rest);
        let left_width = |boundary: &Boundary| widths.get(boundary.before).copied().unwrap_or(usize::MAX);
        let hard_limit = budget + SOFT_OVERFLOW;

        let fitting_sentence = boundaries
            .iter()
            .filter(|boundary| boundary.rank == Rank::Sentence && left_width(boundary) <= budget)
            .max_by_key(|boundary| boundary.before)
            .copied();
        let preferred = fitting_sentence.or_else(|| choose_in_band(&boundaries, budget, hard_limit, &left_width));
        // A line may run a little over the budget unless it can break at a sentence end
        // or before a coordinating conjunction, which is always worth the extra line.
        let strong_boundary = preferred.is_some_and(|boundary| boundary.rank >= Rank::ClauseTier2);
        if !strong_boundary && width <= hard_limit {
            pieces.push(rest);
            break;
        }

        let chosen = preferred.or_else(|| {
            boundaries
                .iter()
                .filter(|boundary| boundary.rank > Rank::Word && left_width(boundary) <= budget)
                .max_by_key(|boundary| (boundary.rank, boundary.before))
                .copied()
        });
        let chosen = chosen.or_else(|| {
            let leftmost = boundaries
                .iter()
                .filter(|boundary| boundary.rank > Rank::Word)
                .min_by_key(|boundary| boundary.before)
                .copied();
            if leftmost.is_some() {
                report_too_long(
                    &rest,
                    line_offset,
                    "no clause boundary fits within the line limit",
                    violations,
                );
            }
            leftmost
        });
        let chosen = chosen.or_else(|| {
            if options.allow_word_break {
                boundaries
                    .iter()
                    .filter(|boundary| left_width(boundary) <= budget)
                    .max_by_key(|boundary| boundary.before)
                    .copied()
            } else {
                None
            }
        });

        match chosen {
            Some(boundary) if boundary.before > 0 && boundary.before < rest.len() => {
                let right = rest.split_off(boundary.before);
                pieces.push(rest);
                rest = right;
            }
            _ => {
                report_too_long(&rest, line_offset, "no break point available", violations);
                pieces.push(rest);
                break;
            }
        }
    }
    pieces
}

/// Choose the best boundary whose left part lands inside the preference band.
fn choose_in_band(
    boundaries: &[Boundary],
    budget: usize,
    hard_limit: usize,
    left_width: &dyn Fn(&Boundary) -> usize,
) -> Option<Boundary> {
    let min_fill = budget * MIN_FILL_PERCENT / 100;
    boundaries
        .iter()
        .filter(|boundary| boundary.rank > Rank::Word)
        .filter(|boundary| {
            let width = left_width(boundary);
            width >= min_fill && width <= hard_limit
        })
        .max_by_key(|boundary| {
            let width = left_width(boundary);
            let fits = width <= budget;
            let closeness = if fits { width } else { usize::MAX - width };
            let rank = if fits { boundary.rank } else { boundary.rank.lowered() };
            (rank, fits, closeness)
        })
        .copied()
}

/// Record an unfixable too long line violation at the first token of the run.
fn report_too_long(tokens: &[Token], line_offset: usize, reason: &str, violations: &mut Vec<Violation>) {
    let line = tokens
        .first()
        .map_or(line_offset, |token| line_offset + token.origin_line);
    violations.push(Violation {
        line,
        column: None,
        kind: ViolationKind::LineTooLong,
        message: format!("line exceeds the limit and {reason}"),
        fixable: false,
    });
}

/// Cumulative widths where entry `k` is the width of the first `k` tokens joined with spaces.
fn cumulative_widths(tokens: &[Token]) -> Vec<usize> {
    let mut widths = Vec::with_capacity(tokens.len() + 1);
    widths.push(0);
    let mut total = 0;
    for (index, token) in tokens.iter().enumerate() {
        total += token.width();
        if index > 0 {
            total += 1;
        }
        widths.push(total);
    }
    widths
}

/// Violations for original lines that exceed the width limits.
fn over_long_line_violations(
    paragraph: &Paragraph,
    options: &FormatOptions,
    hard_limit: usize,
    changed: bool,
) -> Vec<Violation> {
    paragraph
        .lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let marker = paragraph.hard_breaks.get(index).copied().unwrap_or_default();
            let width = prefix_width(paragraph.prefix_for(index), options.tab_width)
                + line.chars().count()
                + marker.marker().len();
            let fixable = changed && width > options.max_width;
            if !fixable && width <= hard_limit {
                return None;
            }
            Some(Violation {
                line: paragraph.start_line + index + 1,
                column: None,
                kind: ViolationKind::LineTooLong,
                message: format!("line is {width} characters, limit is {}", options.max_width),
                fixable,
            })
        })
        .collect()
}

#[cfg(test)]
mod test_helpers {
    use super::*;

    /// Tokenize one line with dash normalization enabled.
    pub fn tokens(text: &str) -> Vec<Token> {
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
        reflow_paragraph(&paragraph(lines, ""), &FormatOptions::with_width(width))
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
}

#[cfg(test)]
mod test_sentence_merge {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_joined_sentence_is_re_broken_at_its_best_boundary() {
        let lines = [
            "Parse reads the header from the input and returns the parsed struct, and it reports an error",
            "when the input is",
            "truncated.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "Parse reads the header from the input and returns the parsed struct,".to_string(),
                "and it reports an error when the input is truncated.".to_string(),
            ]),
            "the whole sentence should be re-broken before the conjunction"
        );
    }

    #[test]
    fn a_sentence_that_has_its_own_line_is_kept_on_it() {
        let lines = ["Use the default width for now.", "default width"];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines, None,
            "a line ending a sentence must not absorb the next line"
        );
    }

    #[test]
    fn a_hard_break_stops_the_merge() {
        let mut paragraph = paragraph(&["text that continues", "onto the next line"], "");
        paragraph.hard_breaks = vec![HardBreak::Spaces, HardBreak::None];
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120));
        assert_eq!(outcome.lines, None);
    }

    #[test]
    fn short_sentences_on_their_own_lines_survive_a_join_elsewhere() {
        let lines = [
            "First sentence.",
            "Second sentence.",
            "A third sentence that was wrapped in the",
            "middle of a clause.",
        ];
        let outcome = reflow(&lines, 120);
        assert_eq!(
            outcome.lines,
            Some(vec![
                "First sentence.".to_string(),
                "Second sentence.".to_string(),
                "A third sentence that was wrapped in the middle of a clause.".to_string(),
            ]),
            "only the wrapped sentence should be joined"
        );
    }
}

#[cfg(test)]
mod test_break_choice {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_fitting_conjunction_wins_over_an_overflowing_sentence_end() {
        let lines = [concat!(
            "The current directory placeholder is used as both the first input and first output root, ",
            "and no database is attached. Tests that need a database can use struct update syntax."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "    /// "), &FormatOptions::with_width(120));
        let reflowed = outcome.lines.expect("the paragraph should be reflowed");
        assert_eq!(
            reflowed.first().map(String::as_str),
            Some("The current directory placeholder is used as both the first input and first output root,")
        );
        assert!(
            reflowed.iter().all(|line| line.chars().count() + 8 <= 120),
            "every line should fit the budget: {reflowed:?}"
        );
    }

    #[test]
    fn an_overflowing_conjunction_still_wins_over_a_fitting_comma() {
        let text = "alpha beta gamma delta epsilon zeta eta theta, iota kappa lambda mu nu and xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let outcome = reflow(&[text], 60);
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(lines[0].chars().count(), 70, "a small overshoot is allowed: {lines:?}");
    }
}

#[cfg(test)]
mod test_unchanged_lines {
    use super::test_helpers::*;
    use super::*;
    use crate::semantic_line_breaks::RuleSet;

    #[test]
    fn a_line_that_needs_no_change_keeps_its_internal_spacing() {
        let lines = ["Root/studio dirname/file1.txt       <- prefixed at root"];
        let outcome = reflow(&lines, 120);
        assert_eq!(outcome.lines, None);
    }

    #[test]
    fn an_aligned_line_is_kept_when_another_line_of_the_paragraph_is_reflowed() {
        let lines = [
            "Root/a.txt       <- first",
            "One sentence here. Another sentence there.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(30));
        let reflowed = outcome.lines.expect("the paragraph should be reflowed");
        assert_eq!(
            reflowed.first().map(String::as_str),
            Some("Root/a.txt       <- first"),
            "the untouched line must keep its spacing"
        );
    }

    #[test]
    fn no_enabled_rule_leaves_the_paragraph_alone() {
        let options = FormatOptions {
            rules: RuleSet::NONE,
            ..FormatOptions::with_width(30)
        };
        let lines = [
            "A very long line with several   spaces that would otherwise be reflowed here.",
            "Another line; with a semicolon \u{2014} and a dash.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &options);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }
}

#[cfg(test)]
mod test_tokenizer {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn keeps_code_span_as_one_atom() {
        let tokens = tokens("call `a; b.c()` now");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].kind, TokenKind::Code);
        assert_eq!(tokens[1].core, "`a; b.c()`");
    }

    #[test]
    fn keeps_double_backtick_span_with_inner_backtick() {
        let tokens = tokens("x ``a ` b`` y");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].kind, TokenKind::Code);
        assert_eq!(tokens[1].core, "``a ` b``");
    }

    #[test]
    fn code_span_keeps_following_punctuation_in_trailing() {
        let tokens = tokens("(`num_cpus * 2`).");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].leading, "(");
        assert_eq!(tokens[0].core, "`num_cpus * 2`");
        assert_eq!(tokens[0].trailing, ").");
        assert_eq!(tokens[0].text(), "(`num_cpus * 2`).");
    }

    #[test]
    fn url_keeps_inner_dots_and_peels_final_period() {
        let tokens = tokens("See https://a.b/c.");
        assert_eq!(tokens[1].kind, TokenKind::Url);
        assert_eq!(tokens[1].core, "https://a.b/c");
        assert_eq!(tokens[1].trailing, ".");
    }

    #[test]
    fn classifies_paths_versions_numbers_and_identifiers() {
        assert_eq!(
            kinds("Edit src/lib.rs and bump 1.2.3."),
            vec![
                TokenKind::Word,
                TokenKind::Path,
                TokenKind::Word,
                TokenKind::Word,
                TokenKind::Version
            ]
        );
        assert_eq!(kinds("main.c"), vec![TokenKind::Path]);
        assert_eq!(kinds("42 3.14 50%"), vec![TokenKind::Number; 3]);
        assert_eq!(
            kinds("snake_case std::fs camelCase foo()"),
            vec![TokenKind::Identifier; 4]
        );
        assert_eq!(kinds("e.g. i.e."), vec![TokenKind::Word, TokenKind::Word]);
    }

    #[test]
    fn markdown_link_with_nested_parens_is_one_atom() {
        let tokens = tokens("see [x](https://a/b_(c)) now");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].kind, TokenKind::Link);
        assert_eq!(tokens[1].core, "[x](https://a/b_(c))");
    }

    #[test]
    fn peels_leading_and_trailing_punctuation() {
        let tokens = tokens("(the \"value\").");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].leading, "(");
        assert_eq!(tokens[0].core, "the");
        assert_eq!(tokens[1].leading, "\"");
        assert_eq!(tokens[1].core, "value");
        assert_eq!(tokens[1].trailing, "\").");
    }

    #[test]
    fn recognizes_dash_tokens() {
        assert_eq!(kinds("a — b"), vec![TokenKind::Word, TokenKind::Dash, TokenKind::Word]);
        assert_eq!(kinds("a—b"), vec![TokenKind::Word, TokenKind::Dash, TokenKind::Word]);
        assert_eq!(kinds("a -- b"), vec![TokenKind::Word, TokenKind::Dash, TokenKind::Word]);
        assert_eq!(kinds("--flag"), vec![TokenKind::Word]);
        assert_eq!(kinds("pages 1–5"), vec![TokenKind::Word, TokenKind::Word]);
    }

    #[test]
    fn emphasis_markers_are_peeled_only_when_paired() {
        let tokens = tokens("*strong* _x");
        assert_eq!(tokens[0].leading, "*");
        assert_eq!(tokens[0].core, "strong");
        assert_eq!(tokens[0].trailing, "*");
        assert_eq!(tokens[1].leading, "");
        assert_eq!(tokens[1].core, "_x");
    }

    #[test]
    fn width_counts_characters_not_bytes() {
        let umlauts = tokens("ääää");
        assert_eq!(umlauts[0].width(), 4);
        assert_eq!(tokens_width(&umlauts), 4);
        assert_eq!(tokens_width(&tokens("ab cd")), 5);
        assert_eq!(prefix_width("\t// ", 4), 7);
    }
}

#[cfg(test)]
mod test_sentence_end {
    use super::test_helpers::*;
    use super::*;

    fn is_end(text: &str, index: usize) -> bool {
        is_sentence_end(&tokens(text), index, &FormatOptions::default())
    }

    #[test]
    fn detects_period_followed_by_capital() {
        assert!(is_end("Foo bar. Baz", 1));
        assert!(is_end("Bump to 1.2.3. Then", 2));
        assert!(is_end("Done. `foo` runs", 0));
        assert!(is_end("Really? (Yes)", 0));
        assert!(is_end("Done.) Next", 0));
    }

    #[test]
    fn ignores_abbreviations_ellipses_initials_and_lowercase() {
        assert!(!is_end("Use e.g. this", 1));
        assert!(!is_end("Use etc. This", 1));
        assert!(!is_end("Wait... Then", 0));
        assert!(!is_end("foo. bar", 0));
        assert!(!is_end("A. Smith wrote", 0));
        assert!(!is_end("last word.", 1));
    }
}

#[cfg(test)]
mod test_boundaries {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn ranks_punctuation_and_clause_words() {
        assert_eq!(rank_before("a, b and c", 1), Some(Rank::Punctuation));
        assert_eq!(rank_before("a, b and c", 2), Some(Rank::ClauseTier1));
        assert_eq!(rank_before("a b, and c", 2), Some(Rank::ClauseTier1));
        assert_eq!(rank_before("a b but c", 2), Some(Rank::ClauseTier2));
        assert_eq!(rank_before("a b because c", 2), Some(Rank::ClauseTier3));
        assert_eq!(rank_before("a b which c", 2), Some(Rank::ClauseTier4));
        assert_eq!(rank_before("a b for example c", 2), Some(Rank::ClauseTier3));
        assert_eq!(rank_before("First one. Second", 2), Some(Rank::Sentence));
        assert_eq!(rank_before("a b c d", 2), Some(Rank::Word));
    }

    #[test]
    fn only_first_word_of_a_clause_run_is_a_candidate() {
        assert_eq!(rank_before("x y, and then z", 2), Some(Rank::ClauseTier1));
        assert_eq!(rank_before("x y, and then z", 3), Some(Rank::Word));
    }

    #[test]
    fn never_breaks_off_a_single_first_token() {
        assert_eq!(rank_before("x and y", 1), Some(Rank::Word));
    }

    #[test]
    fn demotes_candidates_inside_brackets_and_ranks_bracket_edges() {
        assert_eq!(rank_before("x y (a, b or c) z", 3), Some(Rank::Word));
        assert_eq!(rank_before("x y (a, b or c) z", 4), Some(Rank::Word));
        assert_eq!(rank_before("x y (a, b or c) z", 2), Some(Rank::ClauseTier4));
        assert_eq!(rank_before("x y (a, b or c) z", 6), Some(Rank::ClauseTier4));
    }

    #[test]
    fn skips_candidates_that_would_start_markdown_structure() {
        assert_eq!(rank_before("a b - c", 2), None);
        assert_eq!(rank_before("a b 1. c", 2), None);
        assert_eq!(rank_before("a b #tag c", 2), None);
    }

    #[test]
    fn extra_clause_starters_from_options_rank_lowest() {
        let options = FormatOptions {
            clause_starters: vec!["meanwhile".to_string()],
            ..FormatOptions::default()
        };
        let boundaries = find_boundaries(&tokens("a b meanwhile c"), &options);
        assert_eq!(boundaries[1].rank, Rank::ClauseTier4);
    }
}

#[cfg(test)]
mod test_mid_clause_break {
    use super::test_helpers::*;
    use super::*;

    fn mid(line_a: &str, line_b: &str) -> bool {
        is_mid_clause_break(
            &tokens(line_a),
            &tokens(line_b),
            HardBreak::None,
            &FormatOptions::default(),
        )
    }

    #[test]
    fn flags_plain_word_breaks() {
        assert!(mid("the quick brown", "fox jumps"));
        assert!(mid("this works well and", "it works too"));
        assert!(mid("according to the", "RFC document"));
        assert!(mid("stored on `FilteredParts`", "to avoid work"));
    }

    #[test]
    fn accepts_semantic_boundaries() {
        assert!(!mid("the fox jumps.", "Over the dog"));
        assert!(!mid("first a, second b,", "third c"));
        assert!(!mid("we do this here", "because it works"));
        assert!(!mid("we do this here", "e.g. like this"));
        assert!(!mid("the values (see below)", "are sorted"));
        assert!(!mid("the values are sorted", "(see below)"));
        assert!(!mid("the header is read:", "size in bytes"));
    }

    #[test]
    fn skips_short_lines_keys_structure_and_capitalized_starts() {
        assert!(!mid("Parameters", "foo bar baz"));
        assert!(!mid("two words", "foo bar baz"));
        assert!(!mid("reads the header", "size: bytes"));
        assert!(!mid("reads the header", "- item"));
        assert!(!mid("Returns the parsed config", "Use this for tests"));
        assert!(!mid("the quick brown", ""));
    }

    #[test]
    fn hard_break_is_never_mid_clause() {
        assert!(!is_mid_clause_break(
            &tokens("roses are red"),
            &tokens("violets are blue"),
            HardBreak::Spaces,
            &FormatOptions::default()
        ));
    }
}

#[cfg(test)]
mod test_reflow {
    use super::test_helpers::*;
    use super::*;
    use crate::semantic_line_breaks::types::RuleSet;

    #[test]
    fn joins_mid_clause_lines_that_fit() {
        let outcome = reflow(&["the quick brown fox", "jumps over the dog."], 120);
        assert_eq!(
            outcome.lines,
            Some(vec!["the quick brown fox jumps over the dog.".to_string()])
        );
        assert_eq!(summary(&outcome), vec![(ViolationKind::MidClauseBreak, true)]);
        assert_eq!(outcome.violations[0].line, 1);
    }

    #[test]
    fn leaves_sentence_boundaries_alone() {
        let outcome = reflow(&["First one is here.", "Second one is here."], 120);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn breaks_before_and_when_in_band() {
        let text = "This sentence is long enough that it needs wrapping and it keeps going with more words, because we want to see the break.";
        let outcome = reflow(&[text], 80);
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(lines[0], "This sentence is long enough that it needs wrapping");
        assert!(lines[1].starts_with("and it keeps going"));
        assert!(lines.iter().all(|line| line.chars().count() <= 80));
    }

    #[test]
    fn prefers_and_with_small_overshoot_over_comma_and_stays_stable() {
        let text = "alpha beta gamma delta epsilon zeta eta theta, iota kappa lambda mu nu and xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let outcome = reflow(&[text], 60);
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(
            lines[0],
            "alpha beta gamma delta epsilon zeta eta theta, iota kappa lambda mu nu"
        );
        assert_eq!(lines[0].chars().count(), 70);
        assert_eq!(lines[1], "and xi omicron pi rho sigma tau upsilon phi chi psi omega");
        let again = reflow_paragraph(
            &paragraph(&lines.iter().map(String::as_str).collect::<Vec<_>>(), ""),
            &FormatOptions::with_width(60),
        );
        assert_eq!(again.lines, None);
        assert!(again.violations.is_empty());
    }

    #[test]
    fn falls_back_to_comma_when_and_is_below_minimum_fill() {
        let text = "alpha beta and gamma delta epsilon zeta eta theta iota kappa lambda, mu nu xi omicron pi rho sigma tau upsilon";
        let outcome = reflow(&[text], 60);
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(
            lines[0],
            "alpha beta and gamma delta epsilon zeta eta theta iota kappa lambda,"
        );
    }

    #[test]
    fn prefers_sentence_end_over_clause_words() {
        let text =
            "First sentence ends here. Second part continues and keeps going for a while longer than the limit allows.";
        let outcome = reflow(&[text], 70);
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(lines[0], "First sentence ends here.");
    }

    #[test]
    fn reports_unfixable_when_no_boundary_fits() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi, rho sigma";
        let outcome = reflow(&[text], 40);
        let lines = outcome.lines.expect("should still break at the comma");
        assert_eq!(lines.len(), 2);
        assert!(
            outcome
                .violations
                .iter()
                .any(|violation| violation.kind == ViolationKind::LineTooLong && !violation.fixable)
        );
    }

    #[test]
    fn leaves_unbreakable_token_and_reports_unfixable() {
        let url = format!("https://example.com/{}", "a".repeat(100));
        let outcome = reflow(&[url.as_str()], 80);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::LineTooLong, false)]);
    }

    #[test]
    fn tolerates_lines_inside_the_soft_overflow() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron";
        assert_eq!(text.chars().count(), 80);
        let outcome = reflow(&[text], 75);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn uses_first_prefix_budget_for_the_first_line() {
        let text = "alpha beta gamma delta epsilon zeta, eta theta iota kappa lambda mu nu xi omicron pi, rho sigma tau upsilon";
        let mut paragraph = paragraph(&[text], "");
        paragraph.first_prefix = "1. ".to_string();
        paragraph.rest_prefix = "   ".to_string();
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(60));
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| line.chars().count() + 3 <= 60), "{lines:?}");
    }

    #[test]
    fn join_sentences_packs_short_sentences() {
        let options = FormatOptions {
            join_sentences: true,
            ..FormatOptions::with_width(120)
        };
        let outcome = reflow_paragraph(&paragraph(&["A short one.", "Another short one."], ""), &options);
        assert_eq!(outcome.lines, Some(vec!["A short one. Another short one.".to_string()]));
    }

    #[test]
    fn disabled_mid_clause_rule_keeps_lines_apart() {
        let options = FormatOptions {
            rules: RuleSet {
                mid_clause_break: false,
                ..RuleSet::ALL
            },
            ..FormatOptions::with_width(120)
        };
        let outcome = reflow_paragraph(
            &paragraph(&["the quick brown fox", "jumps over the dog."], ""),
            &options,
        );
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn keeps_hard_break_marker_on_its_line() {
        let mut paragraph = paragraph(&["roses are red", "violets are blue"], "");
        paragraph.hard_breaks = vec![HardBreak::Spaces, HardBreak::None];
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120));
        assert_eq!(outcome.lines, None);
    }

    #[test]
    fn reflow_is_idempotent() {
        let inputs: Vec<Vec<&str>> = vec![
            vec![
                "the quick brown fox",
                "jumps over the lazy dog; it was",
                "fast — very fast.",
            ],
            vec![
                "This sentence is long enough that it needs wrapping and it keeps going with more words, because we want to see the break.",
            ],
            vec![
                "Use `x`; then run `cargo test` and",
                "check the output (see below)",
                "for details.",
            ],
        ];
        for input in inputs {
            let first = reflow(&input, 60);
            let lines = first.lines.expect("first pass should change the text");
            let second = reflow(&lines.iter().map(String::as_str).collect::<Vec<_>>(), 60);
            assert_eq!(second.lines, None, "second pass changed {lines:?}");
        }
    }
}

#[cfg(test)]
mod test_rewording {
    use super::test_helpers::*;
    use super::*;
    use crate::semantic_line_breaks::types::RuleSet;

    #[test]
    fn semicolon_becomes_period_and_capitalized_sentence() {
        let outcome = reflow(&["run it; then check the result"], 120);
        assert_eq!(outcome.lines, Some(vec!["run it. Then check the result".to_string()]));
        assert_eq!(summary(&outcome), vec![(ViolationKind::Semicolon, true)]);
    }

    #[test]
    fn a_rewritten_semicolon_breaks_the_line_when_the_sentences_do_not_fit() {
        let outcome = reflow(
            &["run the whole pipeline once; then check the result of every single step"],
            40,
        );
        assert_eq!(
            outcome.lines,
            Some(vec![
                "run the whole pipeline once.".to_string(),
                "Then check the result of every single step".to_string()
            ])
        );
    }

    #[test]
    fn semicolon_at_line_end_capitalizes_next_line() {
        let outcome = reflow(&["Do this first;", "then do that."], 120);
        assert_eq!(
            outcome.lines,
            Some(vec!["Do this first.".to_string(), "Then do that.".to_string()])
        );
    }

    #[test]
    fn semicolon_refusals_are_reported_as_unfixable() {
        for text in [
            "(a; b) c d",
            "call foo(); then check",
            "x y; snake_case wins",
            "run it; npm handles it",
            "ends with one;",
        ] {
            let outcome = reflow(&[text], 120);
            assert_eq!(outcome.lines, None, "{text}");
            assert_eq!(summary(&outcome), vec![(ViolationKind::Semicolon, false)], "{text}");
        }
    }

    #[test]
    fn semicolon_before_atoms_and_capitals_needs_no_capitalization() {
        let outcome = reflow(&["x y; `y` wins"], 120);
        assert_eq!(outcome.lines, Some(vec!["x y. `y` wins".to_string()]));
        let outcome = reflow(&["x y; Foo wins"], 120);
        assert_eq!(outcome.lines, Some(vec!["x y. Foo wins".to_string()]));
    }

    #[test]
    fn semicolon_inside_code_span_is_ignored() {
        let outcome = reflow(&["run `a; b` now"], 120);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn dashes_become_commas() {
        assert_eq!(reflow(&["a — b"], 120).lines, Some(vec!["a, b".to_string()]));
        assert_eq!(reflow(&["a—b"], 120).lines, Some(vec!["a, b".to_string()]));
        assert_eq!(reflow(&["a -- b"], 120).lines, Some(vec!["a, b".to_string()]));
        assert_eq!(reflow(&["a, — b"], 120).lines, Some(vec!["a, b".to_string()]));
        assert_eq!(reflow(&["a – b"], 120).lines, Some(vec!["a, b".to_string()]));
        assert_eq!(summary(&reflow(&["a — b"], 120)), vec![(ViolationKind::EmDash, true)]);
    }

    #[test]
    fn ranges_flags_and_edge_dashes_are_left_alone() {
        assert_eq!(reflow(&["pages 1–5 and --flag"], 120).lines, None);
        let outcome = reflow(&["— quoted text here"], 120);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::EmDash, false)]);
    }

    #[test]
    fn disabled_rules_skip_rewording() {
        let options = FormatOptions {
            rules: RuleSet {
                semicolon: false,
                em_dash: false,
                ..RuleSet::ALL
            },
            ..FormatOptions::with_width(120)
        };
        let outcome = reflow_paragraph(&paragraph(&["a; b — c"], ""), &options);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn capitalize_handles_unicode_and_empty() {
        assert_eq!(capitalize("ääkkönen"), "Ääkkönen");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("x"), "X");
    }
}
