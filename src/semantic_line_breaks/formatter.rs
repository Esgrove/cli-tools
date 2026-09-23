//! Formatting pipeline for the semantic line breaks formatter.
//!
//! Splits a text buffer into lines while preserving line endings,
//! runs the trailing comment pass and the prose pass,
//! and reassembles the result byte for byte where nothing changed.

use std::borrow::Cow;

use super::comments;
use super::docstrings;
use super::file_kind::FileKind;
use super::line_ranges::LineRanges;
use super::markdown;
use super::markdown::SkipNotice;
use super::options::{FormatOptions, FormatResult};
use super::paragraph::Region;
use super::reflow;
use super::violation::Violation;

/// One line of the input with its original line ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceLine<'a> {
    /// Line text without the line ending.
    text: &'a str,
    /// The line ending, empty for the last line of a file without a final newline.
    eol: &'a str,
}

/// One line of the working buffer, produced by the trailing comment pass, with its line ending.
struct WorkingLine<'a> {
    /// Line text, owned when the trailing comment pass rewrote it.
    text: Cow<'a, str>,
    /// The line ending carried over from the source line it came from.
    eol: &'a str,
}

/// Lines a prose pass produced, with the selected ones marked.
///
/// A reflowed paragraph rarely needs as many lines as it came from,
/// so the selection is carried per output line rather than mapped through the line count changes afterwards.
#[derive(Debug, Default)]
struct ProseOutput<'a> {
    /// The output lines.
    lines: Vec<Cow<'a, str>>,
    /// Whether each output line belongs to the selection, empty when the whole text is selected.
    selected: Vec<bool>,
    /// Whether the selection is worth tracking, false when every line is selected anyway.
    track_selection: bool,
}

impl<'a> ProseOutput<'a> {
    /// Output with room for the given number of lines, tracking the selection only when there is one.
    fn new(capacity: usize, track_selection: bool) -> Self {
        Self {
            lines: Vec::with_capacity(capacity),
            selected: Vec::with_capacity(if track_selection { capacity } else { 0 }),
            track_selection,
        }
    }

    /// Add one line that belongs to the selection.
    fn push_selected(&mut self, line: Cow<'a, str>) {
        self.lines.push(line);
        if self.track_selection {
            self.selected.push(true);
        }
    }

    /// Copy the lines in `from..to` unchanged, keeping the selection each of them had.
    fn copy(&mut self, lines: &[&'a str], from: usize, to: usize, ranges: &LineRanges) {
        if from >= to {
            return;
        }
        let copied = lines.get(from..to).unwrap_or_default();
        self.lines.extend(copied.iter().copied().map(Cow::Borrowed));
        if self.track_selection {
            self.selected
                .extend((from..from + copied.len()).map(|index| ranges.contains_line(index + 1)));
        }
    }

    /// The selected lines as ranges over the output, empty when the whole text was selected.
    fn ranges(&self) -> LineRanges {
        LineRanges::from_flags(&self.selected)
    }
}

/// Check a text buffer and return the violations and skip notices without producing fixed text.
#[must_use]
pub fn check(text: &str, kind: FileKind, options: &FormatOptions) -> FormatResult {
    run(text, kind, options, false)
}

/// Check and fix a text buffer.
#[must_use]
pub fn format(text: &str, kind: FileKind, options: &FormatOptions) -> FormatResult {
    run(text, kind, options, true)
}

/// Run the pipeline over a text buffer, building the fixed text only when `produce_fix` is set.
///
/// Violations always describe the original text.
/// Moving a trailing comment to its own line shifts every line number after it,
/// so the fixed text needs a second prose pass over the rewritten lines,
/// which is skipped when the caller only asked for the violations.
fn run(text: &str, kind: FileKind, options: &FormatOptions, produce_fix: bool) -> FormatResult {
    let source = split_lines(text);
    let texts: Vec<&str> = source.iter().map(|line| line.text).collect();
    if markdown::has_ignore_file_marker(&texts) {
        return FormatResult::default();
    }

    let mut violations = Vec::new();
    // The line splitting passes turn one line into two, so every line below the split moves down.
    let mut replacements: Vec<Option<(String, String)>> = vec![None; source.len()];
    // The trailing comment pass, the docstring quote pass,
    // and the region split all need the same scan of the same lines,
    // so it is taken once here and handed to each of them.
    let scan = (kind != FileKind::Markdown).then(|| comments::scan_lines(&texts, kind));
    if let Some(scan) = &scan {
        if kind.supports_trailing_comment_check() && options.rules.trailing_comment {
            let (trailing_replacements, trailing_violations) =
                comments::fix_trailing_comments_scanned(&texts, kind, options, scan);
            violations.extend(trailing_violations);
            merge_replacements(&mut replacements, trailing_replacements);
        }
        if options.rules.docstring_quotes {
            let (quote_replacements, quote_violations) = docstrings::fix_docstring_quotes(&texts, kind, options, scan);
            violations.extend(quote_violations);
            merge_replacements(&mut replacements, quote_replacements);
        }
    }
    let lines_changed = replacements.iter().any(Option::is_some);
    // The selection is given in the original line numbers,
    // so it has to follow the lines the splitting passes added before the prose pass can use it.
    let working_ranges = if lines_changed {
        shift_ranges(&options.line_ranges, &replacements)
    } else {
        options.line_ranges.clone()
    };
    let working = working_lines(&source, replacements);

    let working_texts: Vec<&str> = working.iter().map(|line| line.text.as_ref()).collect();
    let (output, prose_violations, notices) = if lines_changed {
        // Splitting a line shifts the lines below it,
        // so the violations come from the original numbering and the fixed text from the new lines,
        // which need their own scan.
        let (_, original_violations, original_notices) =
            prose_pass(&texts, kind, options, &options.line_ranges, false, scan.as_ref());
        let output = produce_fix.then(|| prose_pass(&working_texts, kind, options, &working_ranges, true, None).0);
        (output, original_violations, original_notices)
    } else {
        let (output, prose_violations, notices) = prose_pass(
            &working_texts,
            kind,
            options,
            &working_ranges,
            produce_fix,
            scan.as_ref(),
        );
        (produce_fix.then_some(output), prose_violations, notices)
    };
    violations.extend(prose_violations);
    violations.sort_by_key(|violation| (violation.line, violation.kind));

    let mut fixed_line_ranges = LineRanges::default();
    let fixed_text = output.and_then(|output| {
        let fixed = assemble(&output.lines, &working, text);
        fixed_line_ranges = output.ranges();
        (fixed != text).then_some(fixed)
    });
    FormatResult {
        violations,
        fixed_text,
        fixed_line_ranges,
        skips: notices,
    }
}

/// Split text into lines keeping each line's own ending.
fn split_lines(text: &str) -> Vec<SourceLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, _) in text.match_indices('\n') {
        let line_end = if text.get(..index).is_some_and(|before| before.ends_with('\r')) {
            index - 1
        } else {
            index
        };
        lines.push(SourceLine {
            text: text.get(start..line_end).unwrap_or_default(),
            eol: text.get(line_end..=index).unwrap_or_default(),
        });
        start = index + 1;
    }
    if start < text.len() {
        lines.push(SourceLine {
            text: text.get(start..).unwrap_or_default(),
            eol: "",
        });
    }
    lines
}

