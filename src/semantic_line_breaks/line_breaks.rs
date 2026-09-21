//! Line break planning for the semantic line breaks formatter.
//!
//! Chooses the break points of a paragraph by scoring every candidate line,
//! weighing the width a line leaves unused against the strength of the boundary it breaks at.
//! The lines of a paragraph are planned together,
//! so the break points that read best and spread the text most evenly over the lines win.
//! Also reports the lines that stay too long whatever the plan.

use std::ops::Range;

use super::boundaries::{MIN_COLON_INTRODUCTION_WORDS, PHRASE_CONJUNCTIONS, find_boundaries};
use super::list_runs::suppress_list_runs;
use super::options::FormatOptions;
use super::paragraph::Paragraph;
use super::rank::Rank;
use super::reflow::{prefix_width, tokens_width};
use super::token::Token;
use super::tokenizer::contains_word;
use super::violation::{Violation, ViolationKind};

/// Share of the maximum width a line may exceed it by before it is considered too long.
///
/// The width is a soft target.
/// Tolerating a small overflow allows breaking before a conjunction
/// when that reads better than a strict break at a comma.
const SOFT_OVERFLOW_FRACTION: f64 = 0.05;

/// Minimum fill of the budget, in percent, for a boundary to count as a reachable strong break.
const MIN_FILL_PERCENT: usize = 50;

/// Weight of the width a line runs over the budget, relative to the width it leaves unused.
///
/// Going over the soft limit reads worse than stopping short by the same amount,
/// so the overflow counts several times heavier.
const OVERFLOW_WEIGHT: usize = 4;

/// Weight of the width the line after a break exceeds the line before it by.
///
/// Of two lines the first one should be the longer,
/// so the text of a paragraph stays close to the margin it is read from.
/// The width that matters here is the one on the page,
/// since a wider prefix on the following lines makes the same amount of text reach further right.
const IMBALANCE_WEIGHT: usize = 1;

/// Share of the budget a line may fill before it is crowded against the limit.
const COMFORTABLE_FILL_FRACTION: f64 = 0.9;

/// Cost of filling a line to the limit where a clause boundary could have ended it comfortably.
///
/// A width limit is where a line has to stop, not where it should aim.
/// A line that runs up against it has no room left for the next edit,
/// so a clean break a few words earlier is worth the width it gives up.
const CROWDED_LINE_COST: usize = 1_000_000;

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

/// Cost of breaking in the middle of a clause.
///
/// This stays above every semantic break cost,
/// but below the cost of leaving even one character past the hard limit.
const WORD_BREAK_COST: usize = 1_000_000;

/// Smallest usable budget. Paragraphs with a narrower budget are left alone.
pub(super) const MIN_BUDGET: usize = 10;

/// Widths the lines of a run have for their text, once their prefixes are taken off the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Budgets {
    /// Width the first line of the paragraph has.
    pub first: usize,
    /// Width every line after the first one has.
    pub rest: usize,
}

/// Widths a line of the run is measured against, worked out once from its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineWidths {
    /// Width the line has for its text.
    budget: usize,
    /// Width the line aims at, which is short of the budget only when the lines are the formatter's own.
    target: usize,
    /// Width the prefix of the line takes from the limit.
    prefix: usize,
    /// Overflow tolerance past the budget, zero in `--strict` mode.
    overflow: usize,
}

/// The boundaries of a token run as the tables the line planner walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BreakTable<'a> {
    /// Rank of the boundary before each token, `None` where no line may end.
    ranks: &'a [Option<Rank>],
    /// Width of the run up to each token.
    widths: &'a [usize],
    /// Whether the word boundary before each token is safe to break at when nothing better fits.
    word_fallbacks: &'a [bool],
}

/// Cheapest plan for the lines of a run, counted from one token to the end of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinePlan {
    /// Total cost of the lines the plan covers.
    cost: usize,
    /// Token index where the first line of the plan ends.
    line_end: usize,
}

