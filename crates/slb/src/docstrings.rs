//! Python docstring parsing and the quote placement rule.
//!
//! Finds where a triple quoted docstring opens and closes,
//! which is what the region split needs to hand the docstring body to the Markdown splitter.
//! Also produces the replacement lines that move the quotes of a multi line docstring onto lines of their own,
//! so the prose inside the docstring keeps one indentation and one prefix.

use std::sync::LazyLock;

use regex::Regex;

use super::comments::LineScan;
use super::file_kind::FileKind;
use super::markdown;
use super::options::FormatOptions;
use super::paragraph::{Paragraph, Region};
use super::reflow::{prefix_width, reflow_paragraph};
use super::violation::{Violation, ViolationKind};

/// Matches the opening of a Python docstring and captures indentation, string prefix, quotes, and the rest.
///
/// Only plain, raw (`r`/`R`), and legacy unicode (`u`/`U`) prefixes can introduce a Python docstring.
/// Format strings (`f`/`F`) and byte literals (`b`/`B`) are runtime values, never docstrings.
static RE_DOCSTRING_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^(\s*)([rRuU]?)("""|''')(.*)$"#).expect("Invalid docstring regex"));

/// Characters that leave an expression open at the end of a line,
/// so a string on the next line is part of that expression.
const EXPRESSION_CONTINUATIONS: &[char] = &[
    '(', '[', '{', ',', '\\', '=', '+', '-', '*', '/', '%', '&', '|', '^', '<', '>', '~', '@',
];

/// A triple quoted docstring spanning more than one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Docstring<'a> {
    /// Zero based index of the line the opening quotes are on.
    pub(super) open_line: usize,
    /// Zero based index of the line the closing quotes are on.
    pub(super) close_line: usize,
    /// Indentation of the opening line.
    pub(super) indent: &'a str,
    /// String prefix in front of the opening quotes, for example "r".
    pub(super) string_prefix: &'a str,
    /// The quote characters the docstring uses.
    pub(super) quote: &'a str,
    /// Text after the opening quotes on the opening line.
    pub(super) opening_text: &'a str,
    /// Text before the closing quotes on the closing line.
    pub(super) closing_text: &'a str,
}

impl<'a> Docstring<'a> {
    /// Parse the docstring opening on line `index`, when there is one that spans several lines.
    ///
    /// A docstring opening and closing on one line is not a prose block,
    /// and neither is one with code after the closing quotes,
    /// so both of them give `None` and are left verbatim.
    /// A string that continues an expression from the line above is a value rather than a docstring,
    /// so it gives `None` too.
    #[cfg(test)]
    pub(super) fn at(lines: &[&'a str], index: usize) -> Option<Self> {
        Self::at_scanned(lines, index, &super::comments::scan_lines(lines, FileKind::Python))
    }

    /// Parse the docstring opening on line `index` using the lexical context recorded by `scan`.
    pub(super) fn at_scanned(lines: &[&'a str], index: usize, scan: &LineScan) -> Option<Self> {
        let line = lines.get(index)?;
        let captures = RE_DOCSTRING_OPEN.captures(line)?;
        if scan.starts_inside_expression(index) || !starts_statement(lines, index, scan) {
            return None;
        }
        let indent = captures.get(1).map_or("", |group| group.as_str());
        let string_prefix = captures.get(2).map_or("", |group| group.as_str());
        let quote = captures.get(3).map_or(r#"""""#, |group| group.as_str());
        let opening_text = captures.get(4).map_or("", |group| group.as_str());
        if find_unescaped_triple_quote(opening_text, quote).is_some() {
            return None;
        }
        let (close_line, close_offset) =
            lines
                .iter()
                .enumerate()
                .skip(index + 1)
                .find_map(|(position, candidate)| {
                    find_unescaped_triple_quote(candidate, quote).map(|offset| (position, offset))
                })?;
        let close_source = lines.get(close_line)?;
        let closing_text = close_source.get(..close_offset)?;
        let after_quote = close_source.get(close_offset + quote.len()..)?;
        if !after_quote.trim().is_empty() {
            return None;
        }
        Some(Self {
            open_line: index,
            close_line,
            indent,
            string_prefix,
            quote,
            opening_text,
            closing_text,
        })
    }

    /// Whether prose shares the opening line with the quotes.
    pub(super) fn opening_shares_line(&self) -> bool {
        !self.opening_text.trim().is_empty()
    }

    /// Whether prose shares the closing line with the quotes.
    pub(super) fn closing_shares_line(&self) -> bool {
        !self.closing_text.trim().is_empty()
    }
}

