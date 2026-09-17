//! Paragraph reflow for the semantic line breaks formatter.
//!
//! Joins the hard-wrapped lines of a paragraph and breaks them again at semantic boundaries,
//! keeping every line within the configured width
//! and leaving a paragraph untouched when reflowing it would not read better.
//! This is the entry point the formatter calls for every prose paragraph.

use super::line_breaks::{Budgets, MIN_BUDGET, SOFT_OVERFLOW, over_long_line_violations, split_tokens};
use super::options::FormatOptions;
use super::paragraph::{HardBreak, Paragraph};
use super::rewording::{build_segments, merge_changed_sentences, reword_segments};
use super::token::Token;
use super::tokenizer::tokenize_line;
use super::violation::{Violation, ViolationKind};

/// Characters a rewrap of a hanging indent may add to the width the lines grow by.
///
/// The break points are chosen from the text each line has room for,
/// which is not the width the line ends up with when the lines that follow carry a wider prefix.
/// A plan that reads as well as the one the author wrote is not worth the lines it unbalances,
/// so a paragraph that already fit the limit keeps what it had.
/// A small step is normal, since a break belongs at a boundary rather than at a column.
const BALANCE_MARGIN: usize = 10;

/// Reason a reflow was refused, kept to explain why the violations it leaves behind are not fixable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The longest line would come out longer than the paragraph already was.
    Longer,
    /// The paragraph fit the limit and a new line would not.
    OverTheLimit,
    /// The paragraph fit the limit and its hanging indent would leave the new lines less even.
    LessBalanced,
}

/// Content of one line the reflow produced.
///
/// Keeping the description instead of the text lets a check run decide whether a paragraph changed,
/// and how wide its lines would be, without building any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputContent<'text> {
    /// Line taken unchanged from the paragraph.
    Source(usize),
    /// Line rebuilt by joining the tokens with single spaces.
    Tokens(Vec<Token<'text>>),
}

/// Result of reflowing a paragraph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReflowOutcome {
    /// New content lines with hard break markers, built only when the caller asked for a fix.
    pub lines: Option<Vec<String>>,
    /// Whether the reflow would change the paragraph.
    pub changed: bool,
    /// Violations found in the paragraph.
    pub violations: Vec<Violation>,
}

impl Refusal {
    /// What to add to the message of a violation the refusal leaves unfixable.
    const fn reason(self) -> &'static str {
        match self {
            Self::Longer => "but reflowing it would only make the longest line longer",
            Self::OverTheLimit => "but reflowing it would push a line past the limit",
            Self::LessBalanced => "but reflowing it would leave the lines less even",
        }
    }
}

impl OutputContent<'_> {
    /// Whether the content is exactly the paragraph line at `index`.
    fn matches_line(&self, paragraph: &Paragraph, index: usize) -> bool {
        let Some(line) = paragraph.lines.get(index) else {
            return false;
        };
        match self {
            Self::Source(source) => paragraph.lines.get(*source).is_some_and(|source| source == line),
            Self::Tokens(tokens) => tokens_match_line(tokens, line),
        }
    }

    /// Width of the content in characters, without its prefix.
    fn width(&self, paragraph: &Paragraph) -> usize {
        match self {
            Self::Source(source) => paragraph.lines.get(*source).map_or(0, |line| line.chars().count()),
            Self::Tokens(tokens) => tokens_width(tokens),
        }
    }

    /// The content as text.
    fn into_text(self, paragraph: &Paragraph) -> String {
        match self {
            Self::Source(source) => paragraph.lines.get(source).cloned().unwrap_or_default(),
            Self::Tokens(tokens) => join_tokens(&tokens),
        }
    }
}