/// Reflow every selected prose paragraph in the lines and return the new lines and the violations found.
///
/// The lines are only collected when `produce_fix` is set,
/// so a check run never builds the text it would not use.
///
/// A paragraph outside `ranges` is left alone entirely,
/// so neither a violation nor a change comes out of a line the caller did not ask about.
/// The ranges are the ones matching `lines`,
/// which are not the original line numbers once the trailing comment pass has added lines of its own.
fn prose_pass<'a>(
    lines: &[&'a str],
    kind: FileKind,
    options: &FormatOptions,
    ranges: &LineRanges,
    produce_fix: bool,
    scan: Option<&comments::LineScan>,
) -> (ProseOutput<'a>, Vec<Violation>, Vec<SkipNotice>) {
    let (regions, notices) = if kind == FileKind::Markdown {
        markdown::split_paragraphs_with_notices(lines, "", 0, true)
    } else {
        scan.map_or_else(
            || comments::split_source_regions_with_notices(lines, kind),
            |scan| comments::split_source_regions_scanned_with_notices(lines, kind, scan),
        )
    };
    let capacity = if produce_fix { lines.len() } else { 0 };
    let mut output = ProseOutput::new(capacity, produce_fix && !ranges.is_empty());
    let mut violations = Vec::new();
    let mut cursor = 0;
    for region in regions {
        match region {
            Region::Verbatim { end, .. } => {
                if produce_fix {
                    output.copy(lines, cursor, end, ranges);
                }
                cursor = cursor.max(end);
            }
            Region::Paragraph(paragraph) => {
                // A paragraph is reflowed as one unit, so overlapping the selection is enough to take all of it,
                // and a paragraph the selection misses is copied with the lines leading up to it.
                if !ranges.intersects(paragraph.start_line, paragraph.end_line) {
                    if produce_fix {
                        output.copy(lines, cursor, paragraph.end_line, ranges);
                    }
                    cursor = cursor.max(paragraph.end_line);
                    continue;
                }
                if produce_fix {
                    output.copy(lines, cursor, paragraph.start_line, ranges);
                }
                let outcome = reflow::reflow_paragraph(&paragraph, options, produce_fix);
                violations.extend(outcome.violations);
                if produce_fix {
                    match outcome.lines {
                        Some(new_lines) => {
                            let last_index = new_lines.len().saturating_sub(1);
                            for (index, content) in new_lines.iter().enumerate() {
                                let suffix = if index == last_index {
                                    paragraph.last_suffix.as_str()
                                } else {
                                    ""
                                };
                                output.push_selected(Cow::Owned(format!(
                                    "{}{content}{suffix}",
                                    paragraph.prefix_for(index)
                                )));
                            }
                        }
                        None => output.copy(lines, paragraph.start_line, paragraph.end_line, ranges),
                    }
                }
                cursor = cursor.max(paragraph.end_line);
            }
        }
    }
    if produce_fix {
        output.copy(lines, cursor, lines.len(), ranges);
    }
    (output, violations, notices)
}