/// Find the first unescaped occurrence of `quote` (three identical quote chars) in `text`.
///
/// An odd run of immediately preceding backslashes escapes the first quote, even in raw Python strings.
fn find_unescaped_triple_quote(text: &str, quote: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(relative) = text.get(search_from..)?.find(quote) {
        let offset = search_from + relative;
        if !is_escaped_at(text, offset) {
            return Some(offset);
        }
        search_from = offset + 1;
    }
    None
}

/// Whether the byte at `offset` is preceded by an odd number of backslashes.
fn is_escaped_at(text: &str, offset: usize) -> bool {
    let backslashes = text
        .get(..offset)
        .unwrap_or_default()
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count();
    backslashes % 2 == 1
}

/// Strip a Python `#` comment from `line` without cutting through `#` inside string literals.
fn strip_python_comment<'a>(line: &'a str, line_index: usize, scan: &LineScan) -> &'a str {
    if line_index == 0 && line.starts_with("#!") {
        return "";
    }
    let end = scan.comment_start_at(line_index).unwrap_or(line.len());
    line.get(..end).unwrap_or(line).trim_end()
}

/// Whether line `index` starts a statement rather than continuing an expression from the code above it.
///
/// Blank lines and comment lines are passed over.
/// A file start, a block opener ending in a colon, and a complete statement all start a new statement.
/// Implicit string concatenation (previous code ending in `"` or `'`) does not start a docstring.
fn starts_statement(lines: &[&str], index: usize, scan: &LineScan) -> bool {
    let previous_code = lines
        .get(..index)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .rev()
        .map(|(line_index, line)| strip_python_comment(line, line_index, scan))
        .find(|code| !code.trim_start().is_empty());
    previous_code.is_none_or(|code| !code.ends_with(EXPRESSION_CONTINUATIONS) && !code.ends_with(['"', '\'']))
}

