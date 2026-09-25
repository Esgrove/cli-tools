//! Suppressing break candidates inside single word list runs.
//!
//! A list such as "alpha, beta, gamma, and delta" reads as one unit,
//! so a line may end before it or after it but not between two of its items.
//! The run has to fit the budget, since a list too long for one line has to break somewhere.

use super::rank::Rank;
use super::token::Token;
use super::tokenizer::contains_word;

/// Conjunctions that introduce the last item of a list.
///
/// Only the ones that join items are here.
/// A "but" joins two clauses, so it never continues a list of names.
const LIST_CONJUNCTIONS: &[&str] = &["and", "or", "nor"];

/// Smallest number of items a run needs before it reads as a list rather than as a pair.
const MIN_LIST_ITEMS: usize = 3;

/// Clear the break candidates inside every list of single word items that fits on one line.
pub(super) fn suppress_list_runs(tokens: &[Token<'_>], ranks: &mut [Option<Rank>], widths: &[usize], budget: usize) {
    let count = tokens.len();
    let mut start = 0;
    while start < count {
        let Some(end) = list_run_end(tokens, start) else {
            start += 1;
            continue;
        };
        if span_width(widths, start, end) <= budget {
            for slot in ranks.get_mut(start + 1..end).unwrap_or_default() {
                *slot = None;
            }
        }
        start = end;
    }
}

/// One past the last token of the list that starts at `start`, or `None` when no list starts there.
///
/// Every item but the last one carries its own comma,
/// which is what tells a list of names from a sentence that happens to hold commas:
/// an item of several words leaves all but its last token without one.
fn list_run_end(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    let mut end = start;
    while tokens.get(end).is_some_and(|token| token.trailing.ends_with(',')) {
        end += 1;
    }
    if end - start + 1 < MIN_LIST_ITEMS {
        return None;
    }
    if tokens
        .get(end)
        .is_some_and(|token| contains_word(LIST_CONJUNCTIONS, &token.core))
    {
        end += 1;
    }
    // The last item closes the list, so a run that reaches the end of the text is not one.
    (end < tokens.len()).then_some(end + 1)
}

/// Width of the tokens `from..to` set on one line.
///
/// The line loses the space that joined its first token to the previous one.
fn span_width(widths: &[usize], from: usize, to: usize) -> usize {
    let head = widths.get(from).copied().unwrap_or_default();
    let tail = widths.get(to).copied().unwrap_or_default();
    tail.saturating_sub(head).saturating_sub(usize::from(from > 0))
}

#[cfg(test)]
mod test_list_runs {
    use super::super::line_breaks::soft_overflow;
    use super::super::options::FormatOptions;
    use super::super::reflow::reflow_paragraph;
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    /// End of the list starting at every token index of the text.
    fn run_ends(text: &str) -> Vec<Option<usize>> {
        let tokens = tokens(text);
        (0..tokens.len()).map(|start| list_run_end(&tokens, start)).collect()
    }

    #[test]
    fn three_single_word_items_are_a_list() {
        assert_eq!(run_ends("keeps alpha, beta, gamma together").first(), Some(&None));
        assert_eq!(run_ends("keeps alpha, beta, gamma together").get(1), Some(&Some(4)));
    }

    #[test]
    fn a_conjunction_before_the_last_item_belongs_to_the_list() {
        let ends = run_ends("targets alpha, beta, gamma, and delta because of it");
        assert_eq!(ends.get(1), Some(&Some(6)));
    }

    #[test]
    fn two_items_are_a_pair_rather_than_a_list() {
        assert_eq!(run_ends("keeps alpha, beta together").get(1), Some(&None));
    }

    #[test]
    fn items_of_several_words_are_not_a_list() {
        // Only the last token of a multi word item carries the comma,
        // so no two items ever sit next to each other.
        let ends = run_ends("skips empty names, ignored group names, runtime ignored names, and the rest");
        assert!(ends.iter().all(Option::is_none), "{ends:?}");
    }

    #[test]
    fn a_list_that_reaches_the_end_of_the_text_is_not_one() {
        assert!(run_ends("alpha, beta, gamma,").iter().all(Option::is_none));
    }

    #[test]
    fn a_list_that_fits_one_line_is_not_split() {
        let lines = [concat!(
            "Without an explicit account list, `--upload` targets `alpha`, `beta`, `delta`, `gamma`, ",
            "`kappa`, `lambda`, and `theta`, because the tool runs one account per environment."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120), true);
        let reflowed = outcome.lines.expect("the sentence should wrap");
        assert_eq!(
            reflowed.first().map(String::as_str),
            Some(concat!(
                "Without an explicit account list, `--upload` targets `alpha`, `beta`, `delta`, `gamma`, ",
                "`kappa`, `lambda`, and `theta`,"
            ))
        );
    }

    #[test]
    fn a_list_too_long_for_one_line_still_breaks() {
        let lines = [concat!(
            "The tool targets `alpha`, `beta`, `delta`, `gamma`, `kappa`, `lambda`, `sigma`, `omega`, ",
            "`sigmadelta`, `kappagamma`, `omegabeta`, and `deltalambda`, because one account is used per environment."
        )];
        let outcome = reflow_paragraph(&paragraph(&lines, ""), &FormatOptions::with_width(120), true);
        let reflowed = outcome.lines.expect("the sentence should wrap");
        assert!(
            reflowed
                .iter()
                .all(|line| line.chars().count() <= 120 + soft_overflow(120, false)),
            "a list longer than a line has to break somewhere: {reflowed:?}"
        );
        assert!(
            reflowed.first().is_some_and(|line| line.ends_with("`sigmadelta`,")),
            "the break should fall between two items of the list: {reflowed:?}"
        );
    }
}

#[cfg(test)]
mod test_fixture_coverage {
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn every_list_conjunction_closes_a_list_in_a_fixture() {
        let closings: Vec<String> = LIST_CONJUNCTIONS
            .iter()
            .map(|conjunction| format!(", {conjunction} "))
            .collect();
        let closings: Vec<&str> = closings.iter().map(String::as_str).collect();
        assert_fixtures_contain(&closings, "LIST_CONJUNCTIONS");
    }
}
