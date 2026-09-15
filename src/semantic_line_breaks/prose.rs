//! Prose engine for the semantic line breaks formatter.
//!
//! Implements tokenization of a prose line into words and unbreakable atoms,
//! sentence and clause boundary detection, mid-clause break detection between consecutive lines,
//! rewording of semicolons and em dashes, and the reflow algorithm
//! that joins hard-wrapped lines and re-breaks them at semantic boundaries.
//!
//! The lines of a run are planned together and scored,
//! so the break points that read best and spread the text most evenly over the lines win.

use std::borrow::Cow;
use std::ops::Range;
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

/// Minimum fill of the budget, in percent, for a boundary to count as a reachable strong break.
const MIN_FILL_PERCENT: usize = 50;

/// Minimum number of words before a colon for the text to introduce what follows it.
///
/// A label of one or two words, such as "Note:" or "Legacy compatibility:",
/// reads as part of the first clause rather than as an introduction of its own,
/// so it stays on the line with the text it labels.
const MIN_COLON_INTRODUCTION_WORDS: usize = 3;

/// Weight of the width a line runs over the budget, relative to the width it leaves unused.
///
/// Going over the soft limit reads worse than stopping short by the same amount,
/// so the overflow counts several times heavier.
const OVERFLOW_WEIGHT: usize = 4;

/// Cost of using the soft overflow where a sentence end or a coordinating conjunction could end the line instead.
const WEAK_OVERFLOW_COST: usize = 5_000_000;

/// Cost of every character a line runs past the hard limit, when nothing can be broken within it.
///
/// Counting the characters keeps the text that does not fit on as few lines as possible,
/// so an unbreakable atom such as a long URL is left alone instead of dragging prose over the limit with it.
const TOO_LONG_COST: usize = 10_000_000;

/// Cost of a break before a phrase joining conjunction that no comma or colon leads into.
///
/// Such a break may cut a phrase in two instead of separating two clauses.
const BARE_CONJUNCTION_COST: usize = 250_000;

/// Cost of passing a colon that could end a sufficiently filled line in a long run.
///
/// An explanation reads better on its own line, even when keeping it with its introduction saves a line.
const SKIPPED_COLON_COST: usize = 1_000_000;

/// Cost of breaking in the middle of a clause, only reachable when word breaks are allowed.
const WORD_BREAK_COST: usize = 1_000_000_000_000;

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

/// Conjunctions that join two phrases as often as two clauses,
/// such as "the first input and the first output root".
const PHRASE_CONJUNCTIONS: &[&str] = &["and", "or", "but", "nor"];

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

/// Cheapest plan for the lines of a run, counted from one token to the end of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinePlan {
    /// Total cost of the lines the plan covers.
    cost: usize,
    /// Token index where the first line of the plan ends.
    line_end: usize,
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
    demote_command_separators(&mut tokens);
    tokens
}