/// Build the replacement line pairs that move docstring quotes sharing a line with prose onto their own line.
///
/// Returns one entry per input line, `Some((first_line, second_line))` for the lines to split,
/// and the violations found.
/// A docstring written on one line is left alone,
/// since its quotes belong with the single sentence they wrap.
#[must_use]
pub(super) fn fix_docstring_quotes(
    lines: &[&str],
    kind: FileKind,
    options: &FormatOptions,
    scan: &LineScan,
) -> (Vec<Option<(String, String)>>, Vec<Violation>) {
    let mut replacements = vec![None; lines.len()];
    let mut violations = Vec::new();
    if !kind.comment_style().docstrings {
        return (replacements, violations);
    }
    let mut index = 0;
    while index < lines.len() {
        // A triple quote inside another string is not a docstring, so the scan decides where one can start.
        if scan.starts_inside_string(index) {
            index += 1;
            continue;
        }
        let Some(docstring) = Docstring::at_scanned(lines, index, scan) else {
            index += 1;
            continue;
        };
        index = docstring.close_line + 1;
        // The docstring is reflowed as one unit, so overlapping the selection is enough to take all of it.
        if !options.line_ranges.intersects(docstring.open_line, index) {
            continue;
        }
        // A docstring the reflow turns into a one line docstring keeps its quotes around that one sentence.
        if docstring.opening_shares_line()
            && docstring.closing_shares_line()
            && collapses_to_one_line(lines, &docstring, options)
        {
            continue;
        }
        if docstring.opening_shares_line() {
            let column = docstring.indent.chars().count() + docstring.string_prefix.chars().count() + 1;
            violations.push(Violation {
                line: docstring.open_line + 1,
                column: Some(column),
                kind: ViolationKind::DocstringQuotes,
                message: "prose shares a line with the opening docstring quotes, move the quotes to their own line"
                    .into(),
                fixable: true,
            });
            if let Some(slot) = replacements.get_mut(docstring.open_line) {
                *slot = Some((
                    format!("{}{}{}", docstring.indent, docstring.string_prefix, docstring.quote),
                    format!("{}{}", docstring.indent, docstring.opening_text.trim()),
                ));
            }
        }
        if docstring.closing_shares_line() {
            violations.push(Violation {
                line: docstring.close_line + 1,
                column: Some(docstring.closing_text.chars().count() + 1),
                kind: ViolationKind::DocstringQuotes,
                message: "prose shares a line with the closing docstring quotes, move the quotes to their own line"
                    .into(),
                fixable: true,
            });
            if let Some(slot) = replacements.get_mut(docstring.close_line) {
                *slot = Some((
                    docstring.closing_text.trim_end().to_string(),
                    format!("{}{}", docstring.indent, docstring.quote),
                ));
            }
        }
    }
    (replacements, violations)
}

/// The content lines of a docstring with the indentation stripped, in the order they are read.
pub(super) fn docstring_contents<'a>(lines: &[&'a str], docstring: &Docstring<'a>) -> Vec<&'a str> {
    let mut contents = Vec::with_capacity(docstring.close_line - docstring.open_line + 1);
    if docstring.opening_shares_line() {
        contents.push(docstring.opening_text);
    }
    for body_line in lines
        .get(docstring.open_line + 1..docstring.close_line)
        .unwrap_or_default()
    {
        contents.push(
            body_line
                .strip_prefix(docstring.indent)
                .unwrap_or_else(|| body_line.trim_start()),
        );
    }
    if docstring.closing_shares_line() {
        contents.push(
            docstring
                .closing_text
                .strip_prefix(docstring.indent)
                .unwrap_or_else(|| docstring.closing_text.trim_start()),
        );
    }
    contents
}

/// Whether the whole docstring ends up on one line once the prose is reflowed.
///
/// A docstring holding one sentence that fits is written on a single line,
/// so its quotes belong with that sentence instead of on lines of their own.
/// The decision is the reflow's own, taken over the paragraph the docstring would become,
/// so it cannot drift from what the prose pass does with the same text.
fn collapses_to_one_line(lines: &[&str], docstring: &Docstring<'_>, options: &FormatOptions) -> bool {
    let contents = docstring_contents(lines, docstring);
    // A blank line makes several paragraphs of the docstring, and those never share one line.
    if contents.iter().any(|line| line.trim().is_empty()) {
        return false;
    }
    let quote_width = docstring.quote.chars().count();
    let first_prefix = format!("{}{}{}", docstring.indent, docstring.string_prefix, docstring.quote);
    let opening_width = prefix_width(&first_prefix, options.tab_width);
    // Every pair of content lines is joined by one space, and the closing quotes follow the last word.
    let joined_width =
        contents.iter().map(|line| line.trim().chars().count()).sum::<usize>() + contents.len().saturating_sub(1);
    if opening_width + joined_width + quote_width > options.max_width {
        return false;
    }
    let (regions, _) = markdown::split_paragraphs_with_notices(&contents, docstring.indent, docstring.open_line, false);
    let [Region::Paragraph(paragraph)] = regions.as_slice() else {
        return false;
    };
    let candidate = Paragraph {
        first_prefix,
        last_suffix: docstring.quote.to_string(),
        ..paragraph.clone()
    };
    let fits = |line: &String| opening_width + line.chars().count() + quote_width <= options.max_width;
    match reflow_paragraph(&candidate, options, true).lines {
        Some(reflowed) => matches!(reflowed.as_slice(), [line] if fits(line)),
        None => candidate.lines.len() == 1,
    }
}

