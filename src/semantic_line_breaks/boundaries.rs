//! Sentence and clause boundary detection for the semantic line breaks formatter.
//!
//! Ranks the places a line may break, from the end of a sentence down to a weak clause boundary,
//! tracks bracket and emphasis nesting so a break never lands inside a delimited span,
//! and decides whether two consecutive lines were wrapped in the middle of a clause.

use std::sync::LazyLock;

use super::options::FormatOptions;
use super::paragraph::HardBreak;
use super::rank::Rank;
use super::token::{Token, TokenKind, is_opener};
use super::tokenizer::{contains_word, is_abbreviation, is_trailing_closer, starts_markdown_structure};

/// Minimum number of words before a colon for the text to introduce what follows it.
///
/// A label of one or two words, such as "Note:" or "Legacy compatibility:",
/// reads as part of the first clause rather than as an introduction of its own,
/// so it stays on the line with the text it labels.
pub(super) const MIN_COLON_INTRODUCTION_WORDS: usize = 3;

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
    "without",
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

/// Relative pronouns and weak connectors, the least preferred clause words.
const CLAUSE_TIER_4: &[&str] = &["which", "that", "as"];

/// Two word connectors, treated like tier 3, held as the pair of words they are compared against.
const CLAUSE_PAIRS: &[(&str, &str)] = &[
    ("for", "example"),
    ("such", "as"),
    ("as", "well as"),
    ("in", "order to"),
];

/// Conjunctions that join two phrases as often as two clauses,
/// such as "the first input and the first output root".
pub(super) const PHRASE_CONJUNCTIONS: &[&str] = &["and", "or", "but", "nor"];

/// Words that clearly leave a clause unfinished when they end a line.
const DANGLING_WORDS: &[&str] = &[
    "the", "a", "an", "of", "to", "in", "on", "for", "with", "by", "from", "at", "into", "and", "or", "but", "is",
    "are", "be", "was", "were", "that", "which", "this", "these", "those", "its", "their", "as", "not", "no", "any",
    "all", "each", "every", "same", "than", "very", "more", "most",
];

/// Longest clause word that the table can hold, which bounds the lowercasing buffer.
const MAX_CLAUSE_WORD: usize = 16;

/// Clause words with their break quality, sorted by word.
///
/// The tier lists above stay the source of truth.
/// Sorting them into one table lets a lookup compare a word against a handful of candidates
/// instead of scanning every tier in turn, which is the hottest comparison in the formatter.
static CLAUSE_WORDS: LazyLock<Vec<(&'static str, Rank)>> = LazyLock::new(|| {
    let tiers = [
        (CLAUSE_TIER_1, Rank::ClauseTier1),
        (CLAUSE_TIER_2, Rank::ClauseTier2),
        (CLAUSE_TIER_3, Rank::ClauseTier3),
        (CLAUSE_TIER_4, Rank::ClauseTier4),
    ];
    let mut words: Vec<(&'static str, Rank)> = tiers
        .into_iter()
        .flat_map(|(list, rank)| list.iter().map(move |word| (*word, rank)))
        .collect();
    words.sort_unstable_by_key(|(word, _)| *word);
    words
});

/// A candidate break position between two tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// Index of the token that starts the new line.
    pub before: usize,
    /// Quality of the break.
    pub rank: Rank,
}

/// One bracket or emphasis span boundary, at a token position, opening or closing the span.
#[derive(Debug, Clone, Copy)]
pub(super) struct SpanEvent {
    /// Index of the token the boundary sits at.
    position: usize,
    /// Whether the boundary opens the span, as opposed to closing it.
    opens: bool,
}

/// Collects the group opening and closing events of one token at the given position.
type EventCollector = fn(&Token, usize, &mut Vec<SpanEvent>);