/// Turn a double hyphen that precedes a command line flag into a plain word.
///
/// A line such as `pnpm run migrate -- --env dev` uses the double hyphen to separate arguments,
/// so rewriting it as a dash would corrupt the command.
fn demote_command_separators(tokens: &mut [Token]) {
    for index in 0..tokens.len() {
        let next_is_flag = tokens
            .get(index + 1)
            .is_some_and(|next| next.core.starts_with('-') || next.leading.starts_with('-'));
        if !next_is_flag {
            continue;
        }
        if let Some(token) = tokens.get_mut(index)
            && token.kind == TokenKind::Dash
        {
            token.kind = TokenKind::Word;
        }
    }
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
    let depths = token_depths(tokens, collect_bracket_events);
    let emphasis = token_depths(tokens, collect_emphasis_events);
    for index in 1..tokens.len() {
        let (Some(previous), Some(current)) = (tokens.get(index - 1), tokens.get(index)) else {
            continue;
        };
        let depth = depths.get(index).copied().unwrap_or_default() + emphasis.get(index).copied().unwrap_or_default();
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
        } else if previous.trailing.contains(':') {
            Rank::Colon
        } else if previous.ends_clause_punctuation() {
            clause.map_or(Rank::Punctuation, |clause| clause.max(Rank::Punctuation))
        } else {
            clause.unwrap_or(Rank::Word)
        };
        if depth > 0 || touches_dash(previous, current) {
            // Text inside brackets or emphasis markers belongs together, so a break there is a last resort.
            // A dash that was kept would be left dangling at the end of a line,
            // or read as a list marker at the start of the next one.
            rank = Rank::Word;
        } else if rank == Rank::Word {
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

/// Whether the boundary between the two tokens sits next to a dash that was not rewritten.
fn touches_dash(previous: &Token, current: &Token) -> bool {
    previous.kind == TokenKind::Dash || current.kind == TokenKind::Dash || previous.trailing.ends_with(['—', '–'])
}

/// Collects the group opening and closing events of one token at the given position.
type EventCollector = fn(&Token, usize, &mut Vec<(usize, bool)>);

/// Group nesting depth before each token, counting only the groups that are closed later.
///
/// A group that is never closed is ignored,
/// so a stray parenthesis or emphasis marker in prose does not make the rest of the text unbreakable.
///
/// Returns an empty vector when the tokens hold no groups, since every depth is then zero.
fn token_depths(tokens: &[Token], collect: EventCollector) -> Vec<usize> {
    let mut events = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        collect(token, index, &mut events);
    }
    if events.is_empty() {
        return Vec::new();
    }
    matched_depths(&mut events, tokens.len())
}

/// Whether each paragraph line ends inside a group that closes on a later line.
///
/// Returns an empty vector when the lines hold no groups.
fn lines_ending_inside(line_tokens: &[Vec<Token>], collect: EventCollector) -> Vec<bool> {
    let mut events = Vec::new();
    for (index, tokens) in line_tokens.iter().enumerate() {
        for token in tokens {
            collect(token, index, &mut events);
        }
    }
    if events.is_empty() {
        return Vec::new();
    }
    matched_depths(&mut events, line_tokens.len())
        .into_iter()
        .skip(1)
        .map(|depth| depth > 0)
        .collect()
}

/// Nesting depth before each position, counting only the events that pair up.
///
/// The result holds one entry more than `count`,
/// so the entry after the last position gives the depth the positions end at.
fn matched_depths(events: &mut Vec<(usize, bool)>, count: usize) -> Vec<usize> {
    retain_matched_events(events);

    let mut depths = Vec::with_capacity(count + 1);
    let mut depth = 0usize;
    let mut next = 0;
    for index in 0..=count {
        depths.push(depth);
        while let Some(&(position, opens)) = events.get(next) {
            if position != index {
                break;
            }
            depth = if opens { depth + 1 } else { depth.saturating_sub(1) };
            next += 1;
        }
    }
    depths
}

/// Append the bracket characters of the token as `(position, opens)` events.
///
/// Brackets are ASCII, so the parts are scanned as bytes to avoid decoding every character.
/// The inner text of an atom is skipped, so a bracket in a code span or a link does not count.
fn collect_bracket_events(token: &Token, position: usize, events: &mut Vec<(usize, bool)>) {
    let core = if token.is_atom() { "" } else { token.core.as_str() };
    for part in [token.leading.as_str(), core, token.trailing.as_str()] {
        for byte in part.bytes() {
            match byte {
                b'(' | b'[' => events.push((position, true)),
                b')' | b']' => events.push((position, false)),
                _ => {}
            }
        }
    }
}

/// Drop the events of groups that are never opened or never closed.
fn retain_matched_events(events: &mut Vec<(usize, bool)>) {
    let mut matched = vec![false; events.len()];
    let mut open_events: Vec<usize> = Vec::new();
    for (index, (_, opens)) in events.iter().enumerate() {
        if *opens {
            open_events.push(index);
        } else if let Some(open) = open_events.pop() {
            if let Some(slot) = matched.get_mut(open) {
                *slot = true;
            }
            if let Some(slot) = matched.get_mut(index) {
                *slot = true;
            }
        }
    }
    let mut index = 0;
    events.retain(|_| {
        let keep = matched.get(index).copied().unwrap_or_default();
        index += 1;
        keep
    });
}

/// Append the emphasis span events of the token as `(position, opens)` events.
///
/// A token that carries both of its own markers, such as `*strong*`, delimits no span past itself.
/// Delimited text is skipped, so a marker inside a code span, a link, or a URL does not count.
/// Other kinds are read as prose, since `_italic` looks the same as an identifier on its own.
fn collect_emphasis_events(token: &Token, position: usize, events: &mut Vec<(usize, bool)>) {
    if is_delimited(token.kind) {
        return;
    }
    let (opening, open_marker) = emphasis_run(
        token
            .leading
            .chars()
            .chain(token.core.chars())
            .chain(token.trailing.chars()),
    );
    let (closing, close_marker) = emphasis_run(
        token
            .trailing
            .chars()
            .rev()
            .chain(token.core.chars().rev())
            .chain(token.leading.chars().rev()),
    );
    // A single tilde delimits nothing, unlike a single asterisk or underscore,
    // so a home directory such as `~/.config` never opens a span.
    let opening = if open_marker == '~' && opening < 2 { 0 } else { opening };
    let closing = if close_marker == '~' && closing < 2 { 0 } else { closing };
    if opening > 0 && closing > 0 && open_marker == close_marker {
        return;
    }
    if opening > 0 {
        events.push((position, true));
    }
    if closing > 0 {
        events.push((position, false));
    }
}

/// Length of the emphasis marker run the characters start with, and the marker character.
///
/// Punctuation around the run is skipped, so `(**bold` reads as a run of two and `words*,` as a run of one.
/// Text has to follow the run for it to delimit a span,
/// so a token made only of markers, such as the multiplication sign in `a * b`, gives a length of zero.
fn emphasis_run(characters: impl Iterator<Item = char>) -> (usize, char) {
    let mut marker = '\0';
    let mut run = 0usize;
    for character in characters {
        if run == 0 {
            if is_emphasis_marker(character) {
                marker = character;
                run = 1;
            } else if !is_opener(character) && !is_trailing_closer(character) {
                return (0, '\0');
            }
        } else if character == marker {
            run += 1;
        } else {
            return (run, marker);
        }
    }
    (0, '\0')
}

/// Whether the character marks emphasis or strikethrough in Markdown.
const fn is_emphasis_marker(character: char) -> bool {
    matches!(character, '*' | '_' | '~')
}

/// Whether the token is delimited text whose inner characters are literal.
const fn is_delimited(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Code | TokenKind::Link | TokenKind::Html | TokenKind::Url
    )
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

    let mut changed = output.len() != paragraph.lines.len()
        || output
            .iter()
            .zip(paragraph.lines.iter().zip(&paragraph.hard_breaks))
            .any(|((content, hard_break), (line, original_break))| content != line || hard_break != original_break);

    if changed && !fits_as_well_as_before(&output, paragraph, options, hard_limit) {
        // Joining lines that cannot be broken again would replace readable lines with a longer one.
        // The over-long lines the split attempt reported are never written, so those reports go too,
        // and the original lines are measured further down like any other unchanged paragraph.
        changed = false;
        violations.retain(|violation| violation.kind != ViolationKind::LineTooLong);
        for violation in &mut violations {
            violation.fixable = false;
        }
    }

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

/// Whether the reflowed lines are no longer than the lines the paragraph started with.
///
/// A line may use the soft overflow, but reflowing must never push a line past the hard limit
/// when the paragraph did not start out that long.
fn fits_as_well_as_before(
    output: &[(String, HardBreak)],
    paragraph: &Paragraph,
    options: &FormatOptions,
    hard_limit: usize,
) -> bool {
    let line_width = |index: usize, content: &str, hard_break: HardBreak| {
        prefix_width(paragraph.prefix_for(index), options.tab_width)
            + content.chars().count()
            + hard_break.marker().len()
    };
    let output_width = output
        .iter()
        .enumerate()
        .map(|(index, (content, hard_break))| line_width(index, content, *hard_break))
        .max()
        .unwrap_or_default();
    if output_width <= hard_limit {
        return true;
    }
    let original_width = paragraph
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let hard_break = paragraph.hard_breaks.get(index).copied().unwrap_or_default();
            line_width(index, line, hard_break)
        })
        .max()
        .unwrap_or_default();
    output_width <= original_width
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
    let inside_brackets = lines_ending_inside(&line_tokens, collect_bracket_events);
    let inside_emphasis = lines_ending_inside(&line_tokens, collect_emphasis_events);
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
        let unclosed_bracket = inside_brackets.get(index - 1).copied().unwrap_or_default();
        let unclosed_emphasis = inside_emphasis.get(index - 1).copied().unwrap_or_default();
        let inside_group = previous_break == HardBreak::None && (unclosed_bracket || unclosed_emphasis);
        let mid_clause = options.rules.mid_clause_break
            && (inside_group || is_mid_clause_break(&segment.tokens, &tokens, previous_break, options));
        if mid_clause {
            let message = if unclosed_bracket && inside_group {
                "line breaks inside brackets"
            } else if inside_group {
                "line breaks inside an emphasized phrase"
            } else {
                "line breaks in the middle of a clause"
            };
            violations.push(Violation {
                line: paragraph.start_line + index,
                column: None,
                kind: ViolationKind::MidClauseBreak,
                message: message.to_string(),
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
            reword_dashes(segment, options, line_offset, violations);
        }
    }
    if options.rules.semicolon {
        reword_semicolons(segments, options, line_offset, violations);
    }
}