#[cfg(test)]
mod test_docstring_parsing {
    use super::*;

    #[test]
    fn a_multi_line_docstring_reports_its_parts() {
        let lines = ["def f():", "    \"\"\"Summary line.", "    More body.", "    \"\"\""];
        let docstring = Docstring::at(&lines, 1).expect("the docstring should be found");
        assert_eq!((docstring.open_line, docstring.close_line), (1, 3));
        assert_eq!(docstring.indent, "    ");
        assert_eq!(docstring.string_prefix, "");
        assert_eq!(docstring.quote, "\"\"\"");
        assert_eq!(docstring.opening_text, "Summary line.");
        assert_eq!(docstring.closing_text, "    ");
        assert!(docstring.opening_shares_line());
        assert!(!docstring.closing_shares_line());
    }

    #[test]
    fn a_raw_single_quoted_docstring_keeps_its_prefix_and_quotes() {
        let lines = ["r'''Summary.", "More.", "'''"];
        let docstring = Docstring::at(&lines, 0).expect("the docstring should be found");
        assert_eq!(docstring.string_prefix, "r");
        assert_eq!(docstring.quote, "'''");
    }

    #[test]
    fn escaped_triple_quotes_do_not_close_raw_docstrings() {
        for quote in ["\"\"\"", "'''"] {
            for prefix in ["r", "R"] {
                let opening = format!("{prefix}{quote}Summary.");
                let escaped = format!("Example: \\{quote}");
                let lines = [opening.as_str(), escaped.as_str(), "Actual body.", quote];
                let docstring = Docstring::at(&lines, 0).unwrap();
                assert_eq!(docstring.close_line, 3, "{lines:?}");
                assert!(docstring.closing_text.is_empty());
            }
        }
    }

    #[test]
    fn even_backslashes_do_not_escape_raw_docstring_quotes() {
        let lines = ["r'''Summary.", "Ends with \\\\'''", "unrelated = 1"];
        assert_eq!(Docstring::at(&lines, 0).unwrap().close_line, 1);
    }

    #[test]
    fn prose_on_the_closing_line_is_reported() {
        let lines = ["    \"\"\"Summary line.", "    more text.\"\"\""];
        let docstring = Docstring::at(&lines, 0).expect("the docstring should be found");
        assert_eq!(docstring.closing_text, "    more text.");
        assert!(docstring.closing_shares_line());
    }

    #[test]
    fn a_one_line_docstring_and_code_after_the_quotes_are_not_docstring_blocks() {
        assert_eq!(Docstring::at(&["    \"\"\"Just this.\"\"\""], 0), None);
        assert_eq!(Docstring::at(&["    \"\"\"Summary.", "    \"\"\" + tail"], 0), None);
        assert_eq!(Docstring::at(&["    \"\"\"Summary.", "    no close here"], 0), None);
        assert_eq!(Docstring::at(&["value = \"\"\"Summary.", "\"\"\""], 0), None);
    }
}

#[cfg(test)]
mod test_docstring_position {
    use super::*;

    const BODY: [&str; 2] = ["    \"\"\"", "    Some prose."];

