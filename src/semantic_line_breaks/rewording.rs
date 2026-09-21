//! Semicolon and dash rewording for the semantic line breaks formatter.
//!
//! Splits the tokens of a paragraph into segments at sentence boundaries,
//! turns a semicolon or a dash that separates two clauses into a period and a new sentence,
//! and reports the places it refuses to reword.

use std::ops::Range;

use super::boundaries::{
    clause_rank, collect_bracket_events, collect_emphasis_events, collect_quote_events, ends_sentence,
    is_mid_clause_break, is_sentence_end, lines_ending_inside, token_depths,
};
use super::options::FormatOptions;
use super::paragraph::{HardBreak, Paragraph};
use super::reflow::capitalize;
use super::token::{Token, TokenKind};
use super::tokenizer::RE_LOWERCASE_WORD;
use super::violation::{Violation, ViolationKind};

/// Punctuation, sentence-starting capitalization, and whether a new sentence starts, for a dash rewrite.
struct DashRewrite {
    /// Punctuation that replaces the dash, or `None` when the dash is dropped without a replacement.
    punctuation: Option<char>,
    /// Capitalized core of the token after the dash, when the rewrite starts a new sentence with it.
    capitalized_core: Option<String>,
    /// Whether the rewrite starts a new sentence after the dash.
    starts_new_sentence: bool,
}

/// A run of tokens that is reflowed as one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment<'text> {
    /// Tokens of the segment in order.
    pub tokens: Vec<Token<'text>>,
    /// Hard break marker at the end of the segment.
    pub hard_break: HardBreak,
    /// Whether joining or rewording changed the segment, forcing a re-split.
    pub modified: bool,
    /// Index of the single paragraph line the segment came from, when it covers exactly one line.
    pub source_line: Option<usize>,
}