/// Take the replacements of one pass into the replacements collected so far.
///
/// Two passes never claim the same line in practice,
/// since a comment sharing a line with code is not inside a docstring,
/// and the line a pass claimed first is kept in case they ever do.
fn merge_replacements(collected: &mut [Option<(String, String)>], from_pass: Vec<Option<(String, String)>>) {
    for (slot, replacement) in collected.iter_mut().zip(from_pass) {
        if slot.is_none() {
            *slot = replacement;
        }
    }
}

/// Build the lines the prose pass works on, splitting each replaced line into the two lines it became.
fn working_lines<'a>(source: &[SourceLine<'a>], replacements: Vec<Option<(String, String)>>) -> Vec<WorkingLine<'a>> {
    let mut working = Vec::with_capacity(source.len());
    for (line, replacement) in source.iter().zip(replacements) {
        match replacement {
            Some((first, second)) => {
                working.push(WorkingLine {
                    text: Cow::Owned(first),
                    eol: line.eol,
                });
                working.push(WorkingLine {
                    text: Cow::Owned(second),
                    eol: line.eol,
                });
            }
            None => working.push(WorkingLine {
                text: Cow::Borrowed(line.text),
                eol: line.eol,
            }),
        }
    }
    working
}

/// Move a selection of original lines onto the lines the line splitting passes produced.
///
/// Every applied replacement turns one line into two,
/// so a line moves down by the number of replacements above it,
/// and a replaced line covers both of the lines it became.
fn shift_ranges(ranges: &LineRanges, replacements: &[Option<(String, String)>]) -> LineRanges {
    if ranges.is_empty() || replacements.iter().all(Option::is_none) {
        return ranges.clone();
    }
    // Number of replacements strictly above each one based line, so the shift of a line is a lookup.
    let mut shifts = Vec::with_capacity(replacements.len() + 1);
    let mut applied = 0;
    for replacement in replacements {
        shifts.push(applied);
        if replacement.is_some() {
            applied += 1;
        }
    }
    shifts.push(applied);
    let shift_of = |line: usize| shifts.get(line - 1).copied().unwrap_or(applied);
    let was_replaced = |line: usize| replacements.get(line - 1).is_some_and(Option::is_some);
    LineRanges::new(ranges.iter().map(|range| {
        let start = range.start() + shift_of(*range.start());
        let end = range.end() + shift_of(*range.end()) + usize::from(was_replaced(*range.end()));
        start..=end
    }))
}

/// Join output lines with line endings taken from the working lines.
///
/// When the line counts differ, the dominant line ending of the original text is used.
fn assemble(output: &[Cow<'_, str>], working: &[WorkingLine<'_>], original: &str) -> String {
    let default_eol = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_newline = original.ends_with('\n') || original.is_empty();
    let mut result = String::with_capacity(original.len() + 64);
    let last_index = output.len().saturating_sub(1);
    for (index, line) in output.iter().enumerate() {
        result.push_str(line.as_ref());
        if index == last_index {
            if trailing_newline && !output.is_empty() {
                let eol = working.get(index).map_or(default_eol, |line| line.eol);
                result.push_str(if eol.is_empty() { default_eol } else { eol });
            }
        } else {
            let eol = if output.len() == working.len() {
                working.get(index).map_or(default_eol, |line| line.eol)
            } else {
                default_eol
            };
            result.push_str(if eol.is_empty() { default_eol } else { eol });
        }
    }
    result
}

#[cfg(test)]
mod test_split_and_assemble {
    use super::*;

    #[test]
    fn split_lines_preserves_line_endings() {
        let lines = split_lines("a\r\nb\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].text, lines[0].eol), ("a", "\r\n"));
        assert_eq!((lines[1].text, lines[1].eol), ("b", "\n"));
        assert_eq!((lines[2].text, lines[2].eol), ("c", ""));
        assert!(split_lines("").is_empty());
        assert_eq!(split_lines("\n").len(), 1);
    }

    #[test]
    fn unchanged_text_round_trips_byte_for_byte() {
        for text in ["a\r\nb\r\n", "a\nb", "\n\n", "", "x\n\ny\n"] {
            let result = format(text, FileKind::Markdown, &FormatOptions::default());
            assert_eq!(result.fixed_text, None, "{text:?}");
        }
    }
}

#[cfg(test)]
mod test_format {
    use super::*;
    use crate::semantic_line_breaks::options::RuleSet;
    use crate::semantic_line_breaks::violation::ViolationKind;