    /// The given lines followed by a docstring opening and body, and the index the docstring opens on.
    fn with_body<'a>(before: &[&'a str]) -> (Vec<&'a str>, usize) {
        let mut lines = before.to_vec();
        let index = lines.len();
        lines.extend(BODY);
        lines.push("    \"\"\"");
        (lines, index)
    }

    #[test]
    fn a_string_continuing_an_expression_is_not_a_docstring() {
        for before in [
            &["y = ("][..],
            &["QUERIES = ["],
            &["call("],
            &["value = {"],
            &["    first,"],
            &["total = a + \\"],
        ] {
            let (lines, index) = with_body(before);
            assert_eq!(Docstring::at(&lines, index), None, "{before:?}");
        }
    }

    #[test]
    fn a_triple_quoted_dictionary_value_is_preserved() {
        let source = concat!(
            "settings = {\n",
            "    \"message\":  # Display text.\n",
            "\n",
            "        \"\"\"Leave the punctuation; it belongs to the value.\n",
            "        Leave the line breaks too.\"\"\"\n",
            "}\n",
        );
        let formatted = crate::format(source, FileKind::Python, &FormatOptions::default());
        assert_eq!(formatted.fixed_text.as_deref().unwrap_or(source), source);
        assert!(formatted.violations.is_empty());
    }

    #[test]
    fn bracket_characters_in_strings_and_comments_do_not_change_expression_nesting() {
        let source = concat!(
            "value = \"{\"  # [\n",
            "def example(\n",
            "    labels=(\")\",),  # {\n",
            "):\n",
            "    \"\"\"This is prose; it should be formatted.\n",
            "    It is not an argument.\"\"\"\n",
        );
        let formatted = crate::format(source, FileKind::Python, &FormatOptions::default());
        assert!(formatted.fixed_text.unwrap().contains("This is prose.\n"));
    }

    #[test]
    fn a_string_as_the_first_statement_is_a_docstring() {
        let cases: [&[&str]; 5] = [
            &[],
            &["#!/usr/bin/env python3", "# A comment."],
            &["def f():"],
            &["class Thing(Base):  # trailing note"],
            &["def f(", "    first,", ") -> int:"],
        ];
        for before in cases {
            let (lines, index) = with_body(before);
            assert!(Docstring::at(&lines, index).is_some(), "{before:?}");
        }
    }

    #[test]
    fn a_string_after_a_complete_statement_is_an_attribute_docstring() {
        let (lines, index) = with_body(&["value = 1", ""]);
        assert!(Docstring::at(&lines, index).is_some());
    }
}

#[cfg(test)]
mod test_docstring_quotes {
    use super::super::comments::scan_lines;
    use super::*;

    /// Apply the docstring quote fix to the given Python lines and return the fixed lines.
    fn fix(lines: &[&str]) -> (Vec<String>, Vec<Violation>) {
        let options = FormatOptions::default();
        let scan = scan_lines(lines, FileKind::Python);
        let (replacements, violations) = fix_docstring_quotes(lines, FileKind::Python, &options, &scan);
        let mut fixed = Vec::new();
        for (line, replacement) in lines.iter().zip(replacements) {
            match replacement {
                Some((first, second)) => {
                    fixed.push(first);
                    fixed.push(second);
                }
                None => fixed.push((*line).to_string()),
            }
        }
        (fixed, violations)
    }

    #[test]
    fn opening_quotes_move_to_their_own_line() {
        let (fixed, violations) = fix(&[
            "def parse(path):",
            "    \"\"\"Parse the file at the given path.",
            "    Missing files raise.",
            "    \"\"\"",
        ]);
        assert_eq!(
            fixed,
            vec![
                "def parse(path):",
                "    \"\"\"",
                "    Parse the file at the given path.",
                "    Missing files raise.",
                "    \"\"\"",
            ]
        );
        assert_eq!(violations.len(), 1);
        assert_eq!((violations[0].line, violations[0].column), (2, Some(5)));
        assert_eq!(violations[0].kind, ViolationKind::DocstringQuotes);
        assert!(violations[0].fixable);
    }

    #[test]
    fn both_sets_of_quotes_move_to_their_own_lines() {
        let (fixed, violations) = fix(&["\"\"\"Summary line.", "More body.\"\"\""]);
        assert_eq!(fixed, vec!["\"\"\"", "Summary line.", "More body.", "\"\"\""]);
        assert_eq!(violations.len(), 2);
        assert_eq!(violations[1].line, 2);
        assert_eq!(violations[1].column, Some(11));
    }

