//! Line selection ranges used to scope checking and fixing to specific lines.

use std::ops::RangeInclusive;
use std::str::FromStr;

/// The lines a run is limited to.
///
/// An empty set selects every line,
/// so a run without a line selection needs no special handling at the places that consult it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineRanges {
    /// One based inclusive ranges, sorted and merged so no two of them touch or overlap.
    ranges: Vec<RangeInclusive<usize>>,
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
    /// The bounds are the ones [`super::paragraph::Region`] and [`super::paragraph::Paragraph`] carry,
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

/// Parse one side of a line range, rejecting anything that is not a line number of at least one.
fn parse_line_number(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("Line numbers start at 1".to_string()),
        Ok(number) => Ok(number),
        Err(_) => Err(format!("Invalid line number '{text}'")),
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