/// Replace standalone dashes with the punctuation that reads best in their place.
///
/// A single dash usually extends the sentence it sits in,
/// so it becomes a period and the text after it becomes a new sentence.
/// When that text cannot start a sentence the dash becomes a colon instead,
/// which introduces the rest without changing a word.
/// A pair of dashes in one sentence encloses an aside, so both become commas.
fn reword_dashes(segment: &mut Segment, options: &FormatOptions, line_offset: usize, violations: &mut Vec<Violation>) {
    let dashes: Vec<usize> = segment
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| token.kind == TokenKind::Dash)
        .map(|(index, _)| index)
        .collect();
    if dashes.is_empty() {
        return;
    }
    let count = segment.tokens.len();
    let depths = token_depths(&segment.tokens, collect_bracket_events);
    let mut removed = vec![false; count];
    for index in dashes {
        let line = line_offset + segment.tokens.get(index).map_or(0, |token| token.origin_line);
        if index == 0 || index + 1 == count {
            violations.push(Violation {
                line,
                column: None,
                kind: ViolationKind::EmDash,
                message: "dash at the edge of a sentence cannot be rewritten automatically".to_string(),
                fixable: false,
            });
            continue;
        }
        let (punctuation, capitalized, new_sentence) = dash_rewrite(&segment.tokens, index, &depths, options);
        if let Some(previous) = segment.tokens.get_mut(index - 1) {
            if let Some(punctuation) = punctuation {
                previous.trailing.push(punctuation);
            }
            previous.force_break_after |= new_sentence;
        }
        if let Some(core) = capitalized
            && let Some(next) = segment.tokens.get_mut(index + 1)
        {
            next.core = core;
        }
        if let Some(slot) = removed.get_mut(index) {
            *slot = true;
        }
        violations.push(Violation {
            line,
            column: None,
            kind: ViolationKind::EmDash,
            message: dash_message(punctuation).to_string(),
            fixable: true,
        });
        segment.modified = true;
    }
    let mut index = 0;
    segment.tokens.retain(|_| {
        let keep = !removed.get(index).copied().unwrap_or_default();
        index += 1;
        keep
    });
}

/// Punctuation that replaces the dash at `index`, the capitalized core of the token after it,
/// and whether the rewrite starts a new sentence.
///
/// No punctuation means the text before the dash already ends a clause,
/// so the dash is dropped and nothing takes its place.
/// A comma is used where a new sentence would read wrong:
/// inside brackets, between a pair of dashes that enclose an aside,
/// before a single trailing word, and before a clause word that carries the sentence on.
/// A colon is used where the text before the dash names what follows it,
/// and where code like text on either side rules out capitalizing a name into a new sentence.
fn dash_rewrite(
    tokens: &[Token],
    index: usize,
    depths: &[usize],
    options: &FormatOptions,
) -> (Option<char>, Option<String>, bool) {
    let previous = tokens.get(index - 1);
    let next = tokens.get(index + 1);
    if previous.is_some_and(|previous| previous.ends_clause_punctuation() || previous.ends_sentence_punctuation()) {
        return (None, None, false);
    }
    let inside_brackets = depths.get(index).copied().unwrap_or_default() > 0;
    let sentence = sentence_bounds(tokens, index, options);
    let encloses_aside = tokens
        .get(sentence.clone())
        .unwrap_or_default()
        .iter()
        .enumerate()
        .any(|(offset, token)| sentence.start + offset != index && token.kind == TokenKind::Dash);
    // A single word after the dash trails the sentence as an afterthought,
    // and a sentence of its own would read as a fragment.
    let trails_sentence = index + 2 >= sentence.end;
    // A clause word after the dash joins what follows to the clause before it,
    // so "a semicolon — and a dash" continues the sentence instead of starting a new one.
    let joins_clause = clause_rank(tokens, index + 1, options).is_some();
    if inside_brackets || encloses_aside || trails_sentence || joins_clause {
        return (Some(','), None, false);
    }
    // A name before the dash is what the text after it describes,
    // as in `"a_file.mp4" — index 29`, which a colon introduces without touching a word.
    let labels_what_follows =
        previous.is_some_and(|previous| previous.is_atom() || is_quoted(previous) || looks_like_code(previous));
    if labels_what_follows || next.is_some_and(looks_like_code) {
        return (Some(':'), None, false);
    }
    if let Some(core) = next.and_then(|next| capitalize_token(next, options)) {
        return (Some('.'), Some(core), true);
    }
    if next.is_some_and(can_start_sentence_unchanged) {
        return (Some('.'), None, true);
    }
    (Some(':'), None, false)
}

/// Token range of the sentence that holds the token at `index`.
fn sentence_bounds(tokens: &[Token], index: usize, options: &FormatOptions) -> Range<usize> {
    let start = (0..index)
        .rev()
        .find(|position| is_sentence_end(tokens, *position, options))
        .map_or(0, |position| position + 1);
    let end = (index..tokens.len())
        .find(|position| is_sentence_end(tokens, *position, options))
        .map_or(tokens.len(), |position| position + 1);
    start..end
}

/// Message for the dash violation the punctuation resolves it with.
const fn dash_message(punctuation: Option<char>) -> &'static str {
    match punctuation {
        Some('.') => "dash replaced with a period and a new sentence",
        Some(':') => "dash replaced with a colon",
        Some(_) => "dash replaced with a comma",
        None => "dash removed where the text already ends a clause",
    }
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

/// Whether the token is wrapped in quotes, which makes it a name rather than a word of the sentence.
fn is_quoted(token: &Token) -> bool {
    token.leading.contains(['"', '\'', '\u{201c}', '\u{2018}'])
        && token.trailing.contains(['"', '\'', '\u{201d}', '\u{2019}'])
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
    if tokens.is_empty() {
        return Vec::new();
    }
    let budget = if *is_first_line { first_budget } else { rest_budget };
    *is_first_line = false;
    if tokens_width(&tokens) <= budget {
        return vec![tokens];
    }

    let breaks = plan_line_breaks(&tokens, budget, rest_budget, options);
    let mut pieces: Vec<Vec<Token>> = Vec::with_capacity(breaks.len() + 1);
    let mut rest = tokens;
    let mut line_start = 0;
    for position in breaks {
        let tail = rest.split_off(position - line_start);
        pieces.push(std::mem::replace(&mut rest, tail));
        line_start = position;
    }
    pieces.push(rest);

    for (index, piece) in pieces.iter().enumerate() {
        let budget = if index == 0 { budget } else { rest_budget };
        if tokens_width(piece) > budget + SOFT_OVERFLOW {
            let reason = if piece.len() > 1 {
                "no clause boundary fits within the line limit"
            } else {
                "no break point available"
            };
            report_too_long(piece, line_offset, reason, violations);
        }
    }
    pieces
}