    #[test]
    fn quotes_already_on_their_own_lines_are_left_alone() {
        let (fixed, violations) = fix(&["    \"\"\"", "    Body text.", "    \"\"\""]);
        assert_eq!(fixed, vec!["    \"\"\"", "    Body text.", "    \"\"\""]);
        assert_eq!(violations, [] as [Violation; 0]);
    }

    #[test]
    fn a_one_line_docstring_and_plain_strings_are_left_alone() {
        let lines = [
            "    \"\"\"Just this.\"\"\"",
            "    value = \"\"\"raw",
            "    text\"\"\"",
            "    other = 1",
        ];
        let (fixed, violations) = fix(&lines);
        assert_eq!(fixed, lines.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(violations, [] as [Violation; 0]);
    }

    #[test]
    fn a_docstring_outside_the_selection_is_left_alone() {
        let lines = [
            "    \"\"\"Summary line.",
            "    More body.",
            "    \"\"\"",
            "    value = 1",
        ];
        let options = FormatOptions {
            line_ranges: "4".parse().expect("the range should parse"),
            ..FormatOptions::default()
        };
        let scan = scan_lines(&lines, FileKind::Python);
        let (replacements, violations) = fix_docstring_quotes(&lines, FileKind::Python, &options, &scan);
        assert!(replacements.iter().all(Option::is_none));
        assert_eq!(violations, [] as [Violation; 0]);
    }

    #[test]
    fn languages_without_docstrings_are_never_touched() {
        let lines = ["/// Summary line.", "/// More body."];
        let scan = scan_lines(&lines, FileKind::Rust);
        let (replacements, violations) = fix_docstring_quotes(&lines, FileKind::Rust, &FormatOptions::default(), &scan);
        assert!(replacements.iter().all(Option::is_none));
        assert_eq!(violations, [] as [Violation; 0]);
    }

    #[test]
    fn a_short_wrapping_docstring_keeps_quotes_around_one_line() {
        let (fixed, violations) = fix(&["    \"\"\"Return the label", "    of the given value.\"\"\""]);
        assert_eq!(
            fixed,
            vec!["    \"\"\"Return the label", "    of the given value.\"\"\""]
        );
        assert_eq!(violations, [] as [Violation; 0]);
    }

    #[test]
    fn a_blank_line_in_a_docstring_moves_the_quotes() {
        let (fixed, violations) = fix(&[
            "    \"\"\"Short summary.",
            "",
            "    More detail that still fits the width.",
            "    \"\"\"",
        ]);
        assert_eq!(
            fixed,
            vec![
                "    \"\"\"",
                "    Short summary.",
                "",
                "    More detail that still fits the width.",
                "    \"\"\"",
            ]
        );
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::DocstringQuotes);
    }

    #[test]
    fn a_heading_inside_a_docstring_moves_the_quotes() {
        // No blank line, but the heading is its own region, so the docstring cannot collapse to one line.
        let (fixed, violations) = fix(&["    \"\"\"# Errors", "    Returns when x.\"\"\""]);
        assert_eq!(
            fixed,
            vec!["    \"\"\"", "    # Errors", "    Returns when x.", "    \"\"\""]
        );
        assert_eq!(violations.len(), 2);
        assert!(
            violations
                .iter()
                .all(|violation| violation.kind == ViolationKind::DocstringQuotes)
        );
    }

    #[test]
    fn a_docstring_too_wide_to_collapse_moves_the_quotes() {
        let options = FormatOptions {
            max_width: 40,
            ..FormatOptions::default()
        };
        let lines = ["    \"\"\"Return the long label of the given value.", "    \"\"\""];
        let scan = scan_lines(&lines, FileKind::Python);
        let (replacements, violations) = fix_docstring_quotes(&lines, FileKind::Python, &options, &scan);
        assert!(replacements[0].is_some());
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::DocstringQuotes);
    }
}
