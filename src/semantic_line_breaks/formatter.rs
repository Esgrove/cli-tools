//! Formatting pipeline for the semantic line breaks formatter.
//!
//! Splits a text buffer into lines while preserving line endings,
//! runs the trailing comment pass and the prose pass,
//! and reassembles the result byte for byte where nothing changed.

use std::borrow::Cow;

use super::comments;
use super::markdown;
use super::prose;
use super::types::{FileKind, FormatOptions, FormatResult, Region, Violation};

/// One line of the input with its original line ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceLine<'a> {
    /// Line text without the line ending.
    text: &'a str,
    /// The line ending, empty for the last line of a file without a final newline.
    eol: &'a str,
}

/// Check a text buffer and return the violations without producing fixed text.
#[must_use]
pub fn check(text: &str, kind: FileKind, options: &FormatOptions) -> Vec<Violation> {
    run(text, kind, options, false).violations
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
    let mut working: Vec<(Cow<'_, str>, &str)> = Vec::with_capacity(source.len());
    let mut trailing_changed = false;
    if kind.supports_trailing_comment_check() && options.rules.trailing_comment {
        let (replacements, trailing_violations) = comments::fix_trailing_comments(&texts, kind, options);
        violations.extend(trailing_violations);
        for (line, replacement) in source.iter().zip(replacements) {
            match replacement {
                Some((comment, code)) => {
                    working.push((Cow::Owned(comment), line.eol));
                    working.push((Cow::Owned(code), line.eol));
                    trailing_changed = true;
                }
                None => working.push((Cow::Borrowed(line.text), line.eol)),
            }
        }
    } else {
        working.extend(source.iter().map(|line| (Cow::Borrowed(line.text), line.eol)));
    }

    let working_texts: Vec<&str> = working.iter().map(|(text, _)| text.as_ref()).collect();
    let (output, prose_violations) = if trailing_changed {
        let original_violations = prose_pass(&texts, kind, options).1;
        let output = produce_fix.then(|| prose_pass(&working_texts, kind, options).0);
        (output, original_violations)
    } else {
        let (output, prose_violations) = prose_pass(&working_texts, kind, options);
        (produce_fix.then_some(output), prose_violations)
    };
    violations.extend(prose_violations);
    violations.sort_by_key(|violation| (violation.line, violation.kind));

    let fixed_text = output.and_then(|output| {
        let fixed = assemble(&output, &working, text);
        (fixed != text).then_some(fixed)
    });
    FormatResult { violations, fixed_text }
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

/// Reflow every prose paragraph in the lines and return the new lines and the violations found.
fn prose_pass<'a>(lines: &[&'a str], kind: FileKind, options: &FormatOptions) -> (Vec<Cow<'a, str>>, Vec<Violation>) {
    let regions = if kind == FileKind::Markdown {
        markdown::split_paragraphs(lines, "", 0, true)
    } else {
        comments::split_source_regions(lines, kind)
    };
    let mut output: Vec<Cow<'a, str>> = Vec::with_capacity(lines.len());
    let mut violations = Vec::new();
    let mut cursor = 0;
    for region in regions {
        match region {
            Region::Verbatim { end, .. } => {
                copy_lines(lines, cursor, end, &mut output);
                cursor = cursor.max(end);
            }
            Region::Paragraph(paragraph) => {
                copy_lines(lines, cursor, paragraph.start_line, &mut output);
                let outcome = prose::reflow_paragraph(&paragraph, options);
                violations.extend(outcome.violations);
                match outcome.lines {
                    Some(new_lines) => {
                        let last_index = new_lines.len().saturating_sub(1);
                        for (index, content) in new_lines.iter().enumerate() {
                            let suffix = if index == last_index {
                                paragraph.last_suffix.as_str()
                            } else {
                                ""
                            };
                            output.push(Cow::Owned(format!("{}{content}{suffix}", paragraph.prefix_for(index))));
                        }
                    }
                    None => copy_lines(lines, paragraph.start_line, paragraph.end_line, &mut output),
                }
                cursor = cursor.max(paragraph.end_line);
            }
        }
    }
    copy_lines(lines, cursor, lines.len(), &mut output);
    (output, violations)
}

/// Copy the lines in `from..to` into the output.
fn copy_lines<'a>(lines: &[&'a str], from: usize, to: usize, output: &mut Vec<Cow<'a, str>>) {
    if from >= to {
        return;
    }
    output.extend(
        lines
            .get(from..to)
            .unwrap_or_default()
            .iter()
            .copied()
            .map(Cow::Borrowed),
    );
}

/// Join output lines with line endings taken from the working lines.
///
/// When the line counts differ, the dominant line ending of the original text is used.
fn assemble(output: &[Cow<'_, str>], working: &[(Cow<'_, str>, &str)], original: &str) -> String {
    let default_eol = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_newline = original.ends_with('\n') || original.is_empty();
    let mut result = String::with_capacity(original.len() + 64);
    let last_index = output.len().saturating_sub(1);
    for (index, line) in output.iter().enumerate() {
        result.push_str(line.as_ref());
        if index == last_index {
            if trailing_newline && !output.is_empty() {
                let eol = working.get(index).map_or(default_eol, |(_, eol)| eol);
                result.push_str(if eol.is_empty() { default_eol } else { eol });
            }
        } else {
            let eol = if output.len() == working.len() {
                working.get(index).map_or(default_eol, |(_, eol)| eol)
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
    use crate::semantic_line_breaks::types::ViolationKind;

    #[test]
    fn check_matches_format_violations() {
        let text = "/// the quick brown fox\n/// jumps over; it is fast — really.\nfn f() {}\n";
        let options = FormatOptions::default();
        assert_eq!(
            check(text, FileKind::Rust, &options),
            format(text, FileKind::Rust, &options).violations
        );
    }

    #[test]
    fn reflows_rust_doc_comment_and_keeps_code() {
        let text = "/// the quick brown fox\n/// jumps over; it is fast — really.\nfn f() {}\n";
        let result = format(text, FileKind::Rust, &FormatOptions::default());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some("/// the quick brown fox jumps over. It is fast, really.\nfn f() {}\n")
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
        let result = format(text, FileKind::Rust, &FormatOptions::default());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some("fn f() {\r\n    // one\r\n    let x = 1;\r\n}\r\n")
        );
        assert_eq!(result.violations[0].kind, ViolationKind::TrailingComment);
        assert_eq!(result.violations[0].line, 2);
    }

    #[test]
    fn ignore_file_marker_disables_formatting() {
        let text = "<!-- slb-ignore-file -->\nthe quick brown fox\njumps over the dog.\n";
        let result = format(text, FileKind::Markdown, &FormatOptions::default());
        assert_eq!(result, FormatResult::default());
    }

    #[test]
    fn python_docstring_keeps_quotes_in_place() {
        let text = "def f():\n    \"\"\"Parse the header\n    of the file and return it.\n\n    Args:\n        x: the input\n    \"\"\"\n    return 1\n";
        let result = format(text, FileKind::Python, &FormatOptions::default());
        assert_eq!(
            result.fixed_text.as_deref(),
            Some(
                "def f():\n    \"\"\"Parse the header of the file and return it.\n\n    Args:\n        x: the input\n    \"\"\"\n    return 1\n"
            )
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
}