/// Plan where a run of tokens breaks into lines, and return the token indices that start a new line.
///
/// The whole run is planned at once so that the lines come out balanced.
/// Of the break sets that use equally good boundaries the most even one wins,
/// so a long sentence becomes two medium length lines instead of one full line and a short remainder.
fn plan_line_breaks(tokens: &[Token], first_budget: usize, rest_budget: usize, options: &FormatOptions) -> Vec<usize> {
    let count = tokens.len();
    let widths = cumulative_widths(tokens);
    let mut ranks = vec![None; count + 1];
    for boundary in find_boundaries(tokens, options) {
        if (boundary.rank > Rank::Word || options.allow_word_break)
            && let Some(slot) = ranks.get_mut(boundary.before)
        {
            *slot = Some(boundary.rank);
        }
    }
    let colon_counts = cumulative_colon_counts(&ranks);
    // The width of a line, which loses the space that joined it to the previous line.
    let span_width = |from: usize, to: usize| {
        let head = widths.get(from).copied().unwrap_or_default();
        let tail = widths.get(to).copied().unwrap_or_default();
        tail.saturating_sub(head).saturating_sub(usize::from(from > 0))
    };

    // The run is walked backwards so the plan for every tail is known when a line is measured against it.
    let empty_tail = LinePlan {
        cost: 0,
        line_end: count,
    };
    let mut plans = vec![empty_tail; count + 1];
    let mut candidates = Vec::new();
    for start in (0..count).rev() {
        let budget = if start == 0 { first_budget } else { rest_budget };
        let mut best = LinePlan {
            cost: usize::MAX,
            line_end: count,
        };
        let colon_range = if span_width(start, count) > budget {
            reachable_boundary_range(&widths, start, budget)
        } else {
            0..0
        };
        collect_line_candidates(start, count, budget, &ranks, &span_width, &mut candidates);
        for end in candidates.iter().copied() {
            let tail = plans.get(end).map_or(usize::MAX, |plan| plan.cost);
            let cost = line_cost(tokens, start, end, budget, &ranks, &span_width)
                .saturating_add(skipped_colon_cost(&colon_counts, &colon_range, end))
                .saturating_add(tail);
            // Candidates come in reading order, so the last one of an equal cost fills the line the most.
            if cost <= best.cost {
                best = LinePlan { cost, line_end: end };
            }
        }
        if let Some(plan) = plans.get_mut(start) {
            *plan = best;
        }
    }

    let mut breaks = Vec::new();
    let mut position = plans.first().map_or(count, |plan| plan.line_end);
    while position < count {
        breaks.push(position);
        position = plans.get(position).map_or(count, |plan| plan.line_end);
    }
    breaks
}

/// Collect the token indices where the line that starts at `start` may end, in reading order.
fn collect_line_candidates(
    start: usize,
    count: usize,
    budget: usize,
    ranks: &[Option<Rank>],
    span_width: &dyn Fn(usize, usize) -> usize,
    candidates: &mut Vec<usize>,
) {
    candidates.clear();
    if span_width(start, count) <= budget {
        candidates.push(count);
        return;
    }

    // A sentence end is the best place to break, whatever the line is filled to.
    // A forced break marks a sentence the formatter created itself, such as a rewritten semicolon,
    // and counts the same.
    let sentence_end = (start + 1..count)
        .filter(|end| rank_at(ranks, *end).is_some_and(|rank| rank >= Rank::Sentence))
        .take_while(|end| span_width(start, *end) <= budget)
        .last();
    if let Some(end) = sentence_end {
        candidates.push(end);
        return;
    }

    // An introduction that ends in a colon reads best on its own line,
    // however little of the line it fills, as long as the clause it introduces carries on past it.
    let colon = (start + 1..count)
        .filter(|end| rank_at(ranks, *end) == Some(Rank::Colon))
        .take_while(|end| span_width(start, *end) <= budget)
        .last();
    if let Some(end) = colon
        && end - start >= MIN_COLON_INTRODUCTION_WORDS
        && continues_past(ranks, end, count)
    {
        candidates.push(end);
        return;
    }

    for end in start + 1..count {
        if rank_at(ranks, end).is_none() {
            continue;
        }
        candidates.push(end);
        if span_width(start, end) > budget + SOFT_OVERFLOW {
            // One boundary past the limit is kept for text that cannot be broken within it,
            // such as a long URL at the start of the line.
            break;
        }
    }
    candidates.push(count);
}

/// Whether the text after the boundary holds another boundary a line may end at.
///
/// A colon followed by a comma or a stronger boundary separates an introduction from a clause that continues,
/// which is what makes the colon the better place to break.
fn continues_past(ranks: &[Option<Rank>], end: usize, count: usize) -> bool {
    (end + 1..count).any(|index| rank_at(ranks, index).is_some_and(|rank| rank >= Rank::Punctuation))
}

/// Rank of the boundary before the token at `index`, or `None` when no line may end there.
fn rank_at(ranks: &[Option<Rank>], index: usize) -> Option<Rank> {
    ranks.get(index).copied().flatten()
}

/// Count colon boundaries before each token index for constant-time range queries.
fn cumulative_colon_counts(ranks: &[Option<Rank>]) -> Vec<usize> {
    let mut counts = Vec::with_capacity(ranks.len() + 1);
    let mut total = 0;
    counts.push(total);
    for rank in ranks {
        total += usize::from(*rank == Some(Rank::Colon));
        counts.push(total);
    }
    counts
}

/// Find the boundary range that fills at least half the line without exceeding its budget.
///
/// Cumulative widths are sorted, so two binary searches suffice for each line start.
fn reachable_boundary_range(widths: &[usize], start: usize, budget: usize) -> Range<usize> {
    let remaining = widths.get(start + 1..).unwrap_or_default();
    let offset = widths.get(start).copied().unwrap_or_default() + usize::from(start > 0);
    let min_fill = budget * MIN_FILL_PERCENT / 100;
    let first = remaining.partition_point(|width| width.saturating_sub(offset) < min_fill);
    let last = remaining.partition_point(|width| width.saturating_sub(offset) <= budget);
    start + 1 + first..start + 1 + last
}

/// Query the penalty for reachable colons strictly before the candidate line end.
fn skipped_colon_cost(counts: &[usize], reachable: &Range<usize>, end: usize) -> usize {
    let first = counts.get(reachable.start.min(end)).copied().unwrap_or_default();
    let last = counts.get(reachable.end.min(end)).copied().unwrap_or_default();
    last.saturating_sub(first).saturating_mul(SKIPPED_COLON_COST)
}