/// Reflow a paragraph: join mid-clause breaks, reword semicolons and dashes, and re-break long lines.
///
/// The new lines are only built when `produce_fix` is set,
/// so a check run pays for the decisions but not for the text.
#[must_use]
pub fn reflow_paragraph(paragraph: &Paragraph, options: &FormatOptions, produce_fix: bool) -> ReflowOutcome {
    let line_offset = paragraph.start_line + 1;
    let budgets = Budgets {
        first: options
            .max_width
            .saturating_sub(prefix_width(&paragraph.first_prefix, options.tab_width)),
        rest: options
            .max_width
            .saturating_sub(prefix_width(&paragraph.rest_prefix, options.tab_width)),
    };
    let hard_limit = options.max_width + SOFT_OVERFLOW;

    if budgets.first.min(budgets.rest) <= MIN_BUDGET {
        let violations = if options.rules.line_too_long {
            over_long_line_violations(paragraph, options, hard_limit, false)
        } else {
            Vec::new()
        };
        return ReflowOutcome {
            lines: None,
            changed: false,
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

    let mut output: Vec<(OutputContent, HardBreak)> = Vec::with_capacity(paragraph.lines.len());
    let mut is_first_line = true;
    for segment in segments {
        let should_split = segment.modified || options.rules.line_too_long;
        let source_line = if segment.modified { None } else { segment.source_line };
        let mut emitted_any = false;
        let pieces = if should_split {
            split_tokens(
                segment.tokens,
                budgets,
                segment.modified,
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
            let content = match unchanged_line {
                Some(line) if line < paragraph.lines.len() => OutputContent::Source(line),
                _ => OutputContent::Tokens(piece),
            };
            output.push((content, HardBreak::None));
            emitted_any = true;
        }
        if emitted_any && let Some(last) = output.last_mut() {
            last.1 = segment.hard_break;
        }
    }

    let mut changed = output.len() != paragraph.lines.len()
        || output.iter().enumerate().any(|(index, (content, hard_break))| {
            let original_break = paragraph.hard_breaks.get(index).copied().unwrap_or_default();
            *hard_break != original_break || !content.matches_line(paragraph, index)
        });

    let reworded = holds_reworded_fix(&violations);
    if changed && let Some(refusal) = reflow_refusal(&output, paragraph, options, hard_limit, reworded) {
        changed = false;
        keep_original_lines(&mut violations, refusal);
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

    let lines = (changed && produce_fix).then(|| {
        output
            .into_iter()
            .map(|(content, hard_break)| {
                let mut line = content.into_text(paragraph);
                line.push_str(hard_break.marker());
                line
            })
            .collect()
    });
    ReflowOutcome {
        lines,
        changed,
        violations,
    }
}

/// Whether a rule rewrote the sentence rather than only rewrapping it.
///
/// A reworded sentence is restructured on purpose,
/// so it may use the soft overflow like any other text.
/// Only rewrapping a paragraph that already fit has to keep fitting.
fn holds_reworded_fix(violations: &[Violation]) -> bool {
    violations.iter().any(|violation| {
        violation.fixable && matches!(violation.kind, ViolationKind::Semicolon | ViolationKind::EmDash)
    })
}

/// Drop the reports of a refused reflow and explain the ones that are left.
///
/// Joining lines that cannot be broken again would replace readable lines with a longer one.
/// The over-long lines the split attempt reported are never written, so those reports go too,
/// and the original lines are measured by the caller like any other unchanged paragraph.
/// What is left carries the reason the fix was refused, so it can be made by hand.
fn keep_original_lines(violations: &mut Vec<Violation>, refusal: Refusal) {
    violations.retain(|violation| violation.kind != ViolationKind::LineTooLong);
    for violation in violations.iter_mut() {
        violation.fixable = false;
        violation.message = format!("{}, {}", violation.message, refusal.reason()).into();
    }
}

/// Whether joining the tokens with single spaces would produce exactly the given line.
///
/// Comparing in place answers the question without building the joined line,
/// which is all a check run needs.
fn tokens_match_line(tokens: &[Token<'_>], line: &str) -> bool {
    let mut rest = line;
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            match rest.strip_prefix(' ') {
                Some(tail) => rest = tail,
                None => return false,
            }
        }
        for part in [token.leading.as_ref(), token.core.as_ref(), token.trailing.as_ref()] {
            match rest.strip_prefix(part) {
                Some(tail) => rest = tail,
                None => return false,
            }
        }
    }
    rest.is_empty()
}

/// Why the reflowed lines are not worth writing, or `None` when they are.
///
/// A line may use the soft overflow, but reflowing must never push a line past the hard limit
/// when the paragraph did not start out that long.
/// A paragraph whose lines all fit the limit is only rewrapped for how it reads,
/// so it also has to keep fitting and to keep filling its lines from the top.
/// Rewording is exempt from the width rule, since it rewrites the sentence rather than rewrapping it.
fn reflow_refusal(
    output: &[(OutputContent, HardBreak)],
    paragraph: &Paragraph,
    options: &FormatOptions,
    hard_limit: usize,
    reworded: bool,
) -> Option<Refusal> {
    let line_width = |index: usize, width: usize, hard_break: HardBreak| {
        prefix_width(paragraph.prefix_for(index), options.tab_width) + width + hard_break.marker().len()
    };
    let new_widths: Vec<usize> = output
        .iter()
        .enumerate()
        .map(|(index, (content, hard_break))| line_width(index, content.width(paragraph), *hard_break))
        .collect();
    let old_widths: Vec<usize> = paragraph
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let hard_break = paragraph.hard_breaks.get(index).copied().unwrap_or_default();
            line_width(index, line.chars().count(), hard_break)
        })
        .collect();
    let new_max = new_widths.iter().copied().max().unwrap_or_default();
    let old_max = old_widths.iter().copied().max().unwrap_or_default();
    if new_max > hard_limit && new_max > old_max {
        return Some(Refusal::Longer);
    }
    if old_max > options.max_width {
        return None;
    }
    if new_max > options.max_width {
        return (!reworded).then_some(Refusal::OverTheLimit);
    }
    // Only a plan that keeps the lines it started with is compared for evenness.
    // Joining a paragraph or splitting a rewritten sentence changes the line count on purpose,
    // and there is nothing to compare the new lines against.
    if new_widths.len() != old_widths.len() {
        return None;
    }
    // With one prefix for every line the cost model already weighs the widths against each other.
    // A hanging indent is what it cannot see,
    // since the same amount of text reaches further right on every line after the first.
    let hanging_indent = prefix_width(&paragraph.rest_prefix, options.tab_width)
        > prefix_width(&paragraph.first_prefix, options.tab_width);
    if !hanging_indent {
        return None;
    }
    (descent(&new_widths) > descent(&old_widths) + BALANCE_MARGIN).then_some(Refusal::LessBalanced)
}

/// Characters by which the lines are wider than the line above them, added up over the paragraph.
///
/// Of two lines the first one should be the longer,
/// so only the steps that go the other way count.
/// A paragraph that fills its lines from the top scores zero however uneven the last line is.
fn descent(widths: &[usize]) -> usize {
    widths
        .iter()
        .zip(widths.iter().skip(1))
        .map(|(current, next)| next.saturating_sub(*current))
        .sum()
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
pub fn tokens_width(tokens: &[Token<'_>]) -> usize {
    if tokens.is_empty() {
        return 0;
    }
    tokens.iter().map(Token::width).sum::<usize>() + tokens.len() - 1
}

/// Join the tokens with single spaces.
#[must_use]
pub fn join_tokens(tokens: &[Token<'_>]) -> String {
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

#[cfg(test)]
mod test_reflow {
    use super::*;
    use crate::semantic_line_breaks::options::RuleSet;
    use crate::semantic_line_breaks::test_helpers::*;

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
            true,
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
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(60), true);
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
        let outcome = reflow_paragraph(&paragraph(&["A short one.", "Another short one."], ""), &options, true);
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
            true,
        );
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn keeps_hard_break_marker_on_its_line() {
        let mut paragraph = paragraph(&["roses are red", "violets are blue"], "");
        paragraph.hard_breaks = vec![HardBreak::Spaces, HardBreak::None];
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120), true);
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
mod test_reflow_safety {
    use super::*;
    use crate::semantic_line_breaks::options::RuleSet;
    use crate::semantic_line_breaks::test_helpers::*;

    /// A paragraph whose following lines line up under a column of the first one.
    fn hanging(lines: &[&str], first_prefix: &str, rest_prefix: &str) -> Paragraph {
        Paragraph {
            start_line: 0,
            end_line: lines.len(),
            first_prefix: first_prefix.to_string(),
            rest_prefix: rest_prefix.to_string(),
            last_suffix: String::new(),
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
            hard_breaks: vec![HardBreak::None; lines.len()],
        }
    }

    #[test]
    fn a_hanging_indent_keeps_its_lines_when_rewrapping_unbalances_them() {
        // The only boundary in the description is "which",
        // which reads no better than the break the author made
        // and leaves the wide continuation line far longer than the first one.
        let lines = [
            "@param archiveEntryIdentifier The id of the archive entry which will be removed",
            "from the favourites of the signed in reader.",
        ];
        let paragraph = hanging(&lines, "     * ", "     *                               ");
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120), true);
        assert_eq!(outcome.lines, None);
        assert_eq!(summary(&outcome), vec![(ViolationKind::MidClauseBreak, false)]);
        assert!(
            outcome
                .violations
                .iter()
                .all(|violation| { violation.message.contains("would leave the lines less even") }),
            "{:?}",
            outcome.violations
        );
    }

    #[test]
    fn a_paragraph_that_fits_is_not_rewrapped_past_the_limit() {
        let lines = [
            "@param archiveEntryIdentifier The id of the archive entry which will be added",
            "to the favourites of the signed in reader.",
        ];
        let paragraph = hanging(&lines, "     * ", "     *                               ");
        let outcome = reflow_paragraph(&paragraph, &FormatOptions::with_width(120), true);
        assert_eq!(outcome.lines, None);
        assert!(
            outcome.violations.iter().all(|violation| {
                !violation.fixable && violation.message.contains("would push a line past the limit")
            }),
            "{:?}",
            outcome.violations
        );
    }

    #[test]
    fn rewording_may_still_use_the_soft_overflow() {
        // A rewritten sentence is not a rewrap, so the width rule that guards a rewrap is off.
        let outcome = reflow(
            &[
                "Use `x`; then run the checks and",
                "read the output (see below)",
                "for details.",
            ],
            60,
        );
        let reflowed = outcome.lines.expect("the semicolon should be rewritten");
        assert!(reflowed.iter().any(|line| line.starts_with("Then run")), "{reflowed:?}");
    }

    #[test]
    fn a_prefix_that_leaves_no_budget_is_reported_but_not_reflowed() {
        let options = FormatOptions::with_width(20);
        let prefix = "                    // ";
        let outcome = reflow_paragraph(
            &paragraph(&["a line of prose that is far too long"], prefix),
            &options,
            true,
        );
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
        let outcome = reflow_paragraph(
            &paragraph(&["a line of prose that is far too long"], prefix),
            &options,
            true,
        );
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }

    #[test]
    fn a_join_that_cannot_be_broken_again_is_not_applied() {
        let lines = [
            "and `shopifyEventHandler` in `packages/api` consumes them to link the product back to its Iron Bank",
            "item through an `ironbank_id` metafield.",
        ];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120), true);

        assert_eq!(outcome.lines, None, "the paragraph should be left as it is");
        assert_eq!(summary(&outcome), vec![(ViolationKind::MidClauseBreak, false)]);
    }

    #[test]
    fn a_join_that_stays_within_the_limit_is_applied() {
        let lines = ["a short line that ends with the", "word that continues the clause."];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120), true);

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
mod test_command_text {
    use super::super::token::TokenKind;
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

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
mod test_unchanged_lines {
    use super::*;
    use crate::semantic_line_breaks::RuleSet;
    use crate::semantic_line_breaks::test_helpers::*;

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
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(30), true);
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
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &options, true);
        assert_eq!(outcome.lines, None);
        assert!(outcome.violations.is_empty());
    }
}