impl LineWidths {
    /// Work out the widths a line with the given budget is measured against.
    ///
    /// `relaid_out` marks a run whose lines the formatter is choosing itself,
    /// which is the only case where a line aims short of the budget.
    /// A line the author wrote that already fits is left at the width it has,
    /// since rewrapping it would move the text around for nothing.
    fn new(limit: usize, budget: usize, relaid_out: bool, strict: bool) -> Self {
        Self {
            budget,
            target: if relaid_out { comfortable_fill(budget) } else { budget },
            prefix: limit.saturating_sub(budget),
            overflow: soft_overflow(limit, strict),
        }
    }
}

/// Overflow tolerance for the given width, zero when `strict` asks for a hard cap at the width.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(super) fn soft_overflow(max_width: usize, strict: bool) -> usize {
    if strict {
        0
    } else {
        (max_width as f64 * SOFT_OVERFLOW_FRACTION) as usize
    }
}

/// Width a run the formatter lays out itself aims at, out of the budget its line has.
///
/// A line the formatter chose stops short of the limit,
/// so the text on it has room to grow before the next edit has to rewrap it.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(super) fn comfortable_fill(budget: usize) -> usize {
    (budget as f64 * COMFORTABLE_FILL_FRACTION) as usize
}

/// Break a run of tokens into lines that fit the budgets, preferring semantic boundaries.
///
/// `relaid_out` marks a run whose lines the formatter is choosing itself,
/// because joining or rewording replaced the ones the author wrote.
/// Such a run may stop short of the limit, while lines that already fit are left where they are.
pub(super) fn split_tokens<'text>(
    tokens: Vec<Token<'text>>,
    budgets: Budgets,
    relaid_out: bool,
    is_first_line: &mut bool,
    options: &FormatOptions,
    line_offset: usize,
    violations: &mut Vec<Violation>,
) -> Vec<Vec<Token<'text>>> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let budget = if *is_first_line { budgets.first } else { budgets.rest };
    let rest_budget = budgets.rest;
    *is_first_line = false;
    let has_forced_break = tokens.iter().any(|token| token.force_break_after);
    if !has_forced_break
        && tokens_width(&tokens) <= LineWidths::new(options.max_width, budget, relaid_out, options.strict).target
    {
        return vec![tokens];
    }

    let breaks = plan_line_breaks(&tokens, budget, rest_budget, relaid_out, options);
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
        if tokens_width(piece) > budget + soft_overflow(options.max_width, options.strict) {
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

/// Violations for original lines that exceed the width limits.
pub(super) fn over_long_line_violations(
    paragraph: &Paragraph,
    options: &FormatOptions,
    hard_limit: usize,
    changed: bool,
) -> Vec<Violation> {
    let first_prefix = prefix_width(&paragraph.first_prefix, options.tab_width);
    let rest_prefix = prefix_width(&paragraph.rest_prefix, options.tab_width);
    paragraph
        .lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let marker = paragraph.hard_breaks.get(index).copied().unwrap_or_default();
            let prefix = if index == 0 { first_prefix } else { rest_prefix };
            // A character count is never larger than a byte count,
            // so a line that is short in bytes cannot reach the limit and needs no counting.
            if prefix + line.len() + marker.marker().len() <= options.max_width {
                return None;
            }
            let width = prefix + line.chars().count() + marker.marker().len();
            let fixable = changed && width > options.max_width;
            if !fixable && width <= hard_limit {
                return None;
            }
            Some(Violation {
                line: paragraph.start_line + index + 1,
                column: None,
                kind: ViolationKind::LineTooLong,
                message: format!("line is {width} characters, limit is {}", options.max_width).into(),
                fixable,
            })
        })
        .collect()
}