/// Cost of a line that covers the tokens `start..end` of the run.
fn line_cost(
    tokens: &[Token],
    start: usize,
    end: usize,
    budget: usize,
    ranks: &[Option<Rank>],
    span_width: &dyn Fn(usize, usize) -> usize,
) -> usize {
    let count = tokens.len();
    let width = span_width(start, end);
    let rank = if end < count { rank_at(ranks, end) } else { None };
    let mut cost = width_cost(budget, width);
    if let Some(rank) = rank {
        cost = cost.saturating_add(boundary_cost(tokens, end, rank));
    }
    if width > budget {
        if !overflow_is_earned(start, count, budget, rank, ranks, span_width) {
            cost = cost.saturating_add(WEAK_OVERFLOW_COST);
        }
        let excess = width.saturating_sub(budget + SOFT_OVERFLOW);
        cost = cost.saturating_add(excess.saturating_mul(TOO_LONG_COST));
    }
    cost
}

/// Cost of ending a line at the boundary before the token at `index`.
///
/// A comma or a colon before a conjunction marks the end of a clause, so the break reads as intended there.
/// The same conjunction without one may only join two parts of a phrase,
/// which is a worse place to break than the width of the lines alone suggests.
fn boundary_cost(tokens: &[Token], index: usize, rank: Rank) -> usize {
    let cost = break_cost(rank);
    if !tokens
        .get(index)
        .is_some_and(|token| contains_word(PHRASE_CONJUNCTIONS, &token.core))
    {
        return cost;
    }
    let follows_punctuation = index
        .checked_sub(1)
        .and_then(|previous| tokens.get(previous))
        .is_some_and(Token::ends_clause_punctuation);
    if follows_punctuation {
        cost
    } else {
        cost.saturating_add(BARE_CONJUNCTION_COST)
    }
}

/// Cost of the width a line leaves unused, or runs over the budget by, as a squared per mille of the budget.
///
/// Squaring the share of the budget balances the lines of a run:
/// two lines that stop short by a little cost less than one full line and a short remainder.
fn width_cost(budget: usize, width: usize) -> usize {
    let share = |amount: usize| amount * 1000 / budget.max(1);
    if width <= budget {
        let unused = share(budget - width);
        unused * unused
    } else {
        let over = share(width - budget);
        OVERFLOW_WEIGHT * over * over
    }
}

/// Cost of ending a line at a boundary, in the squared share units of `width_cost`.
///
/// A sentence end is free.
/// A weaker boundary has to earn its place:
/// it costs as much as leaving the share of the budget given in the match unused,
/// so a clearly better boundary wins over a small gain in balance.
const fn break_cost(rank: Rank) -> usize {
    match rank {
        Rank::Forced | Rank::Sentence => 0,
        Rank::Colon => 5_000,
        Rank::ClauseTier1 => 10_000,
        Rank::ClauseTier2 => 25_000,
        Rank::Punctuation => 50_000,
        Rank::ClauseTier3 => 80_000,
        Rank::ClauseTier4 => 250_000,
        Rank::Word => WORD_BREAK_COST,
    }
}

/// Whether a line that runs over the budget has earned the overflow.
///
/// The width limit is soft,
/// but the extra width has to buy a better break than the line could get by staying inside the budget,
/// so a boundary that is at least as good and fills enough of the line blocks it.
fn overflow_is_earned(
    start: usize,
    count: usize,
    budget: usize,
    rank: Option<Rank>,
    ranks: &[Option<Rank>],
    span_width: &dyn Fn(usize, usize) -> usize,
) -> bool {
    // The end of the run and a sentence end are the best breaks there are,
    // so only a coordinating conjunction or better is worth staying inside the budget for.
    let blocking = match rank {
        Some(rank) if rank < Rank::Sentence => rank,
        _ => Rank::ClauseTier2,
    };
    let min_fill = budget * MIN_FILL_PERCENT / 100;
    !(start + 1..count).any(|end| {
        let width = span_width(start, end);
        rank_at(ranks, end).is_some_and(|candidate| candidate >= blocking) && width >= min_fill && width <= budget
    })
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
mod test_command_text {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_double_hyphen_before_a_flag_is_not_a_dash() {
        let tokens = tokens("pnpm run migrate -- --env dev");
        let separator = tokens
            .iter()
            .find(|token| token.core == "--")
            .expect("the separator should be a token");
        assert_eq!(separator.kind, TokenKind::Word);

        let outcome = reflow(&["Run the migrations first: pnpm run migrate -- --env dev"], 120);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn a_double_hyphen_between_words_is_still_a_dash() {
        let outcome = reflow(&["the value is set -- always"], 120);
        assert_eq!(summary(&outcome), vec![(ViolationKind::EmDash, true)]);
    }
}

#[cfg(test)]
mod test_reflow_safety {
    use super::test_helpers::*;
    use super::*;
    use crate::semantic_line_breaks::types::RuleSet;

    #[test]
    fn a_prefix_that_leaves_no_budget_is_reported_but_not_reflowed() {
        let options = FormatOptions::with_width(20);
        let prefix = "                    // ";
        let outcome = reflow_paragraph(&paragraph(&["a line of prose that is far too long"], prefix), &options);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::LineTooLong, false)]);
    }

    #[test]
    fn a_narrow_budget_reports_nothing_when_the_length_rule_is_off() {
        let options = FormatOptions {
            rules: RuleSet {
                line_too_long: false,
                ..RuleSet::ALL
            },
            ..FormatOptions::with_width(20)
        };
        let prefix = "                    // ";
        let outcome = reflow_paragraph(&paragraph(&["a line of prose that is far too long"], prefix), &options);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn a_join_that_cannot_be_broken_again_is_not_applied() {
        let lines = [
            "and `shopifyEventHandler` in `packages/api` consumes them to link the product back to its Iron Bank",
            "item through an `ironbank_id` metafield.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120));

        assert_eq!(outcome.lines, None, "the paragraph should be left as it is");
        assert_eq!(summary(&outcome), vec![(ViolationKind::MidClauseBreak, false)]);
    }

    #[test]
    fn a_join_that_stays_within_the_limit_is_applied() {
        let lines = ["a short line that ends with the", "word that continues the clause."];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120));

        assert_eq!(
            outcome.lines,
            Some(vec![
                "a short line that ends with the word that continues the clause.".to_string()
            ])
        );
        assert_eq!(summary(&outcome), vec![(ViolationKind::MidClauseBreak, true)]);
    }

    #[test]
    fn a_paragraph_that_is_already_too_long_can_still_be_improved() {
        let long_line = format!("Alpha beta gamma delta epsilon, {}", "zeta ".repeat(40));
        let outcome = reflow(&[long_line.trim()], 60);
        let reflowed = outcome.lines.expect("the long line should be reflowed");
        assert!(reflowed.len() > 1);
        let widest = reflowed
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or_default();
        assert!(widest <= long_line.chars().count(), "the result must not be wider");
    }
}