    /// Options with every rule on, including the opt-in trailing comment rule.
    fn all_rules() -> FormatOptions {
        FormatOptions {
            rules: RuleSet::ALL,
            ..FormatOptions::default()
        }
    }

    #[test]
    fn check_matches_format_violations() {
        let text = "/// the quick brown fox\n/// jumps over; it is fast — really.\nfn f() {}\n";
        let options = FormatOptions::default();
        assert_eq!(
            check(text, FileKind::Rust, &options).violations,
            format(text, FileKind::Rust, &options).violations
        );
    }

    #[test]
    fn a_code_like_comment_line_is_reported_as_a_skip_notice() {
        let text = "/// Some prose here.\n/// let x = foo(bar);\nfn f() {}\n";
        let result = format(text, FileKind::Rust, &FormatOptions::default());
        assert_eq!(result.fixed_text, None);
        assert_eq!(result.skips.len(), 1);
        assert_eq!(result.skips[0].message, "the line looks like code");
    }

    #[test]
    fn reflows_rust_doc_comment_and_keeps_code() {
        let text = "/// the quick brown fox\n/// jumps over; it is fast — really.\nfn f() {}\n";
        let result = format(text, FileKind::Rust, &FormatOptions::default());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some("/// the quick brown fox jumps over.\n/// It is fast, really.\nfn f() {}\n")
        );
        let kinds: Vec<ViolationKind> = result.violations.iter().map(|violation| violation.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ViolationKind::MidClauseBreak,
                ViolationKind::Semicolon,
                ViolationKind::EmDash
            ]
        );
    }

    #[test]
    fn moves_trailing_comment_above_code_with_crlf() {
        let text = "fn f() {\r\n    let x = 1; // one\r\n}\r\n";
        let result = format(text, FileKind::Rust, &all_rules());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some("fn f() {\r\n    // one\r\n    let x = 1;\r\n}\r\n")
        );
        assert_eq!(result.violations[0].kind, ViolationKind::TrailingComment);
        assert_eq!(result.violations[0].line, 2);
    }

    #[test]
    fn a_trailing_comment_under_a_comment_is_reported_but_left_in_place() {
        let text = concat!(
            "// Various dotted prefixes all below threshold of 15\n",
            "std::fs::write(root.join(\"One.Two.File.001.mp4\"), \"\").unwrap(); // 6 chars\n",
        );
        let result = format(text, FileKind::Rust, &all_rules());
        assert_eq!(result.fixed_text, None);
        assert_eq!(result.violations.len(), 1);
        assert_eq!(result.violations[0].kind, ViolationKind::TrailingComment);
        assert_eq!(result.violations[0].line, 2);
        assert!(!result.violations[0].fixable);
    }

    #[test]
    fn a_trailing_comment_without_a_comment_above_is_still_moved() {
        let text = "let x = 1; // 6 chars\n";
        let result = format(text, FileKind::Rust, &all_rules());
        assert_eq!(result.fixed_text.as_deref(), Some("// 6 chars\nlet x = 1;\n"));
        assert!(result.violations[0].fixable);
        // The fixed text is stable, the moved comment is not merged into anything.
        let second = format("// 6 chars\nlet x = 1;\n", FileKind::Rust, &all_rules());
        assert_eq!(second.fixed_text, None);
        assert!(second.violations.is_empty());
    }

    #[test]
    fn the_trailing_rule_is_off_unless_it_is_enabled() {
        let text = "let x = 1; // one\n";
        let result = format(text, FileKind::Rust, &FormatOptions::default());
        assert_eq!(result.fixed_text, None);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn ignore_file_marker_disables_formatting() {
        let text = "<!-- slb-ignore-file -->\nthe quick brown fox\njumps over the dog.\n";
        let result = format(text, FileKind::Markdown, &FormatOptions::default());
        assert_eq!(result, FormatResult::default());
    }

    #[test]
    fn a_multi_line_python_docstring_gets_its_own_quote_lines() {
        let text = "def f():\n    \"\"\"Parse the header\n    of the file and return it.\n\n    Args:\n        x: the input\n    \"\"\"\n    return 1\n";
        let result = format(text, FileKind::Python, &FormatOptions::default());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some(
                "def f():\n    \"\"\"\n    Parse the header of the file and return it.\n\n    Args:\n        x: the input\n    \"\"\"\n    return 1\n"
            )
        );
        assert!(
            result
                .violations
                .iter()
                .any(|violation| violation.kind == ViolationKind::DocstringQuotes && violation.line == 2)
        );
    }

    #[test]
    fn a_one_line_python_docstring_keeps_its_quotes_in_place() {
        let text = "def f():\n    \"\"\"Parse the header of the file.\"\"\"\n    return 1\n";
        let result = format(text, FileKind::Python, &FormatOptions::default());
        assert_eq!(result.fixed_text, None);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn the_docstring_quote_rule_can_be_turned_off() {
        let text = "def f():\n    \"\"\"Parse the header of the file.\n    Missing files raise.\n    \"\"\"\n";
        let options = FormatOptions {
            rules: RuleSet {
                docstring_quotes: false,
                ..RuleSet::DEFAULT
            },
            ..FormatOptions::default()
        };
        let result = format(text, FileKind::Python, &options);
        assert!(
            result
                .violations
                .iter()
                .all(|violation| violation.kind != ViolationKind::DocstringQuotes)
        );
    }

    #[test]
    fn markdown_list_item_is_reflowed_with_continuation_indent() {
        let text = "# Title\n\n- the quick brown fox\n  jumps over the dog.\n- second item.\n";
        let result = format(text, FileKind::Markdown, &FormatOptions::default());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some("# Title\n\n- the quick brown fox jumps over the dog.\n- second item.\n")
        );
    }

    #[test]
    fn formatting_is_idempotent_on_fixed_output() {
        let text = "//! Module docs that were hard wrapped at eighty\n//! columns by an editor; which is exactly the kind\n//! of text the formatter repairs — always.\n\nfn main() {\n    let width = 80; // default\n}\n";
        let options = FormatOptions::default();
        let first = format(text, FileKind::Rust, &options);
        let fixed = first.fixed_text.expect("first pass should change the text");
        let second = format(&fixed, FileKind::Rust, &options);
        assert_eq!(second.fixed_text, None, "second pass changed:\n{fixed}");
        assert!(second.violations.iter().all(|violation| !violation.fixable));
    }

    #[test]
    fn regex_object_is_preserved_while_real_comments_are_formatted() {
        let text = concat!(
            "const config = {\n",
            "  test: /node_modules\\/(react|react-dom)\\//,\n",
            "  name: 'react',\n",
            "  chunks: 'all',\n",
            "};\n",
            "const expression = /[/*]/g; // Match a delimiter.\n",
            "const next = 1; // Keep this comment.\n",
        );
        let expected = concat!(
            "const config = {\n",
            "  test: /node_modules\\/(react|react-dom)\\//,\n",
            "  name: 'react',\n",
            "  chunks: 'all',\n",
            "};\n",
            "// Match a delimiter.\n",
            "const expression = /[/*]/g;\n",
            "// Keep this comment.\n",
            "const next = 1;\n",
        );
        let options = all_rules();
        for kind in [FileKind::JavaScript, FileKind::CLike, FileKind::Rust, FileKind::Go] {
            let first = format(text, kind, &options);
            assert_eq!(first.fixed_text.as_deref(), Some(expected), "{kind:?}");
            assert_eq!(check(text, kind, &options).violations, first.violations);
            assert_eq!(format(expected, kind, &options), FormatResult::default());
        }
    }

    #[test]
    fn regex_only_source_needs_no_formatting() {
        let text = concat!(
            "const config = {\n",
            "  test: /node_modules\\/(react|react-dom)\\//,\n",
            "};\n",
            "if (ready) /path\\//.test(value);\n",
            "const expression = /[/*\"'`]/g;\n",
            "const nested = /[[a]--[/]]/v;\n",
        );
        let options = FormatOptions::default();
        for kind in [FileKind::JavaScript, FileKind::CLike, FileKind::Rust, FileKind::Go] {
            assert_eq!(format(text, kind, &options), FormatResult::default(), "{kind:?}");
            assert!(check(text, kind, &options).violations.is_empty());
        }
    }

    #[test]
    fn contextual_identifiers_format_real_comments_without_changing_block_contents() {
        let text = concat!(
            "let value = new / divisor; // note\n",
            "let value = new / divisor; /// doc note\n",
            "let value = new / divisor; //// longer note\n",
            "let value = new / divisor; /* block\n",
            "code // literal; not prose\n",
            "code /// literal; not prose\n",
            "code //// literal; not prose\n",
            "// wrapped block\n",
            "// content.\n",
            "*/ let next = 1; // next note\n",
        );
        let expected = concat!(
            "// note\n",
            "let value = new / divisor;\n",
            "let value = new / divisor; /// doc note\n",
            "let value = new / divisor; //// longer note\n",
            "let value = new / divisor; /* block\n",
            "code // literal; not prose\n",
            "code /// literal; not prose\n",
            "code //// literal; not prose\n",
            "// wrapped block\n",
            "// content.\n",
            // The block content line above the code keeps the trailing comment in place,
            // so nothing is inserted inside the block comment.
            "*/ let next = 1; // next note\n",
        );
        let options = all_rules();
        for kind in [FileKind::Rust, FileKind::Go, FileKind::CLike, FileKind::JavaScript] {
            let first = format(text, kind, &options);
            assert_eq!(first.fixed_text.as_deref(), Some(expected), "{kind:?}");
            assert_eq!(check(text, kind, &options).violations, first.violations, "{kind:?}");
            // The trailing comment that cannot be moved is still reported, so only the fix is checked.
            let second = format(expected, kind, &options);
            assert_eq!(second.fixed_text, None, "{kind:?}");
            assert!(
                second.violations.iter().all(|violation| !violation.fixable),
                "{kind:?}: {:?}",
                second.violations
            );
        }
    }

    #[test]
    fn contextual_regexes_keep_attached_comments_and_format_idempotently() {
        let text = concat!(
            "return /foo/;\n",
            "return /foo/// attached note\n",
            "return /foo;/// comment\n",
            "return /foo;//// comment\n",
            "return /path\\//// note\n",
            "new /[/*]//* block\n",
            "code // literal; not prose\n",
            "*/ let next = 1; // next note\n",
        );
        let expected = concat!(
            "return /foo/;\n",
            "// attached note\n",
            "return /foo/\n",
            "return /foo;/// comment\n",
            "return /foo;//// comment\n",
            "// note\n",
            "return /path\\//\n",
            "new /[/*]//* block\n",
            "code // literal; not prose\n",
            "// next note\n",
            "*/ let next = 1;\n",
        );
        let options = all_rules();
        for kind in [FileKind::Rust, FileKind::Go, FileKind::CLike, FileKind::JavaScript] {
            let first = format(text, kind, &options);
            assert_eq!(first.fixed_text.as_deref(), Some(expected), "{kind:?}");
            assert_eq!(check(text, kind, &options).violations, first.violations, "{kind:?}");
            assert_eq!(format(expected, kind, &options), FormatResult::default(), "{kind:?}");
        }
    }

    #[test]
    fn regex_hashes_are_preserved_while_real_hash_comments_are_formatted() {
        let text = "pattern = /[ #/]/ # Match a delimiter.\nnext = 1 # Keep this comment.\n";
        let expected = "# Match a delimiter.\npattern = /[ #/]/\n# Keep this comment.\nnext = 1\n";
        let options = all_rules();
        for kind in [FileKind::Python, FileKind::Shell, FileKind::Toml, FileKind::Yaml] {
            assert_eq!(
                format(text, kind, &options).fixed_text.as_deref(),
                Some(expected),
                "{kind:?}"
            );
            assert_eq!(format(expected, kind, &options), FormatResult::default(), "{kind:?}");
        }
    }

    #[test]
    fn quoted_slashes_in_division_and_paths_preserve_code() {
        let options = all_rules();
        for (kind, code) in [
            (FileKind::Python, r#"value = default / len("/# not a comment")"#),
            (FileKind::Python, r#"value = default / len(["]/# not a comment"])"#),
            (FileKind::Shell, r#"path=/"foo/bar # literal""#),
            (FileKind::Shell, r#"path=/["]foo/bar # literal""#),
        ] {
            assert_eq!(format(code, kind, &options), FormatResult::default(), "{kind:?}");
            let text = format!("{code}\nnext = 1 # Keep this comment.\n");
            let expected = format!("{code}\n# Keep this comment.\nnext = 1\n");
            assert_eq!(
                format(&text, kind, &options).fixed_text.as_deref(),
                Some(expected.as_str()),
                "{kind:?}"
            );
            assert_eq!(format(&expected, kind, &options), FormatResult::default(), "{kind:?}");
        }
    }

    #[test]
    fn uncertain_slashes_preserve_heredocs_and_block_scalars() {
        let options = FormatOptions::default();
        for (kind, text) in [
            (
                FileKind::Shell,
                "DIR=/root cat <<EOF /dev/stdin\nliteral # not a comment\nEOF\n",
            ),
            (FileKind::Yaml, "foo/\"bar\": |\n  literal # not a comment\n"),
            (
                FileKind::Shell,
                "DIR=/\"root\" cat <<'EOF'\nliteral # not a comment\nEOF\n",
            ),
            (
                FileKind::Shell,
                "cat <<'EOF' DIR=/\"root\"\nliteral # not a comment\nEOF\n",
            ),
            (
                FileKind::Yaml,
                "/path\"key\": |\n  literal # not a comment\nnext: value\n",
            ),
        ] {
            assert_eq!(format(text, kind, &options), FormatResult::default(), "{text}");
            assert!(check(text, kind, &options).violations.is_empty(), "{text}");
        }
    }

    #[test]
    fn javascript_division_preserves_multiline_template_content() {
        let text = concat!(
            "const value = total / count + `\n",
            "// literal; text must stay unchanged\n",
            "// even when it looks like wrapped\n",
            "// prose.\n",
            "`;\n",
            "const next = 1; // Keep this comment.\n",
        );
        let expected = concat!(
            "const value = total / count + `\n",
            "// literal; text must stay unchanged\n",
            "// even when it looks like wrapped\n",
            "// prose.\n",
            "`;\n",
            "// Keep this comment.\n",
            "const next = 1;\n",
        );
        let options = all_rules();
        assert_eq!(
            format(text, FileKind::JavaScript, &options).fixed_text.as_deref(),
            Some(expected)
        );
        assert_eq!(
            format(expected, FileKind::JavaScript, &options),
            FormatResult::default()
        );
    }

    #[test]
    fn javascript_uncertain_slashes_preserve_remaining_source() {
        let options = FormatOptions::default();
        for opening in [
            "const value = total() / count + `\n",
            "const value = /[[a]--[/]]/v + `\n",
        ] {
            let text = format!(
                "{opening}{}",
                concat!(
                    "// literal; text must stay unchanged\n",
                    "// even when it looks like wrapped\n",
                    "// prose.\n",
                    "`;\n",
                    "const next = 1; // Keep this comment in place when state is uncertain.\n",
                )
            );
            assert_eq!(format(&text, FileKind::JavaScript, &options), FormatResult::default());
            assert!(check(&text, FileKind::JavaScript, &options).violations.is_empty());
        }
    }
}

#[cfg(test)]
mod test_line_range_scoping {
    use super::*;
    use crate::semantic_line_breaks::options::RuleSet;

    /// Three doc comment blocks, each one sentence too long for the width, with code between them.
    const THREE_BLOCKS: &str = "\
/// One sentence in the first block. Another sentence that does not fit.
fn first() {}

/// One sentence in the second block. Another sentence that does not fit.
fn second() {}

/// One sentence in the third block. Another sentence that does not fit.
fn third() {}
";

    /// Options with a width that forces every block of [`THREE_BLOCKS`] to split, and the given selection.
    fn options(selection: &str) -> FormatOptions {
        FormatOptions {
            line_ranges: ranges(selection),
            ..FormatOptions::with_width(50)
        }
    }

    /// The given selection, where empty text means no selection at all.
    fn ranges(selection: &str) -> LineRanges {
        if selection.is_empty() {
            return LineRanges::default();
        }
        selection.parse().expect("the selection should parse")
    }

    /// Lines of the fixed text, or of the input when nothing changed.
    fn fixed_lines(text: &str, options: &FormatOptions) -> Vec<String> {
        let result = format(text, FileKind::Rust, options);
        let fixed = result.fixed_text.unwrap_or_else(|| text.to_string());
        fixed.lines().map(str::to_string).collect()
    }

    #[test]
    fn only_the_selected_block_is_reflowed() {
        let lines = fixed_lines(THREE_BLOCKS, &options("4"));
        assert_eq!(
            lines,
            vec![
                "/// One sentence in the first block. Another sentence that does not fit.",
                "fn first() {}",
                "",
                "/// One sentence in the second block.",
                "/// Another sentence that does not fit.",
                "fn second() {}",
                "",
                "/// One sentence in the third block. Another sentence that does not fit.",
                "fn third() {}",
            ]
        );
    }

    #[test]
    fn only_the_selected_block_is_reported() {
        let violations = check(THREE_BLOCKS, FileKind::Rust, &options("4")).violations;
        assert_eq!(
            violations.iter().map(|violation| violation.line).collect::<Vec<_>>(),
            vec![4]
        );
    }

    #[test]
    fn separate_ranges_each_select_their_own_block() {
        let violations = check(THREE_BLOCKS, FileKind::Rust, &options("1,7")).violations;
        assert_eq!(
            violations.iter().map(|violation| violation.line).collect::<Vec<_>>(),
            vec![1, 7]
        );
    }

    #[test]
    fn an_empty_selection_gives_the_unscoped_result() {
        let scoped = format(THREE_BLOCKS, FileKind::Rust, &options(""));
        let unscoped = format(THREE_BLOCKS, FileKind::Rust, &FormatOptions::with_width(50));
        assert_eq!(scoped.fixed_text, unscoped.fixed_text);
        assert_eq!(scoped.violations, unscoped.violations);
    }

    #[test]
    fn a_selection_matching_no_block_changes_nothing() {
        let result = format(THREE_BLOCKS, FileKind::Rust, &options("2-3"));
        assert_eq!(result.fixed_text, None);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn a_block_overlapping_the_selection_is_reflowed_in_full() {
        let text = "\
/// One sentence in a block spread over two lines. Another
/// sentence that does not fit on one line.
fn only() {}
";
        // The selection names only the second line of the block,
        // so taking the block as a whole has to rewrite the first line and report its violations too.
        let result = format(text, FileKind::Rust, &options("2"));
        assert_eq!(
            result.fixed_text.as_deref(),
            Some(
                "/// One sentence in a block spread over two lines.\n\
                 /// Another sentence that does not fit on one line.\nfn only() {}\n"
            )
        );
        assert!(result.violations.iter().all(|violation| violation.line == 1));
        let unscoped = format(text, FileKind::Rust, &options(""));
        assert_eq!(result.fixed_text, unscoped.fixed_text);
        assert_eq!(result.violations, unscoped.violations);
    }

    #[test]
    fn a_trailing_comment_outside_the_selection_is_left_alone() {
        let text = "fn first() {} // A note.\nfn second() {} // Another note.\n";
        let options = FormatOptions {
            rules: RuleSet::ALL,
            line_ranges: "2".parse().expect("the selection should parse"),
            ..FormatOptions::default()
        };
        let result = format(text, FileKind::Rust, &options);
        assert_eq!(
            result.fixed_text.as_deref(),
            Some("fn first() {} // A note.\n// Another note.\nfn second() {}\n")
        );
        assert_eq!(
            result
                .violations
                .iter()
                .map(|violation| violation.line)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_block_below_a_moved_trailing_comment_keeps_its_place() {
        let text = "\
fn first() {} // A note that moves onto its own line.
/// One sentence in the block. Another sentence that does not fit.
fn second() {}
";
        let options = FormatOptions {
            rules: RuleSet::ALL,
            line_ranges: "1".parse().expect("the selection should parse"),
            ..FormatOptions::with_width(50)
        };
        // The move adds a line, so the block that was on line 2 is on line 3 of the fixed text.
        // It is outside the selection either way, so it has to come through untouched.
        let result = format(text, FileKind::Rust, &options);
        assert_eq!(
            result.fixed_text.as_deref(),
            Some(
                "// A note that moves onto its own line.\nfn first() {}\n\
                 /// One sentence in the block. Another sentence that does not fit.\nfn second() {}\n"
            )
        );
    }

    #[test]
    fn the_fixed_ranges_follow_the_lines_a_reflow_adds() {
        // Lines 1 to 4 hold the first two blocks, and each of them splits into two lines,
        // so the four selected lines are six lines of the fixed text.
        let result = format(THREE_BLOCKS, FileKind::Rust, &options("1-4"));
        assert_eq!(
            result
                .fixed_line_ranges
                .iter()
                .map(|range| (*range.start(), *range.end()))
                .collect::<Vec<_>>(),
            vec![(1, 6)]
        );
    }

    #[test]
    fn the_fixed_ranges_stay_empty_without_a_selection() {
        let result = format(THREE_BLOCKS, FileKind::Rust, &FormatOptions::with_width(50));
        assert!(result.fixed_line_ranges.is_empty());
    }
}

#[cfg(test)]
mod test_shift_ranges {
    use super::*;

    fn replacements(replaced: &[usize], length: usize) -> Vec<Option<(String, String)>> {
        (0..length)
            .map(|index| replaced.contains(&index).then(|| (String::new(), String::new())))
            .collect()
    }

    fn shifted(selection: &str, replaced: &[usize], length: usize) -> Vec<(usize, usize)> {
        let ranges: LineRanges = selection.parse().expect("the selection should parse");
        shift_ranges(&ranges, &replacements(replaced, length))
            .iter()
            .map(|range| (*range.start(), *range.end()))
            .collect()
    }

    #[test]
    fn nothing_moves_without_a_replacement() {
        assert_eq!(shifted("5-10", &[], 20), vec![(5, 10)]);
    }

    #[test]
    fn an_empty_selection_stays_empty() {
        let empty = LineRanges::default();
        assert!(shift_ranges(&empty, &replacements(&[0], 5)).is_empty());
    }

    #[test]
    fn a_replacement_above_the_selection_moves_it_down() {
        assert_eq!(shifted("5-10", &[0], 20), vec![(6, 11)]);
        assert_eq!(shifted("5-10", &[0, 1, 2], 20), vec![(8, 13)]);
    }

    #[test]
    fn a_replacement_below_the_selection_leaves_it_alone() {
        assert_eq!(shifted("5-10", &[14], 20), vec![(5, 10)]);
    }

    #[test]
    fn a_replacement_inside_the_selection_stretches_it() {
        assert_eq!(shifted("5-10", &[6], 20), vec![(5, 11)]);
    }

    #[test]
    fn a_replaced_line_covers_both_lines_it_became() {
        // Line 5 is the zero based index 4, and becomes lines 5 and 6.
        assert_eq!(shifted("5", &[4], 20), vec![(5, 6)]);
        assert_eq!(shifted("1", &[0], 20), vec![(1, 2)]);
    }

    #[test]
    fn a_replacement_on_the_last_line_is_covered() {
        assert_eq!(shifted("20", &[19], 20), vec![(20, 21)]);
    }

    #[test]
    fn several_ranges_each_take_the_replacements_above_them() {
        assert_eq!(shifted("5,15", &[0, 9], 20), vec![(6, 6), (17, 17)]);
    }
}