/// Plan where a run of tokens breaks into lines, and return the token indices that start a new line.
///
/// The whole run is planned at once so that the lines come out balanced.
/// Of the break sets that use equally good boundaries the most even one wins,
/// so a long sentence becomes two medium length lines instead of one full line and a short remainder.
fn plan_line_breaks(
    tokens: &[Token<'_>],
    first_budget: usize,
    rest_budget: usize,
    relaid_out: bool,
    options: &FormatOptions,
) -> Vec<usize> {
    let count = tokens.len();
    let widths = cumulative_widths(tokens);
    let mut ranks = vec![None; count + 1];
    let mut word_fallbacks = vec![false; count + 1];
    for boundary in find_boundaries(tokens, options) {
        if let Some(slot) = ranks.get_mut(boundary.before) {
            *slot = Some(boundary.rank);
        }
        if let Some(slot) = word_fallbacks.get_mut(boundary.before) {
            *slot = boundary.word_fallback;
        }
    }
    suppress_list_runs(tokens, &mut ranks, &widths, first_budget.max(rest_budget));
    let colon_counts = cumulative_colon_counts(&ranks);
    let table = BreakTable {
        ranks: &ranks,
        widths: &widths,
        word_fallbacks: &word_fallbacks,
    };
    let limit = options.max_width;

    // The run is walked backwards so the plan for every tail is known when a line is measured against it.
    let empty_tail = LinePlan {
        cost: 0,
        line_end: count,
    };
    let mut plans = vec![empty_tail; count + 1];
    let mut candidates = Vec::new();
    let mut ranked_ends = Vec::new();
    // Only the first line of the paragraph has a budget of its own,
    // so the width it aims at and the prefix it carries are the same for every other line.
    let first = LineWidths::new(limit, first_budget, relaid_out, options.strict);
    let rest = LineWidths::new(limit, rest_budget, relaid_out, options.strict);
    for start in (0..count).rev() {
        let line_widths @ LineWidths { budget, prefix, .. } = if start == 0 { first } else { rest };
        let mut best = LinePlan {
            cost: usize::MAX,
            line_end: count,
        };
        let colon_range = if span_width(&widths, start, count) > budget {
            reachable_boundary_range(&widths, start, budget)
        } else {
            0..0
        };
        collect_line_candidates(
            start,
            count,
            line_widths,
            table,
            options.allow_word_break,
            &mut candidates,
            &mut ranked_ends,
        );
        for end in candidates.iter().copied() {
            let plan = plans.get(end);
            let tail = plan.map_or(usize::MAX, |plan| plan.cost);
            // The line after the candidate is already planned,
            // so how the two compare is known without looking any further ahead.
            let imbalance = match plan {
                Some(plan) if end < count => imbalance_cost(
                    limit,
                    prefix + span_width(&widths, start, end),
                    rest.prefix + span_width(&widths, end, plan.line_end),
                ),
                _ => 0,
            };
            let cost = line_cost(tokens, start, end, line_widths, &ranks, &widths)
                .saturating_add(skipped_colon_cost(&colon_counts, &colon_range, end))
                .saturating_add(imbalance)
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

/// Width of the tokens `from..to` set on one line.
///
/// The line loses the space that joined its first token to the previous one.
fn span_width(widths: &[usize], from: usize, to: usize) -> usize {
    let head = widths.get(from).copied().unwrap_or_default();
    let tail = widths.get(to).copied().unwrap_or_default();
    tail.saturating_sub(head).saturating_sub(usize::from(from > 0))
}

/// Collect the token indices where the line that starts at `start` may end, in reading order.
fn collect_line_candidates(
    start: usize,
    count: usize,
    line_widths: LineWidths,
    table: BreakTable<'_>,
    allow_word_break: bool,
    candidates: &mut Vec<usize>,
    ranked_ends: &mut Vec<(usize, Rank)>,
) {
    let BreakTable {
        ranks,
        widths,
        word_fallbacks,
    } = table;
    let LineWidths { budget, overflow, .. } = line_widths;
    candidates.clear();

    // A sentence end is the best place to break, whatever the line is filled to.
    // A forced break marks a sentence the formatter created itself, such as a rewritten semicolon,
    // and counts the same.
    let sentence_end = (start + 1..count)
        .filter(|end| rank_at(ranks, *end).is_some_and(|rank| rank >= Rank::Sentence))
        .take_while(|end| span_width(widths, start, *end) <= budget)
        .last();
    if let Some(end) = sentence_end {
        candidates.push(end);
        return;
    }

    // An introduction that ends in a colon reads best on its own line,
    // however little of the line it fills, as long as the clause it introduces carries on past it.
    let colon = (start + 1..count)
        .filter(|end| rank_at(ranks, *end) == Some(Rank::Colon))
        .take_while(|end| span_width(widths, start, *end) <= budget)
        .last();
    if let Some(end) = colon
        && end - start >= MIN_COLON_INTRODUCTION_WORDS
        && continues_past(ranks, end, count)
    {
        candidates.push(end);
        return;
    }

    // One pass gathers every usable end and whether a semantic break fits the soft limit,
    // then a second pass drops word ends that a semantic break made unnecessary.
    // Word breaks are always kept when the caller asked for them with `--word-break`.
    // Protected word ranks (inside quotes, brackets, or by a dash) stay out of this list,
    // so they never stop the scan before a later usable end past the soft limit.
    // `ranked_ends` is reused across every start of the plan so the DP does not allocate a buffer per start.
    let soft_limit = budget + overflow;
    ranked_ends.clear();
    let mut has_semantic_break_in_range = false;
    for end in start + 1..count {
        let Some(rank) = rank_at(ranks, end) else {
            continue;
        };
        let width = span_width(widths, start, end);
        if rank > Rank::Word && width <= soft_limit {
            has_semantic_break_in_range = true;
        }
        if rank == Rank::Word && !word_fallbacks.get(end).copied().unwrap_or_default() {
            continue;
        }
        ranked_ends.push((end, rank));
        if width > soft_limit {
            // One boundary past the limit is kept for text that cannot be broken within it,
            // such as a long URL at the start of the line.
            break;
        }
    }
    let keep_word_breaks = allow_word_break || !has_semantic_break_in_range;
    for &(end, rank) in ranked_ends.iter() {
        if rank == Rank::Word && !keep_word_breaks {
            continue;
        }
        candidates.push(end);
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
    tokens: &[Token<'_>],
    start: usize,
    end: usize,
    line_widths: LineWidths,
    ranks: &[Option<Rank>],
    widths: &[usize],
) -> usize {
    let LineWidths {
        budget,
        target,
        overflow,
        ..
    } = line_widths;
    let count = tokens.len();
    let width = span_width(widths, start, end);
    let rank = if end < count { rank_at(ranks, end) } else { None };
    let mut cost = width_cost(budget, width);
    if let Some(rank) = rank {
        cost = cost.saturating_add(boundary_cost(tokens, end, rank));
    }
    // Between the target and the limit the line is crowded rather than over long,
    // so it pays only when a boundary could have ended it inside the target.
    if width > target && width <= budget && !overflow_is_earned(start, count, target, rank, ranks, widths) {
        cost = cost.saturating_add(CROWDED_LINE_COST);
    }
    if width > budget {
        if !overflow_is_earned(start, count, budget, rank, ranks, widths) {
            cost = cost.saturating_add(WEAK_OVERFLOW_COST);
        }
        let excess = width.saturating_sub(budget + overflow);
        cost = cost.saturating_add(excess.saturating_mul(TOO_LONG_COST));
    }
    cost
}

/// Cost of ending a line at the boundary before the token at `index`.
///
/// A comma or a colon before a conjunction marks the end of a clause, so the break reads as intended there.
/// The same conjunction without one may only join two parts of a phrase,
/// which is a worse place to break than the width of the lines alone suggests.
fn boundary_cost(tokens: &[Token<'_>], index: usize, rank: Rank) -> usize {
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

/// Cost of a line that the line after it is wider than, as a squared per mille of the width limit.
///
/// Both widths are the ones the lines have on the page, prefix included,
/// so an aligned continuation with a wide prefix is measured by what the reader sees.
/// Squaring matches `width_cost`, so a small difference is free and a lopsided pair is not.
fn imbalance_cost(limit: usize, rendered: usize, next_rendered: usize) -> usize {
    let excess = next_rendered.saturating_sub(rendered) * 1000 / limit.max(1);
    IMBALANCE_WEIGHT * excess * excess
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
    widths: &[usize],
) -> bool {
    // The end of the run and a sentence end are the best breaks there are,
    // so only a coordinating conjunction or better is worth staying inside the budget for.
    let blocking = match rank {
        Some(rank) if rank < Rank::Sentence => rank,
        _ => Rank::ClauseTier2,
    };
    // Binary search narrows the scan to the boundaries that could possibly block the overflow,
    // since only they fill enough of the line without exceeding the budget.
    let reachable = reachable_boundary_range(widths, start, budget);
    let end = reachable.end.min(count);
    !(reachable.start..end).any(|candidate| rank_at(ranks, candidate).is_some_and(|found| found >= blocking))
}

/// Record an unfixable too long line violation at the first token of the run.
fn report_too_long(tokens: &[Token<'_>], line_offset: usize, reason: &str, violations: &mut Vec<Violation>) {
    let line = tokens
        .first()
        .map_or(line_offset, |token| line_offset + token.origin_line);
    violations.push(Violation {
        line,
        column: None,
        kind: ViolationKind::LineTooLong,
        message: format!("line exceeds the limit and {reason}").into(),
        fixable: false,
    });
}

/// Cumulative widths where entry `k` is the width of the first `k` tokens joined with spaces.
fn cumulative_widths(tokens: &[Token<'_>]) -> Vec<usize> {
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

#[cfg(test)]
mod test_break_choice {
    use super::super::reflow::{join_tokens, reflow_paragraph};
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn an_allowed_word_break_keeps_every_line_within_the_limit() {
        let options = FormatOptions {
            allow_word_break: true,
            ..FormatOptions::with_width(24)
        };
        let outcome = reflow_paragraph(
            &paragraph(&["one two three four five six seven eight"], ""),
            &options,
            true,
        );
        assert_eq!(
            outcome.lines,
            Some(vec![
                "one two three four".to_string(),
                "five six seven eight".to_string()
            ])
        );
        assert!(outcome.violations.iter().all(|violation| violation.fixable));
    }

    #[test]
    fn a_forced_break_splits_text_that_would_fit_on_one_line() {
        let mut tokens = tokens("First sentence. Second sentence.");
        tokens[1].force_break_after = true;
        let mut is_first_line = true;
        let mut violations = Vec::new();
        let pieces = split_tokens(
            tokens,
            Budgets { first: 120, rest: 120 },
            true,
            &mut is_first_line,
            &FormatOptions::with_width(120),
            1,
            &mut violations,
        );
        assert_eq!(
            pieces.iter().map(|piece| join_tokens(piece)).collect::<Vec<_>>(),
            ["First sentence.", "Second sentence."]
        );
        assert!(violations.is_empty());
    }

    #[test]
    fn an_empty_token_run_produces_no_lines() {
        let mut is_first_line = true;
        let mut violations = Vec::new();
        let pieces = split_tokens(
            Vec::new(),
            Budgets { first: 120, rest: 120 },
            true,
            &mut is_first_line,
            &FormatOptions::with_width(120),
            1,
            &mut violations,
        );
        assert!(pieces.is_empty());
        assert!(violations.is_empty());
        assert!(is_first_line);
    }

    #[test]
    fn an_introduction_that_ends_in_a_colon_gets_its_own_line() {
        let lines = [concat!(
            "Skip objects without a hierarchyPath: they share the same other fields as a sibling ",
            "but have no way to disambiguate themselves, so they're just ambiguous, ",
            "not \"unique by hierarchyPath\"."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, "// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, " * "), &options, true);
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
        assert_eq!(
            reflow_paragraph(&paragraph(&expected, " * "), &options, true).lines,
            None
        );
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
        let outcome = reflow_paragraph(&paragraph(&lines, "    /// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, "        // "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, "    /// "), &FormatOptions::with_width(120), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, "    /// "), &FormatOptions::with_width(120), true);
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
    fn a_rewrapped_run_stops_short_of_the_limit_at_a_comma() {
        // Joined, the sentence fills the line to the last column,
        // which leaves no room for the next edit when a comma could end it a few words earlier.
        let lines = [
            "The reader role keeps its own search path, so no explicit statement is needed here, and the",
            "migration stays readable.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, "-- "), &FormatOptions::with_width(120), true);
        assert_eq!(
            outcome.lines,
            Some(vec![
                "The reader role keeps its own search path, so no explicit statement is needed here,".to_string(),
                "and the migration stays readable.".to_string(),
            ])
        );
    }

    #[test]
    fn a_line_the_author_wrote_is_left_at_the_width_it_has() {
        // Only a run the formatter lays out itself aims short of the limit.
        // Rewrapping a line that already fits would move the text around for nothing.
        let lines = [concat!(
            "The reader role keeps its own search path, so no explicit statement is needed here, ",
            "and the migration stays readable."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120), true);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn the_longer_of_two_lines_comes_first() {
        // Every comma of this sentence leaves the rest of it too long for one line,
        // so the only question is which of the two lines carries the bulk of the text.
        let lines = [concat!(
            "The backend, the frontend, and the public API all describe archive entry operations ",
            "from the archive tool's perspective, not the server's:"
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120), true);
        let reflowed = outcome.lines.expect("the sentence should wrap");
        let widths: Vec<usize> = reflowed.iter().map(|line| line.chars().count()).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] >= pair[1]),
            "the lines should not grow towards the end: {reflowed:?}"
        );
    }

    #[test]
    fn an_overflowing_conjunction_still_wins_over_a_fitting_comma() {
        let text = "alpha beta gamma delta epsilon zeta eta theta, iota kappa lambda mu nu and xi omicron pi rho sigma tau upsilon phi chi psi omega";
        // 67 plus its 5% overflow reaches the same 70 character limit the fixed budget below relies on.
        let width = 67;
        assert_eq!(width + soft_overflow(width, false), 70);
        let outcome = reflow(&[text], width);
        let lines = outcome.lines.expect("should reflow");
        assert_eq!(lines[0].chars().count(), 70, "a small overshoot is allowed: {lines:?}");
    }

    #[test]
    fn strict_mode_refuses_the_overflow_a_conjunction_would_otherwise_earn() {
        let text = "alpha beta gamma delta epsilon zeta eta theta, iota kappa lambda mu nu and xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let options = FormatOptions {
            strict: true,
            ..FormatOptions::with_width(60)
        };
        let outcome = reflow_paragraph(&paragraph(&[text], ""), &options, true);
        let lines = outcome.lines.expect("should reflow");
        assert!(
            lines[0].chars().count() <= 60,
            "--strict must not let a line run past the width: {lines:?}"
        );
    }
}

#[cfg(test)]
mod test_colon_penalties {
    use super::*;

    use crate::semantic_line_breaks::test_helpers::*;

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
mod test_overflow_is_earned {
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn binary_search_matches_the_linear_scan() {
        let text = "説明: Introduction: first clause: second clause; final clause. Another sentence.";
        let tokens = tokens(text);
        let count = tokens.len();
        let widths = cumulative_widths(&tokens);
        let mut ranks = vec![None; count + 1];
        for boundary in find_boundaries(&tokens, &FormatOptions::default()) {
            ranks[boundary.before] = Some(boundary.rank);
        }
        // Brute-force original scan, checked against the binary search below.
        let brute_force = |start: usize, budget: usize, rank: Option<Rank>| {
            let blocking = match rank {
                Some(rank) if rank < Rank::Sentence => rank,
                _ => Rank::ClauseTier2,
            };
            let min_fill = budget * MIN_FILL_PERCENT / 100;
            !(start + 1..count).any(|end| {
                let width = span_width(&widths, start, end);
                rank_at(&ranks, end).is_some_and(|candidate| candidate >= blocking)
                    && width >= min_fill
                    && width <= budget
            })
        };
        let max_width = widths.last().copied().unwrap_or_default();
        for budget in 0..=max_width + 1 {
            for start in 0..count {
                for rank in [None, Some(Rank::Word), Some(Rank::Colon)] {
                    assert_eq!(
                        overflow_is_earned(start, count, budget, rank, &ranks, &widths),
                        brute_force(start, budget, rank),
                        "start={start}, budget={budget}, rank={rank:?}"
                    );
                }
            }
        }
    }
}