#[cfg(test)]
mod test_brackets {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_break_inside_brackets_is_joined() {
        let lines = [
            "The query compiler only handles primitives (string, number,",
            "boolean, null, Date, Buffer). Arrays are passed through unchanged.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "The query compiler only handles primitives (string, number, boolean, null, Date, Buffer).".to_string(),
                "Arrays are passed through unchanged.".to_string(),
            ])
        );
        assert_eq!(summary(&outcome), vec![(ViolationKind::MidClauseBreak, true)]);
        assert!(
            outcome
                .violations
                .iter()
                .any(|violation| violation.message == "line breaks inside brackets")
        );
    }

    #[test]
    fn a_parenthetical_that_does_not_fit_gets_its_own_line() {
        let lines = [concat!(
            "The resolver walks up the directory tree (it reads .editorconfig, rustfmt.toml, ",
            "pyproject.toml, setup.cfg and .clang-format in that order) and caches the result."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "The resolver walks up the directory tree".to_string(),
                "(it reads .editorconfig, rustfmt.toml, pyproject.toml, setup.cfg and .clang-format in that order)"
                    .to_string(),
                "and caches the result.".to_string(),
            ]),
            "the parenthetical should stay on one line"
        );
    }

    #[test]
    fn no_break_is_made_inside_brackets() {
        let lines = ["one two three four (alpha, beta or gamma) five six seven eight nine ten"];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(40));
        let reflowed = outcome.lines.expect("the line should be reflowed");
        for line in &reflowed {
            assert_eq!(
                line.matches('(').count(),
                line.matches(')').count(),
                "a bracket was left open in {line:?} of {reflowed:?}"
            );
        }
    }

    #[test]
    fn a_sentence_end_inside_brackets_is_not_a_break_point() {
        assert_eq!(rank_before("x y (a. B c) z", 3), Some(Rank::Word));
    }

    #[test]
    fn an_unclosed_bracket_does_not_block_breaks_after_it() {
        let text = "the smiley :-) and a stray ( opening bracket that keeps going on and on and on until the end";
        let outcome = reflow(&[text], 40);
        let reflowed = outcome.lines.expect("the line should still be reflowed");
        assert!(
            reflowed.len() > 1,
            "a stray bracket must not block breaking: {reflowed:?}"
        );
    }

    #[test]
    fn brackets_inside_a_code_span_are_ignored() {
        let lines = [
            "Call `foo(bar` with the flag and then read the result",
            "from the buffer that the call returns.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "Call `foo(bar` with the flag and then read the result from the buffer that the call returns."
                    .to_string()
            ])
        );
    }

    #[test]
    fn a_hard_break_inside_brackets_is_kept() {
        let mut paragraph = paragraph(&["text with (an open bracket", "and the rest) after it"], "");
        paragraph.hard_breaks = vec![HardBreak::Spaces, HardBreak::None];
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120));
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn nested_brackets_are_tracked() {
        assert_eq!(rank_before("x y (a [b, c] d) z", 4), Some(Rank::Word));
        assert_eq!(rank_before("x y (a [b, c] d) z", 2), Some(Rank::ClauseTier4));
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
mod test_colon_penalties {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn precomputed_penalties_match_boundary_scanning() {
        for text in [
            "",
            "A sentence with no colons.",
            "Introduction: first clause: second clause: final clause.",
            "説明: a longer clause with `code: inside` and (a label: value), then more: text.",
        ] {
            let tokens = tokens(text);
            let count = tokens.len();
            let widths = cumulative_widths(&tokens);
            let mut ranks = vec![None; count + 1];
            for boundary in find_boundaries(&tokens, &FormatOptions::default()) {
                ranks[boundary.before] = Some(boundary.rank);
            }
            let colon_counts = cumulative_colon_counts(&ranks);
            let span_width = |start: usize, end: usize| {
                widths[end]
                    .saturating_sub(widths[start])
                    .saturating_sub(usize::from(start > 0))
            };
            for budget in 0..=widths.last().copied().unwrap_or_default() + 1 {
                for start in 0..count {
                    let reachable = if span_width(start, count) > budget {
                        reachable_boundary_range(&widths, start, budget)
                    } else {
                        0..0
                    };
                    for end in start + 1..=count {
                        let expected = if span_width(start, count) > budget {
                            (start + 1..end)
                                .filter(|boundary| {
                                    let width = span_width(start, *boundary);
                                    ranks[*boundary] == Some(Rank::Colon)
                                        && width >= budget * MIN_FILL_PERCENT / 100
                                        && width <= budget
                                })
                                .count()
                                * SKIPPED_COLON_COST
                        } else {
                            0
                        };
                        assert_eq!(
                            skipped_colon_cost(&colon_counts, &reachable, end),
                            expected,
                            "{text:?}, start={start}, end={end}, budget={budget}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod test_break_choice {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_word_break_costs_more_than_running_over_the_limit() {
        let options = FormatOptions {
            allow_word_break: true,
            ..FormatOptions::with_width(24)
        };
        // Allowing word breaks only adds them as candidates.
        // Running over the soft limit stays far cheaper than cutting a clause in two.
        let outcome = reflow_paragraph(&paragraph(&["one two three four five six seven eight"], ""), &options);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::LineTooLong, false)]);
    }

    #[test]
    fn an_introduction_that_ends_in_a_colon_gets_its_own_line() {
        let lines = [concat!(
            "Skip objects without a hierarchyPath: they share the same other fields as a sibling ",
            "but have no way to disambiguate themselves, so they're just ambiguous, ",
            "not \"unique by hierarchyPath\"."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines.expect("the sentence should wrap"),
            vec![
                "Skip objects without a hierarchyPath:".to_string(),
                "they share the same other fields as a sibling but have no way to disambiguate themselves,".to_string(),
                "so they're just ambiguous, not \"unique by hierarchyPath\".".to_string(),
            ]
        );
    }

    #[test]
    fn a_colon_needs_a_boundary_after_it_to_take_a_line() {
        let boundary_ranks = |text: &str| {
            let tokens = tokens(text);
            let mut ranks = vec![None; tokens.len() + 1];
            for boundary in find_boundaries(&tokens, &FormatOptions::default()) {
                ranks[boundary.before] = Some(boundary.rank);
            }
            (ranks, tokens.len())
        };
        let (ranks, count) = boundary_ranks("the first note: it counts the marker, and the indentation");
        assert_eq!(rank_at(&ranks, 3), Some(Rank::Colon));
        assert!(continues_past(&ranks, 3, count));
        let (ranks, count) = boundary_ranks("the first note: it counts the marker of the line");
        assert_eq!(rank_at(&ranks, 3), Some(Rank::Colon));
        assert!(!continues_past(&ranks, 3, count));
    }

    #[test]
    fn a_colon_introduces_explanatory_clauses_on_separate_lines() {
        let lines = [
            "Checks that a raw `TOUCH_DRAG` action value is fully valid for the form: correct shape and",
            "modes (via `isDragGestureShape`), then the pure value rules in `validateDragGestureValue`",
            "(endpoint ranges and duration). Reuses the same shared helper `DragFields` validates against",
            "instead of re-implementing the rules here, so this fixture list and the component can't",
            "silently diverge again.",
            "Uses `DragFields`'s default duration bounds (`{ min: 0.001 }` seconds) so a zero or negative",
            "duration is rejected here exactly like it would be in the actual form.",
        ];
        let options = FormatOptions::with_width(120);
        let outcome = reflow_paragraph(&paragraph(&lines, " * "), &options);
        let expected = [
            "Checks that a raw `TOUCH_DRAG` action value is fully valid for the form:",
            "correct shape and modes (via `isDragGestureShape`),",
            "then the pure value rules in `validateDragGestureValue` (endpoint ranges and duration).",
            "Reuses the same shared helper `DragFields` validates against instead of re-implementing the rules here,",
            "so this fixture list and the component can't silently diverge again.",
            "Uses `DragFields`'s default duration bounds (`{ min: 0.001 }` seconds)",
            "so a zero or negative duration is rejected here exactly like it would be in the actual form.",
        ];
        assert_eq!(outcome.lines, Some(expected.map(str::to_string).to_vec()));
        assert_eq!(reflow_paragraph(&paragraph(&expected, " * "), &options).lines, None);
    }

    #[test]
    fn a_short_colon_sentence_stays_on_one_line() {
        assert_eq!(reflow(&["Validate the form: check shape and modes."], 120).lines, None);
    }

    #[test]
    fn a_short_colon_label_does_not_force_an_underfilled_line() {
        let outcome = reflow(
            &["Note: validate the input shape and modes, then check the endpoint ranges and duration."],
            60,
        );
        let lines = outcome.lines.expect("the sentence should wrap");
        assert_ne!(lines.first().map(String::as_str), Some("Note:"));
    }

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
    fn a_sentence_is_split_into_two_balanced_lines() {
        let lines = [concat!(
            "A forced break marks a sentence the formatter created itself, ",
            "such as a rewritten semicolon, and counts the same."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "        // "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "A forced break marks a sentence the formatter created itself,".to_string(),
                "such as a rewritten semicolon, and counts the same.".to_string(),
            ]),
            "the lines should come out even instead of one full line and a short remainder"
        );
    }

    #[test]
    fn a_comma_in_the_middle_wins_over_a_fuller_first_line() {
        let lines = [concat!(
            "Skips empty names, ignored group names, ignored prefixes, ",
            "names matching the parent directory, and runtime-ignored names."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "    /// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "Skips empty names, ignored group names, ignored prefixes,".to_string(),
                "names matching the parent directory, and runtime-ignored names.".to_string(),
            ])
        );
    }

    #[test]
    fn a_conjunction_after_a_comma_wins_over_a_bare_one() {
        let lines = [concat!(
            "The current directory placeholder is used as both the first input and first output root, ",
            "and no database is attached."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "    /// "), &FormatOptions::with_width(120));
        assert_eq!(
            outcome.lines,
            Some(vec![
                "The current directory placeholder is used as both the first input and first output root,".to_string(),
                "and no database is attached.".to_string(),
            ]),
            "a break before a bare conjunction would cut the phrase in two"
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
    fn an_unterminated_code_span_is_not_an_atom() {
        assert_eq!(
            kinds("run `unterminated code here"),
            vec![TokenKind::Word, TokenKind::Word, TokenKind::Word, TokenKind::Word,]
        );
    }

    #[test]
    fn a_reference_style_link_is_one_atom() {
        assert_eq!(
            kinds("see [the docs][docs] now"),
            vec![TokenKind::Word, TokenKind::Link, TokenKind::Word]
        );
    }

    #[test]
    fn an_unbalanced_bracket_is_not_a_link() {
        assert_eq!(
            kinds("see [the docs now"),
            vec![TokenKind::Word, TokenKind::Word, TokenKind::Word, TokenKind::Word,]
        );
    }

    #[test]
    fn the_width_of_no_tokens_is_zero() {
        assert_eq!(tokens_width(&[]), 0);
        assert!(join_tokens(&[]).is_empty());
    }

    #[test]
    fn a_code_span_with_spaces_stays_one_atom() {
        let tokens = tokens("run `a; b.c() and more` now");
        assert_eq!(
            kinds("run `a; b.c() and more` now"),
            vec![TokenKind::Word, TokenKind::Code, TokenKind::Word]
        );
        assert_eq!(tokens.get(1).map(Token::text), Some("`a; b.c() and more`".to_string()));
    }

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

    #[test]
    fn a_non_ascii_abbreviation_does_not_end_a_sentence() {
        let options = FormatOptions {
            abbreviations: vec!["esim.".to_string()],
            ..FormatOptions::default()
        };
        let tokens = tokens("katso esim. tätä kohtaa");
        assert!(is_abbreviation("esim", &options));
        assert!(!is_abbreviation("tätä", &options));
        assert!(!is_sentence_end(&tokens, 1, &options));
    }

    #[test]
    fn an_index_past_the_tokens_ends_no_sentence() {
        let options = FormatOptions::default();
        let tokens = tokens("one sentence.");
        assert!(!is_sentence_end(&tokens, tokens.len(), &options));
        assert!(!is_sentence_end(&tokens, tokens.len() - 1, &options));
        assert!(!is_sentence_end(&[], 0, &options));
    }

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
    use crate::semantic_line_breaks::types::RuleSet;

    #[test]
    fn emphasis_spans_hold_together() {
        assert_eq!(rank_before("read the **bold text here** now", 3), Some(Rank::Word));
        assert_eq!(rank_before("read the _two words_ now", 3), Some(Rank::Word));
        assert_eq!(rank_before("read the **bold, text here** now", 3), Some(Rank::Word));
        // The first word of the span looks like an identifier on its own, but the markers pair up.
        assert_eq!(rank_before("use _an italic phrase_ in prose", 2), Some(Rank::Word));
        assert_eq!(rank_before("use _an italic phrase_ in prose", 3), Some(Rank::Word));
        assert_eq!(rank_before("drop ~~two struck words~~ now", 3), Some(Rank::Word));
    }

    #[test]
    fn a_single_tilde_opens_no_span() {
        assert_eq!(
            rank_before("copy ~/.config and ~/.cache, and more", 4),
            Some(Rank::ClauseTier1)
        );
    }

    #[test]
    fn a_break_is_allowed_around_an_emphasis_span() {
        assert_eq!(rank_before("a, **bold text** b", 1), Some(Rank::Punctuation));
        assert_eq!(rank_before("**bold text**, and b", 2), Some(Rank::ClauseTier1));
    }

    #[test]
    fn an_unpaired_marker_leaves_the_text_breakable() {
        assert_eq!(rank_before("a _private value, and b", 3), Some(Rank::ClauseTier1));
        assert_eq!(rank_before("the total is a * b, and more", 6), Some(Rank::ClauseTier1));
        assert_eq!(
            rank_before("call snake_case_name, and more", 2),
            Some(Rank::ClauseTier1)
        );
    }

    #[test]
    fn a_marker_inside_an_atom_opens_no_span() {
        assert_eq!(rank_before("run `a_b c_d` here, and more", 3), Some(Rank::ClauseTier1));
    }

    #[test]
    fn no_line_ends_with_a_kept_dash() {
        let options = FormatOptions {
            rules: RuleSet {
                em_dash: false,
                ..RuleSet::ALL
            },
            ..FormatOptions::default()
        };
        let tokens = tokenize_line("the value — a plain integer", 0, false);
        let boundaries = find_boundaries(&tokens, &options);
        // A lone dash never starts a line, so the only boundary beside it is the one after it.
        let beside: Vec<(usize, Rank)> = boundaries
            .iter()
            .filter(|boundary| (2..=3).contains(&boundary.before))
            .map(|boundary| (boundary.before, boundary.rank))
            .collect();
        assert_eq!(beside, vec![(3, Rank::Word)]);
    }

    #[test]
    fn ranks_punctuation_and_clause_words() {
        assert_eq!(rank_before("a, b and c", 1), Some(Rank::Punctuation));
        assert_eq!(rank_before("a b: c", 2), Some(Rank::Colon));
        assert_eq!(rank_before("a b: and c", 2), Some(Rank::Colon));
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
    fn colons_inside_atoms_and_brackets_are_not_preferred_boundaries() {
        assert_eq!(rank_before("x y (label: value) z", 3), Some(Rank::Word));
        assert_eq!(rank_before("x y `label: value` z", 3), Some(Rank::Word));
        assert_eq!(rank_before("x y https://example.com z", 3), Some(Rank::Word));
        assert_eq!(rank_before("x y module::item z", 3), Some(Rank::Word));
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
    fn a_dash_becomes_a_period_and_a_new_sentence() {
        let outcome = reflow(&["the form stays clean — save is disabled until an edit"], 120);
        assert_eq!(
            outcome.lines,
            Some(vec!["the form stays clean. Save is disabled until an edit".to_string()])
        );
        assert_eq!(summary(&outcome), vec![(ViolationKind::EmDash, true)]);
        assert_eq!(
            outcome.violations.first().map(|violation| violation.message.as_str()),
            Some("dash replaced with a period and a new sentence")
        );
    }

    #[test]
    fn every_dash_spelling_starts_a_new_sentence() {
        for text in [
            "the form stays clean — save is disabled now",
            "the form stays clean—save is disabled now",
            "the form stays clean -- save is disabled now",
            "the form stays clean – save is disabled now",
        ] {
            assert_eq!(
                reflow(&[text], 120).lines,
                Some(vec!["the form stays clean. Save is disabled now".to_string()]),
                "{text}"
            );
        }
    }

    #[test]
    fn a_rewritten_dash_breaks_the_line_when_the_sentences_do_not_fit() {
        let outcome = reflow(&["the form stays clean — save is disabled until an edit"], 30);
        assert_eq!(
            outcome.lines,
            Some(vec![
                "the form stays clean.".to_string(),
                "Save is disabled until an edit".to_string()
            ])
        );
    }

    #[test]
    fn a_dash_becomes_a_colon_before_a_word_that_stays_lowercase() {
        let options = FormatOptions {
            preserve_lowercase: vec!["npm".to_string()],
            ..FormatOptions::with_width(120)
        };
        let outcome = reflow_paragraph(&paragraph(&["the runner — npm handles the install step"], ""), &options);
        assert_eq!(
            outcome.lines,
            Some(vec!["the runner: npm handles the install step".to_string()])
        );
    }

    #[test]
    fn a_dash_becomes_a_period_before_text_that_needs_no_capital() {
        let outcome = reflow(&["the counter — 42 items were found in the archive"], 120);
        assert_eq!(
            outcome.lines,
            Some(vec!["the counter. 42 items were found in the archive".to_string()])
        );
    }

    #[test]
    fn a_semicolon_after_a_closing_bracket_is_rewritten() {
        let outcome = reflow(&["the first value (the total) is read; the second one is ignored"], 120);
        assert_eq!(
            outcome.lines,
            Some(vec![
                "the first value (the total) is read. The second one is ignored".to_string()
            ])
        );
    }

    #[test]
    fn a_dash_becomes_a_colon_around_a_name() {
        for (text, expected) in [
            (
                "the counter — snake_case_name holds the total",
                "the counter: snake_case_name holds the total",
            ),
            (
                "\"a_file.mp4\" — index 29 of the list",
                "\"a_file.mp4\": index 29 of the list",
            ),
            (
                "`primaryValue` — the form renders the target",
                "`primaryValue`: the form renders the target",
            ),
            (
                "\"PhotoLabs\" — lowercase s after the name",
                "\"PhotoLabs\": lowercase s after the name",
            ),
        ] {
            let outcome = reflow(&[text], 120);
            assert_eq!(outcome.lines, Some(vec![expected.to_string()]), "{text}");
            assert_eq!(
                outcome.violations.first().map(|violation| violation.message.as_str()),
                Some("dash replaced with a colon"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_dash_stays_a_comma_where_a_new_sentence_would_read_wrong() {
        // A pair encloses an aside, a clause word carries the sentence on,
        // a single word trails it, and brackets hold a whole aside.
        for (text, expected) in [
            (
                "the value — a plain integer — is read",
                "the value, a plain integer, is read",
            ),
            ("it uses a semicolon — and a dash", "it uses a semicolon, and a dash"),
            ("the parser is fast — really", "the parser is fast, really"),
            ("the parser (fast — really) wins", "the parser (fast, really) wins"),
            ("the value, — a plain integer", "the value, a plain integer"),
        ] {
            assert_eq!(reflow(&[text], 120).lines, Some(vec![expected.to_string()]), "{text}");
        }
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