/// Group paragraph lines into segments, joining lines that end mid-clause.
pub(super) fn build_segments<'text>(
    paragraph: &Paragraph,
    line_tokens: Vec<Vec<Token<'text>>>,
    options: &FormatOptions,
) -> (Vec<Segment<'text>>, Vec<Violation>) {
    let mut segments = Vec::new();
    let mut violations = Vec::new();
    let mut current: Option<Segment> = None;
    let inside_brackets = lines_ending_inside(&line_tokens, collect_bracket_events);
    let inside_emphasis = lines_ending_inside(&line_tokens, collect_emphasis_events);
    let inside_quotes = lines_ending_inside(&line_tokens, collect_quote_events);
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
        let unclosed_quote = inside_quotes.get(index - 1).copied().unwrap_or_default();
        let inside_group =
            previous_break == HardBreak::None && (unclosed_bracket || unclosed_emphasis || unclosed_quote);
        let mid_clause = options.rules.mid_clause_break
            && (inside_group || is_mid_clause_break(&segment.tokens, &tokens, previous_break, options));
        if mid_clause {
            let message = if unclosed_bracket && inside_group {
                "line breaks inside brackets"
            } else if unclosed_quote && inside_group {
                "line breaks inside quotes"
            } else if inside_group {
                "line breaks inside an emphasized phrase"
            } else {
                "line breaks in the middle of a clause"
            };
            violations.push(Violation {
                line: paragraph.start_line + index,
                column: None,
                kind: ViolationKind::MidClauseBreak,
                message: message.into(),
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
pub(super) fn reword_segments(
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

/// Join the segments around a change when the line break between them falls inside a sentence.
///
/// Reflowing a sentence means the old break in the middle of it is no longer meaningful,
/// so the whole sentence is re-broken at its best boundary instead.
/// Breaks at sentence ends and hard breaks are always kept,
/// so sentences that already have their own line stay on it.
pub(super) fn merge_changed_sentences(segments: &mut Vec<Segment>, options: &FormatOptions) {
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for next in segments.drain(..) {
        let should_merge = merged.last().is_some_and(|previous: &Segment| {
            previous.hard_break == HardBreak::None
                && !next.tokens.is_empty()
                && previous.tokens.last().is_some_and(|last| !ends_sentence(last, options))
                && (previous.modified || next.modified)
        });
        if should_merge {
            let previous = merged
                .last_mut()
                .expect("just checked should_merge against merged.last()");
            previous.tokens.extend(next.tokens);
            previous.hard_break = next.hard_break;
            previous.modified = true;
            previous.source_line = None;
        } else {
            merged.push(next);
        }
    }
    *segments = merged;
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
    let depths = bracket_and_quote_depths(&segment.tokens);
    let mut removed = vec![false; count];
    for index in dashes {
        let line = line_offset + segment.tokens.get(index).map_or(0, |token| token.origin_line);
        if index == 0 || index + 1 == count {
            violations.push(Violation {
                line,
                column: None,
                kind: ViolationKind::EmDash,
                message: "dash at the edge of a sentence cannot be rewritten automatically".into(),
                fixable: false,
            });
            continue;
        }
        let rewrite = dash_rewrite(&segment.tokens, index, &depths, options);
        if let Some(previous) = segment.tokens.get_mut(index - 1) {
            if let Some(punctuation) = rewrite.punctuation {
                previous.push_trailing(punctuation);
            }
            previous.force_break_after |= rewrite.starts_new_sentence;
        }
        if let Some(core) = rewrite.capitalized_core
            && let Some(next) = segment.tokens.get_mut(index + 1)
        {
            next.set_core(core);
        }
        if let Some(slot) = removed.get_mut(index) {
            *slot = true;
        }
        violations.push(Violation {
            line,
            column: None,
            kind: ViolationKind::EmDash,
            message: dash_message(rewrite.punctuation).into(),
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

/// Nesting depth of brackets and quotes before each token, the two spans a dash or semicolon must respect.
fn bracket_and_quote_depths(tokens: &[Token<'_>]) -> Vec<usize> {
    let brackets = token_depths(tokens, collect_bracket_events);
    let quotes = token_depths(tokens, collect_quote_events);
    (0..=tokens.len())
        .map(|index| brackets.get(index).copied().unwrap_or_default() + quotes.get(index).copied().unwrap_or_default())
        .collect()
}

/// Work out the punctuation that replaces the dash at `index` and whether it starts a new sentence.
///
/// No punctuation means the text before the dash already ends a clause,
/// so the dash is dropped and nothing takes its place.
/// A comma is used where a new sentence would read wrong:
/// inside brackets or quotes, between a pair of dashes that enclose an aside,
/// before a single trailing word, and before a clause word that carries the sentence on.
/// A colon is used where the text before the dash names what follows it,
/// and where code like text on either side rules out capitalizing a name into a new sentence.
fn dash_rewrite(tokens: &[Token<'_>], index: usize, depths: &[usize], options: &FormatOptions) -> DashRewrite {
    let previous = tokens.get(index - 1);
    let next = tokens.get(index + 1);
    if previous.is_some_and(|previous| previous.ends_clause_punctuation() || previous.ends_sentence_punctuation()) {
        return DashRewrite {
            punctuation: None,
            capitalized_core: None,
            starts_new_sentence: false,
        };
    }
    let inside_brackets = depths.get(index).copied().unwrap_or_default() > 0;
    let sentence = sentence_bounds(tokens, index, options);
    let sentence_start = sentence.start;
    let sentence_end = sentence.end;
    let encloses_aside = tokens
        .get(sentence)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .any(|(offset, token)| sentence_start + offset != index && token.kind == TokenKind::Dash);
    // A single word after the dash trails the sentence as an afterthought,
    // and a sentence of its own would read as a fragment.
    let trails_sentence = index + 2 >= sentence_end;
    // A clause word after the dash joins what follows to the clause before it,
    // so "a semicolon, and a dash" continues the sentence instead of starting a new one.
    let joins_clause = clause_rank(tokens, index + 1, options).is_some();
    if inside_brackets || encloses_aside || trails_sentence || joins_clause {
        return DashRewrite {
            punctuation: Some(','),
            capitalized_core: None,
            starts_new_sentence: false,
        };
    }
    // A name before the dash is what the text after it describes,
    // as in `"a_file.mp4" — index 29`, which a colon introduces without touching a word.
    let labels_what_follows =
        previous.is_some_and(|previous| previous.is_atom() || is_quoted(previous) || looks_like_code(previous));
    if labels_what_follows || next.is_some_and(looks_like_code) {
        return DashRewrite {
            punctuation: Some(':'),
            capitalized_core: None,
            starts_new_sentence: false,
        };
    }
    if let Some(core) = next.and_then(|next| capitalize_token(next, options)) {
        return DashRewrite {
            punctuation: Some('.'),
            capitalized_core: Some(core),
            starts_new_sentence: true,
        };
    }
    if next.is_some_and(can_start_sentence_unchanged) {
        return DashRewrite {
            punctuation: Some('.'),
            capitalized_core: None,
            starts_new_sentence: true,
        };
    }
    DashRewrite {
        punctuation: Some(':'),
        capitalized_core: None,
        starts_new_sentence: false,
    }
}

/// Token range of the sentence that holds the token at `index`.
fn sentence_bounds(tokens: &[Token<'_>], index: usize, options: &FormatOptions) -> Range<usize> {
    let start = (0..index)
        .rev()
        .find(|position| is_sentence_end(tokens, *position, options))
        .map_or(0, |position| position + 1);
    let end = (index..tokens.len())
        .find(|position| is_sentence_end(tokens, *position, options))
        .map_or(tokens.len(), |position| position + 1);
    start..end
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
        // The depths are the same for every semicolon of the segment,
        // so scanning the brackets and quotes once keeps the pass linear in the number of tokens.
        let depths = segments
            .get(segment_index)
            .map(|segment| bracket_and_quote_depths(&segment.tokens))
            .unwrap_or_default();
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
            let refusal = semicolon_refusal(&segment.tokens, token_index, next, &depths, options);
            if let Some(reason) = refusal {
                violations.push(Violation {
                    line,
                    column: None,
                    kind: ViolationKind::Semicolon,
                    message: format!("semicolon joins clauses, {reason}").into(),
                    fixable: false,
                });
                continue;
            }
            let capitalized = next.and_then(|next| capitalize_token(next, options));
            if let Some(segment) = segments.get_mut(segment_index)
                && let Some(token) = segment.tokens.get_mut(token_index)
            {
                token.replace_trailing_end('.');
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
                next_token.set_core(new_core);
                next_segment.modified = true;
            }
            violations.push(Violation {
                line,
                column: None,
                kind: ViolationKind::Semicolon,
                message: "semicolon replaced with a period and a new sentence".into(),
                fixable: true,
            });
        }
    }
}

/// Reason the semicolon after `tokens[index]` cannot be rewritten, or `None` when it can.
fn semicolon_refusal(
    tokens: &[Token<'_>],
    index: usize,
    next: Option<&Token>,
    depths: &[usize],
    options: &FormatOptions,
) -> Option<&'static str> {
    let token = tokens.get(index)?;
    let Some(next) = next else {
        return Some("nothing follows it");
    };
    // The table holds the depth before each token,
    // so the entry after the semicolon is the one that counts the brackets and quotes of the semicolon token.
    // Bracket and quote free text yields an empty table, where every depth is zero.
    if depths.get(index + 1).copied().unwrap_or_default() > 0 {
        return Some("it is inside brackets or quotes");
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
fn is_quoted(token: &Token<'_>) -> bool {
    token.leading.contains(['"', '\'', '\u{201c}', '\u{2018}'])
        && token.trailing.contains(['"', '\'', '\u{201d}', '\u{2019}'])
}

/// Whether a token is code-like enough that rewording it would be wrong.
fn looks_like_code(token: &Token<'_>) -> bool {
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
fn can_start_sentence_unchanged(token: &Token<'_>) -> bool {
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
fn capitalize_token(token: &Token<'_>, options: &FormatOptions) -> Option<String> {
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

/// Message for the dash violation the punctuation resolves it with.
const fn dash_message(punctuation: Option<char>) -> &'static str {
    match punctuation {
        Some('.') => "dash replaced with a period and a new sentence",
        Some(':') => "dash replaced with a colon",
        Some(_) => "dash replaced with a comma",
        None => "dash removed where the text already ends a clause",
    }
}

#[cfg(test)]
mod test_rewording {
    use super::super::reflow::reflow_paragraph;
    use super::*;
    use crate::semantic_line_breaks::options::RuleSet;
    use crate::semantic_line_breaks::test_helpers::*;

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
    fn semicolon_inside_quotes_is_refused() {
        // "&amp;" holds a literal semicolon, and rewording it would corrupt the entity into
        // "&amp." followed by a capitalized word that was never a new sentence.
        let outcome = reflow(&["the key is \"Tietokoneet &amp; tabletit\" in the source data"], 120);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::Semicolon, false)]);
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
            outcome.violations.first().map(|violation| violation.message.as_ref()),
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
        let outcome = reflow_paragraph(
            &paragraph(&["the runner — npm handles the install step"], ""),
            &options,
            true,
        );
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
    fn a_stray_bracket_does_not_decide_the_semicolons_around_it() {
        // Only the brackets that pair up count,
        // so an unclosed opener does not make the rest of the line unrewritable
        // and an unmatched closer does not cancel a real group.
        let outcome = reflow(&["see the note (item two; the rest is ignored"], 120);
        assert_eq!(
            outcome.lines,
            Some(vec!["see the note (item two. The rest is ignored".to_string()])
        );
        let outcome = reflow(&["see b) (item two; the rest is ignored)"], 120);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::Semicolon, false)]);
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
                outcome.violations.first().map(|violation| violation.message.as_ref()),
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
        let outcome = reflow_paragraph(&paragraph(&["a; b — c"], ""), &options, true);
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

#[cfg(test)]
mod test_sentence_merge {
    use super::super::reflow::reflow_paragraph;
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn a_joined_sentence_is_re_broken_at_its_best_boundary() {
        let lines = [
            "Parse reads the header from the input and returns the parsed struct, and it reports an error",
            "when the input is",
            "truncated.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120), true);
        assert_eq!(
            outcome.lines, None,
            "a line ending a sentence must not absorb the next line"
        );
    }

    #[test]
    fn a_hard_break_stops_the_merge() {
        let mut paragraph = paragraph(&["text that continues", "onto the next line"], "");
        paragraph.hard_breaks = vec![HardBreak::Spaces, HardBreak::None];
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120), true);
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