/// Whether the token at `index` ends a sentence and the following token starts a new one.
#[must_use]
pub fn is_sentence_end(tokens: &[Token<'_>], index: usize, options: &FormatOptions) -> bool {
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
pub fn is_sentence_boundary(token: &Token<'_>, next: &Token, options: &FormatOptions) -> bool {
    ends_sentence(token, options) && starts_sentence(next)
}

/// Whether the token ends a sentence, ignoring what follows it.
#[must_use]
pub fn ends_sentence(token: &Token<'_>, options: &FormatOptions) -> bool {
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
fn starts_sentence(token: &Token<'_>) -> bool {
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
pub fn clause_rank(tokens: &[Token<'_>], index: usize, options: &FormatOptions) -> Option<Rank> {
    let token = tokens.get(index)?;
    if token.kind != TokenKind::Word {
        return None;
    }
    let word = token.core.as_ref();
    if let Some(next) = tokens.get(index + 1)
        && CLAUSE_PAIRS
            .iter()
            .any(|(first, second)| first.eq_ignore_ascii_case(word) && second.eq_ignore_ascii_case(&next.core))
    {
        return Some(Rank::ClauseTier3);
    }
    if let Some(rank) = clause_word_rank(word) {
        return Some(rank);
    }
    options
        .clause_starters
        .iter()
        .any(|starter| starter.eq_ignore_ascii_case(word))
        .then_some(Rank::ClauseTier4)
}

/// Rank of a clause word in the table, ignoring ASCII case.
///
/// A word longer than every clause word cannot be one, so it is rejected on its length alone.
fn clause_word_rank(word: &str) -> Option<Rank> {
    let bytes = word.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_CLAUSE_WORD {
        return None;
    }
    let mut lowercase = [0u8; MAX_CLAUSE_WORD];
    for (slot, byte) in lowercase.iter_mut().zip(bytes) {
        *slot = byte.to_ascii_lowercase();
    }
    let key = lowercase.get(..bytes.len())?;
    let index = CLAUSE_WORDS
        .binary_search_by(|(candidate, _)| candidate.as_bytes().cmp(key))
        .ok()?;
    CLAUSE_WORDS.get(index).map(|(_, rank)| *rank)
}

/// Find all break candidates between the tokens, ranked by quality.
#[must_use]
pub fn find_boundaries(tokens: &[Token<'_>], options: &FormatOptions) -> Vec<Boundary> {
    let mut boundaries = Vec::new();
    let depths = token_depths(tokens, collect_bracket_events);
    let emphasis = token_depths(tokens, collect_emphasis_events);
    // The rank of the token before the boundary is the rank the previous boundary looked up,
    // so carrying it over halves the number of clause lookups.
    let mut previous_clause = clause_rank(tokens, 0, options);
    for index in 1..tokens.len() {
        let current_clause = clause_rank(tokens, index, options);
        let (Some(previous), Some(current)) = (tokens.get(index - 1), tokens.get(index)) else {
            continue;
        };
        let depth = depths.get(index).copied().unwrap_or_default() + emphasis.get(index).copied().unwrap_or_default();
        if starts_markdown_structure(current) {
            previous_clause = current_clause;
            continue;
        }
        let clause = if index >= 2 && previous_clause.is_none() {
            current_clause
        } else {
            None
        };
        previous_clause = current_clause;
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
            let closes_group = previous.trailing.contains([')', ']', '}']);
            let opens_group = current.leading.contains(['(', '[', '{']);
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

/// Group nesting depth before each token, counting only the groups that are closed later.
///
/// A group that is never closed is ignored,
/// so a stray parenthesis or emphasis marker in prose does not make the rest of the text unbreakable.
///
/// Returns an empty vector when the tokens hold no groups, since every depth is then zero.
pub(super) fn token_depths(tokens: &[Token<'_>], collect: EventCollector) -> Vec<usize> {
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
pub(super) fn lines_ending_inside(line_tokens: &[Vec<Token<'_>>], collect: EventCollector) -> Vec<bool> {
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
fn matched_depths(events: &mut Vec<SpanEvent>, count: usize) -> Vec<usize> {
    retain_matched_events(events);

    let mut depths = Vec::with_capacity(count + 1);
    let mut depth = 0usize;
    let mut next = 0;
    for index in 0..=count {
        depths.push(depth);
        while let Some(&event) = events.get(next) {
            if event.position != index {
                break;
            }
            depth = if event.opens {
                depth + 1
            } else {
                depth.saturating_sub(1)
            };
            next += 1;
        }
    }
    depths
}

/// Append the bracket characters of the token as span events.
///
/// Brackets are ASCII, so the parts are scanned as bytes to avoid decoding every character.
/// The inner text of an atom is skipped, so a bracket in a code span or a link does not count.
pub(super) fn collect_bracket_events(token: &Token<'_>, position: usize, events: &mut Vec<SpanEvent>) {
    let core = if token.is_atom() { "" } else { token.core.as_ref() };
    for part in [token.leading.as_ref(), core, token.trailing.as_ref()] {
        for byte in part.bytes() {
            match byte {
                b'(' | b'[' | b'{' => events.push(SpanEvent { position, opens: true }),
                b')' | b']' | b'}' => events.push(SpanEvent { position, opens: false }),
                _ => {}
            }
        }
    }
}

/// Drop the events of groups that are never opened or never closed.
fn retain_matched_events(events: &mut Vec<SpanEvent>) {
    let mut matched = vec![false; events.len()];
    let mut open_events: Vec<usize> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if event.opens {
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
    let mut matched = matched.into_iter();
    events.retain(|_| matched.next().unwrap_or(false));
}

/// Append the emphasis span events of the token as span events.
///
/// A token that carries both of its own markers, such as `*strong*`, delimits no span past itself.
/// Delimited text is skipped, so a marker inside a code span, a link, or a URL does not count.
/// Other kinds are read as prose, since `_italic` looks the same as an identifier on its own.
pub(super) fn collect_emphasis_events(token: &Token<'_>, position: usize, events: &mut Vec<SpanEvent>) {
    if is_delimited(token.kind) || !holds_emphasis_marker(token) {
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
        events.push(SpanEvent { position, opens: true });
    }
    if closing > 0 {
        events.push(SpanEvent { position, opens: false });
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

/// Whether any part of the token holds a marker character at all.
///
/// A run has to start from a marker, so a token without one delimits nothing,
/// and most words in prose hold none.
fn holds_emphasis_marker(token: &Token<'_>) -> bool {
    [
        token.leading.as_bytes(),
        token.core.as_bytes(),
        token.trailing.as_bytes(),
    ]
    .iter()
    .any(|part| part.iter().any(|byte| matches!(byte, b'*' | b'_' | b'~')))
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

/// Whether the break between two consecutive lines falls in the middle of a clause.
#[must_use]
pub fn is_mid_clause_break(
    line_a: &[Token<'_>],
    line_b: &[Token<'_>],
    hard_break: HardBreak,
    options: &FormatOptions,
) -> bool {
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

#[cfg(test)]
mod test_sentence_end {
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

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
    use super::super::tokenizer::tokenize_line;
    use super::*;
    use crate::semantic_line_breaks::options::RuleSet;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn the_clause_table_holds_every_tier_word() {
        let tiers = [
            (CLAUSE_TIER_1, Rank::ClauseTier1),
            (CLAUSE_TIER_2, Rank::ClauseTier2),
            (CLAUSE_TIER_3, Rank::ClauseTier3),
            (CLAUSE_TIER_4, Rank::ClauseTier4),
        ];
        let mut count = 0;
        for (list, rank) in tiers {
            for word in list {
                assert_eq!(clause_word_rank(word), Some(rank), "{word}");
                assert_eq!(clause_word_rank(&word.to_uppercase()), Some(rank), "{word}");
                count += 1;
            }
        }
        assert_eq!(CLAUSE_WORDS.len(), count);
        assert!(
            CLAUSE_WORDS.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "the table has to stay sorted for the binary search"
        );
        assert_eq!(clause_word_rank(""), None);
        assert_eq!(clause_word_rank("header"), None);
        assert_eq!(
            clause_word_rank("a word that is far too long to be a clause word"),
            None
        );
    }

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
mod test_brackets {
    use super::super::reflow::reflow_paragraph;
    use super::super::violation::ViolationKind;
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn a_break_inside_brackets_is_joined() {
        let lines = [
            "The query compiler only handles primitives (string, number,",
            "boolean, null, Date, Buffer). Arrays are passed through unchanged.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(40), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120), true);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn nested_brackets_are_tracked() {
        assert_eq!(rank_before("x y (a [b, c] d) z", 4), Some(Rank::Word));
        assert_eq!(rank_before("x y (a [b, c] d) z", 2), Some(Rank::ClauseTier4));
    }

    #[test]
    fn a_colon_inside_curly_brackets_is_not_a_break_point() {
        assert_eq!(rank_before("x y {@code mobile: deleteFile} z", 3), Some(Rank::Word));
    }

    #[test]
    fn no_break_is_made_inside_curly_brackets() {
        let lines = ["one two three four {alpha, beta or gamma} five six seven eight nine ten"];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(40), true);
        let reflowed = outcome.lines.expect("the line should be reflowed");
        for line in &reflowed {
            assert_eq!(
                line.matches('{').count(),
                line.matches('}').count(),
                "a curly bracket was left open in {line:?} of {reflowed:?}"
            );
        }
    }
}

#[cfg(test)]
mod test_mid_clause_break {
    use super::*;

    use crate::semantic_line_breaks::test_helpers::*;

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
