//! Comment extraction and trailing comment detection for the semantic line breaks formatter.
//!
//! Finds line comment blocks, block comments, and Python docstrings in source files
//! and hands their content to the Markdown splitter.
//! Also produces the replacement lines that move a comment sharing a line with code above that code.
//! The scanning this needs lives in [`super::scanner`].

use super::docstrings::{Docstring, docstring_contents};
use super::file_kind::{CommentStyle, FileKind};
use super::markdown;
use super::markdown::SkipNotice;
use super::options::FormatOptions;
use super::paragraph::Region;
use super::scanner::{ScanState, scan_line_buffered};
use super::string_syntax::{StringSyntax, string_syntax};
use super::violation::{Violation, ViolationKind};
use crate::{leading_whitespace, starts_with_ignore_case};

/// A comment found after code on the same line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailingComment {
    /// Code part of the line with trailing whitespace removed.
    pub code: String,
    /// Comment text after the marker, trimmed.
    pub text: String,
    /// One based character column of the marker.
    pub column: usize,
}

/// What one pass of the scanner found about every line of a file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineScan {
    /// Whether the line starts inside a multi line string, template, heredoc, or an uncertain region.
    inside_string: Vec<bool>,
    /// Byte offset where a trailing comment starts, for the lines that have one.
    comment_start: Vec<Option<usize>>,
}

impl LineScan {
    /// Whether the given zero based line starts inside a multi line string or an uncertain region.
    #[must_use]
    pub(super) fn starts_inside_string(&self, index: usize) -> bool {
        self.inside_string.get(index).copied().unwrap_or(false)
    }
}

/// Scan every line once, recording what both the region split and the trailing comment fix need.
///
/// Both passes drive the same state machine over the same lines,
/// so running it once and keeping both answers halves the character level work for a file.
///
/// Ambiguous slashes can hide string or block comment boundaries,
/// so a line is reported as inside a string when those boundaries cannot be recovered safely.
#[must_use]
pub fn scan_lines(lines: &[&str], kind: FileKind) -> LineScan {
    let count = lines.len();
    let Some(syntax) = string_syntax(kind) else {
        return LineScan {
            inside_string: vec![false; count],
            comment_start: vec![None; count],
        };
    };
    let mut scan = LineScan {
        inside_string: Vec::with_capacity(count),
        comment_start: Vec::with_capacity(count),
    };
    let mut state = ScanState::Normal;
    let mut characters = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        scan.inside_string.push(matches!(
            state,
            ScanState::InString(_)
                | ScanState::InTripleQuote(_)
                | ScanState::InRawString(_)
                | ScanState::InTemplate
                | ScanState::InBacktickRaw
                | ScanState::InHeredoc(_)
                | ScanState::Uncertain
        ));
        // A shebang is a comment line of its own, so it holds neither code nor the start of a string.
        if index == 0 && line.starts_with("#!") {
            scan.comment_start.push(None);
            continue;
        }
        let result = scan_line_buffered(line, std::mem::take(&mut state), &syntax, &mut characters);
        state = result.state;
        scan.comment_start.push(result.comment_start);
    }
    scan
}

/// Split source lines into verbatim regions and comment paragraphs.
#[must_use]
pub fn split_source_regions(lines: &[&str], kind: FileKind) -> Vec<Region> {
    split_source_regions_with_notices(lines, kind).0
}

/// Split source lines the same way [`split_source_regions`] does, and also return the skip notices.
#[must_use]
pub fn split_source_regions_with_notices(lines: &[&str], kind: FileKind) -> (Vec<Region>, Vec<SkipNotice>) {
    split_source_regions_scanned_with_notices(lines, kind, &scan_lines(lines, kind))
}

/// Split source lines into verbatim regions and comment paragraphs, reusing an existing scan.
#[must_use]
pub fn split_source_regions_scanned(lines: &[&str], kind: FileKind, scan: &LineScan) -> Vec<Region> {
    split_source_regions_scanned_with_notices(lines, kind, scan).0
}

/// Split source lines the same way [`split_source_regions_scanned`] does, and also return the skip notices.
#[must_use]
pub fn split_source_regions_scanned_with_notices(
    lines: &[&str],
    kind: FileKind,
    scan: &LineScan,
) -> (Vec<Region>, Vec<SkipNotice>) {
    let style = kind.comment_style();
    let count = lines.len();
    let inside_string = &scan.inside_string;
    let mut regions = Vec::new();
    let mut notices = Vec::new();
    let mut index = 0;
    while index < count {
        let Some(line) = lines.get(index) else {
            break;
        };
        if inside_string.get(index).copied().unwrap_or(false) {
            regions.push(Region::Verbatim {
                start: index,
                end: index + 1,
            });
            index += 1;
            continue;
        }
        let trimmed = line.trim_start();

        if style.docstrings
            && let Some(end) = docstring_regions(lines, index, &mut regions, &mut notices)
        {
            index = end;
            continue;
        }

        if let Some(block) = style.block
            && trimmed.starts_with(block.open)
        {
            let end = block_comment_regions(
                lines,
                index,
                block.open,
                block.close,
                block.continuation,
                &mut regions,
                &mut notices,
            );
            index = end;
            continue;
        }

        if let Some(marker) = line_marker(trimmed, style.line_markers) {
            let indent = leading_whitespace(line);
            let end = lines
                .iter()
                .enumerate()
                .skip(index)
                .find(|(_, candidate)| {
                    leading_whitespace(candidate) != indent
                        || line_marker(candidate.trim_start(), style.line_markers) != Some(marker)
                })
                .map_or(count, |(position, _)| position);
            let contents: Vec<&str> = lines
                .get(index..end)
                .unwrap_or_default()
                .iter()
                .map(|comment| comment_content(comment.trim_start(), marker))
                .collect();
            let prefix = format!("{indent}{marker} ");
            let (paragraph_regions, paragraph_notices) =
                markdown::split_paragraphs_with_notices(&contents, &prefix, index, false);
            regions.extend(paragraph_regions);
            notices.extend(paragraph_notices);
            index = end;
            continue;
        }

        regions.push(Region::Verbatim {
            start: index,
            end: index + 1,
        });
        index += 1;
    }
    (regions, notices)
}

/// Find trailing comments and build replacement line pairs for each affected line.
///
/// Returns one entry per input line, `Some((comment_line, code_line))` for lines to split,
/// and the violations found.
#[must_use]
pub fn fix_trailing_comments(
    lines: &[&str],
    kind: FileKind,
    options: &FormatOptions,
) -> (Vec<Option<(String, String)>>, Vec<Violation>) {
    fix_trailing_comments_scanned(lines, kind, options, &scan_lines(lines, kind))
}

/// Find the trailing comments and build the replacement lines, reusing an existing scan.
#[must_use]
pub fn fix_trailing_comments_scanned(
    lines: &[&str],
    kind: FileKind,
    options: &FormatOptions,
    scan: &LineScan,
) -> (Vec<Option<(String, String)>>, Vec<Violation>) {
    let mut replacements = vec![None; lines.len()];
    let mut violations = Vec::new();
    let Some(syntax) = string_syntax(kind) else {
        return (replacements, violations);
    };
    let style = kind.comment_style();
    for (index, line) in lines.iter().enumerate() {
        // This pass works line by line, so an unselected line can be passed over exactly
        // rather than by the paragraph the prose pass has to take or leave as a whole.
        if !options.line_ranges.contains_line(index + 1) {
            continue;
        }
        let Some(comment_start) = scan.comment_start.get(index).copied().flatten() else {
            continue;
        };
        let Some(trailing) = trailing_comment(line, comment_start, &syntax) else {
            continue;
        };
        if is_directive(&trailing.text, options) {
            continue;
        }
        // A comment moved above a line that already has a comment above it joins that comment block,
        // and the reflow then merges two separate notes into one sentence.
        // The move is reported so the line can be fixed by hand, but it is not made automatically.
        if has_comment_line_above(lines, index, &style, &scan.inside_string) {
            violations.push(Violation {
                line: index + 1,
                column: Some(trailing.column),
                kind: ViolationKind::TrailingComment,
                message: "comment shares a line with code, a comment already sits above it".into(),
                fixable: false,
            });
            continue;
        }
        let indent = leading_whitespace(line);
        let comment_line = format!("{indent}{} {}", syntax.line_marker, trailing.text);
        violations.push(Violation {
            line: index + 1,
            column: Some(trailing.column),
            kind: ViolationKind::TrailingComment,
            message: "comment shares a line with code, move it to its own line above".into(),
            fixable: true,
        });
        if let Some(slot) = replacements.get_mut(index) {
            *slot = Some((comment_line, trailing.code));
        }
    }
    (replacements, violations)
}

/// The line comment marker the trimmed line starts with, when followed by whitespace or the end of the line.
pub(super) fn line_marker<'a>(trimmed: &str, markers: &[&'a str]) -> Option<&'a str> {
    markers.iter().copied().find(|marker| {
        trimmed
            .strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })
}

/// Whether a line comment already sits directly above the given line.
///
/// A comment moved above such a line would be reflowed together with the comment above it,
/// which merges two separate notes into one sentence, so those lines are reported but not fixed.
fn has_comment_line_above(lines: &[&str], index: usize, style: &CommentStyle, inside_string: &[bool]) -> bool {
    let Some(previous) = index.checked_sub(1) else {
        return false;
    };
    if inside_string.get(previous).copied().unwrap_or(false) {
        return false;
    }
    lines
        .get(previous)
        .is_some_and(|line| line_marker(line.trim_start(), style.line_markers).is_some())
}

/// Split a line at a comment marker byte offset into code and comment text.
fn trailing_comment(line: &str, comment_start: usize, syntax: &StringSyntax) -> Option<TrailingComment> {
    let code = line.get(..comment_start)?.trim_end();
    if code.is_empty() {
        return None;
    }
    let text = line.get(comment_start + syntax.line_marker.len()..)?.trim();
    if text.is_empty() {
        return None;
    }
    let column = line.get(..comment_start)?.chars().count() + 1;
    Some(TrailingComment {
        code: code.to_string(),
        text: text.to_string(),
        column,
    })
}

/// Whether the comment text is a directive for another tool.
fn is_directive(text: &str, options: &FormatOptions) -> bool {
    let normalized = text.trim_start_matches(['!', '/', '#', '-', ' ']);
    options
        .directive_prefixes
        .iter()
        .any(|prefix| starts_with_ignore_case(normalized, prefix))
}

/// Comment content after the marker and a single space.
fn comment_content<'a>(trimmed: &'a str, marker: &str) -> &'a str {
    let rest = trimmed.strip_prefix(marker).unwrap_or(trimmed);
    rest.strip_prefix(' ').unwrap_or(rest)
}

/// Add regions for a block comment starting at `index` and return the index after it.
fn block_comment_regions(
    lines: &[&str],
    index: usize,
    open: &str,
    close: &str,
    continuation: &str,
    regions: &mut Vec<Region>,
    notices: &mut Vec<SkipNotice>,
) -> usize {
    let Some(first) = lines.get(index) else {
        return index + 1;
    };
    let after_open = first.trim_start().get(open.len()..).unwrap_or_default();
    if after_open.contains(close) {
        regions.push(Region::Verbatim {
            start: index,
            end: index + 1,
        });
        return index + 1;
    }
    regions.push(Region::Verbatim {
        start: index,
        end: index + 1,
    });
    let close_index = lines
        .iter()
        .enumerate()
        .skip(index + 1)
        .find(|(_, line)| line.contains(close))
        .map_or(lines.len(), |(position, _)| position);

    let mut inner = index + 1;
    while inner < close_index {
        let Some(line) = lines.get(inner) else {
            break;
        };
        let indent = leading_whitespace(line);
        let has_marker = line.trim_start().starts_with(continuation);
        let group_end = lines
            .iter()
            .enumerate()
            .skip(inner)
            .take_while(|(position, candidate)| {
                *position < close_index
                    && leading_whitespace(candidate) == indent
                    && candidate.trim_start().starts_with(continuation) == has_marker
            })
            .last()
            .map_or(inner + 1, |(position, _)| position + 1);
        let contents: Vec<&str> = lines
            .get(inner..group_end)
            .unwrap_or_default()
            .iter()
            .map(|line| {
                let trimmed = line.trim_start();
                if has_marker {
                    comment_content(trimmed, continuation)
                } else {
                    trimmed
                }
            })
            .collect();
        let prefix = if has_marker {
            format!("{indent}{continuation} ")
        } else {
            indent.to_string()
        };
        let (paragraph_regions, paragraph_notices) =
            markdown::split_paragraphs_with_notices(&contents, &prefix, inner, false);
        regions.extend(paragraph_regions);
        notices.extend(paragraph_notices);
        inner = group_end;
    }
    if close_index < lines.len() {
        regions.push(Region::Verbatim {
            start: close_index,
            end: close_index + 1,
        });
    }
    (close_index + 1).min(lines.len().max(index + 1))
}

/// Add regions for a Python docstring starting at `index`, returning the index after it when one was found.
fn docstring_regions(
    lines: &[&str],
    index: usize,
    regions: &mut Vec<Region>,
    notices: &mut Vec<SkipNotice>,
) -> Option<usize> {
    let docstring = Docstring::at(lines, index)?;
    let indent = docstring.indent;
    let quote = docstring.quote;
    let close_index = docstring.close_line;
    let closing_has_content = docstring.closing_shares_line();

    let contents = docstring_contents(lines, &docstring);
    let content_start = if docstring.opening_shares_line() {
        index
    } else {
        regions.push(Region::Verbatim {
            start: index,
            end: index + 1,
        });
        index + 1
    };

    let (mut inner, inner_notices) = markdown::split_paragraphs_with_notices(&contents, indent, content_start, false);
    for region in &mut inner {
        if let Region::Paragraph(paragraph) = region {
            if paragraph.start_line == index && docstring.opening_shares_line() {
                paragraph.first_prefix = format!("{indent}{}{quote}", docstring.string_prefix);
            }
            if closing_has_content && paragraph.end_line == close_index + 1 {
                paragraph.last_suffix = quote.to_string();
            }
        }
    }
    regions.extend(inner);
    notices.extend(inner_notices);
    if !closing_has_content {
        regions.push(Region::Verbatim {
            start: close_index,
            end: close_index + 1,
        });
    }
    Some(close_index + 1)
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::{FileKind, FormatOptions, Region, Violation, fix_trailing_comments};
    use crate::semantic_line_breaks::paragraph::Paragraph;

    /// Build a verbatim region for the given half open range.
    pub const fn verbatim_region(start: usize, end: usize) -> Region {
        Region::Verbatim { start, end }
    }

    /// The paragraph inside a region, panicking when the region is verbatim.
    pub fn paragraph(region: &Region) -> &Paragraph {
        match region {
            Region::Paragraph(paragraph) => paragraph,
            Region::Verbatim { start, end } => panic!("expected a paragraph, got verbatim {start}..{end}"),
        }
    }

    /// Half open line range covered by a region.
    pub const fn range(region: &Region) -> (usize, usize) {
        match region {
            Region::Verbatim { start, end } => (*start, *end),
            Region::Paragraph(paragraph) => (paragraph.start_line, paragraph.end_line),
        }
    }

    /// Assert that the regions cover `0..count` exactly once and in order.
    pub fn assert_tiles(regions: &[Region], count: usize) {
        let mut expected_start = 0;
        for region in regions {
            let (start, end) = range(region);
            assert_eq!(
                start, expected_start,
                "region {region:?} does not start where the previous one ended"
            );
            assert!(end > start, "region {region:?} is empty");
            expected_start = end;
        }
        assert_eq!(expected_start, count, "regions do not reach the end of the input");
    }

    /// Whether each line starts inside a multi line string.
    pub fn lines_inside_strings(lines: &[&str], kind: FileKind) -> Vec<bool> {
        super::scan_lines(lines, kind).inside_string
    }

    /// Run the trailing comment scanner with default options.
    pub fn fix(lines: &[&str], kind: FileKind) -> (Vec<Option<(String, String)>>, Vec<Violation>) {
        fix_trailing_comments(lines, kind, &FormatOptions::default())
    }

    /// The replacement for a single line, when one was produced.
    pub fn replacement(line: &str, kind: FileKind) -> Option<(String, String)> {
        let (replacements, _) = fix(&[line], kind);
        replacements.into_iter().next().flatten()
    }

    /// A replacement pair built from string literals.
    pub fn pair(comment: &str, code: &str) -> (String, String) {
        (comment.to_string(), code.to_string())
    }

    /// Indices of the lines that received a replacement.
    pub fn replaced_indices(lines: &[&str], kind: FileKind) -> Vec<usize> {
        let (replacements, _) = fix(lines, kind);
        replacements
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|_| index))
            .collect()
    }
}

#[cfg(test)]
mod test_comment_blocks {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn doc_comment_block_forms_one_paragraph() {
        let regions = split_source_regions(&["/// foo", "/// bar", "/// baz"], FileKind::Rust);
        assert_eq!(regions.len(), 1);
        let block = paragraph(&regions[0]);
        assert_eq!(block.first_prefix, "/// ");
        assert_eq!(block.rest_prefix, "/// ");
        assert_eq!(block.lines, vec!["foo", "bar", "baz"]);
        assert_eq!((block.start_line, block.end_line), (0, 3));
    }

    #[test]
    fn blank_doc_comment_line_splits_paragraphs() {
        let regions = split_source_regions(&["/// foo", "///", "/// bar"], FileKind::Rust);
        assert_eq!(regions.len(), 3);
        assert_eq!(paragraph(&regions[0]).lines, vec!["foo"]);
        assert_eq!(regions[1], verbatim_region(1, 2));
        assert_eq!(paragraph(&regions[2]).lines, vec!["bar"]);
    }

    #[test]
    fn line_comment_block_ends_at_code() {
        let regions = split_source_regions(&["// a", "// b", "let x = 1;", "// c"], FileKind::Rust);
        assert_eq!(regions.len(), 3);
        let first = paragraph(&regions[0]);
        assert_eq!(first.first_prefix, "// ");
        assert_eq!(first.lines, vec!["a", "b"]);
        assert_eq!(regions[1], verbatim_region(2, 3));
        assert_eq!(paragraph(&regions[2]).lines, vec!["c"]);
    }

    #[test]
    fn indented_comment_block_keeps_indentation_in_prefix() {
        let lines = ["fn main() {", "    // a", "    // b", "    let x = 1;", "}"];
        let regions = split_source_regions(&lines, FileKind::Rust);
        assert_eq!(regions[0], verbatim_region(0, 1));
        let block = paragraph(&regions[1]);
        assert_eq!(block.first_prefix, "    // ");
        assert_eq!(block.lines, vec!["a", "b"]);
        assert_eq!(regions[2], verbatim_region(3, 4));
        assert_eq!(regions[3], verbatim_region(4, 5));
    }

    #[test]
    fn comment_block_splits_when_indentation_changes() {
        let regions = split_source_regions(&["// a", "    // b"], FileKind::Rust);
        assert_eq!(regions.len(), 2);
        assert_eq!(paragraph(&regions[0]).first_prefix, "// ");
        assert_eq!(paragraph(&regions[1]).first_prefix, "    // ");
    }

    #[test]
    fn marker_without_space_is_verbatim() {
        assert_eq!(
            split_source_regions(&["///Foo"], FileKind::Rust),
            vec![verbatim_region(0, 1)]
        );
        assert_eq!(
            split_source_regions(&["//Foo"], FileKind::Rust),
            vec![verbatim_region(0, 1)]
        );
    }

    #[test]
    fn inner_and_outer_doc_comments_are_separate_paragraphs() {
        let regions = split_source_regions(&["//! crate docs", "/// item docs"], FileKind::Rust);
        assert_eq!(regions.len(), 2);
        assert_eq!(paragraph(&regions[0]).first_prefix, "//! ");
        assert_eq!(paragraph(&regions[0]).lines, vec!["crate docs"]);
        assert_eq!(paragraph(&regions[1]).first_prefix, "/// ");
        assert_eq!(paragraph(&regions[1]).lines, vec!["item docs"]);
    }

    #[test]
    fn heading_inside_doc_comment_is_verbatim() {
        let lines = ["/// Does a thing.", "///", "/// # Errors", "///", "/// Fails when x."];
        let regions = split_source_regions(&lines, FileKind::Rust);
        assert_eq!(regions.len(), 5);
        assert_eq!(paragraph(&regions[0]).lines, vec!["Does a thing."]);
        assert_eq!(regions[1], verbatim_region(1, 2));
        assert_eq!(regions[2], verbatim_region(2, 3));
        assert_eq!(regions[3], verbatim_region(3, 4));
        assert_eq!(paragraph(&regions[4]).lines, vec!["Fails when x."]);
    }

    #[test]
    fn fenced_code_inside_doc_comment_is_verbatim() {
        let lines = ["/// Example:", "/// ```", "/// let x = 1;", "/// ```", "/// Done."];
        let regions = split_source_regions(&lines, FileKind::Rust);
        assert_eq!(regions.len(), 3);
        assert_eq!(paragraph(&regions[0]).lines, vec!["Example:"]);
        assert_eq!(regions[1], verbatim_region(1, 4));
        assert_eq!(paragraph(&regions[2]).lines, vec!["Done."]);
    }

    #[test]
    fn block_comment_has_verbatim_delimiters_and_star_prefixed_paragraph() {
        let regions = split_source_regions(&["/*", " * text here", " * and more", " */"], FileKind::Rust);
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0], verbatim_region(0, 1));
        let body = paragraph(&regions[1]);
        assert_eq!(body.first_prefix, " * ");
        assert_eq!(body.rest_prefix, " * ");
        assert_eq!(body.lines, vec!["text here", "and more"]);
        assert_eq!((body.start_line, body.end_line), (1, 3));
        assert_eq!(regions[2], verbatim_region(3, 4));
    }

    #[test]
    fn block_comment_without_continuation_markers_uses_indent_prefix() {
        let regions = split_source_regions(&["/*", "   plain text", "*/"], FileKind::CLike);
        let body = paragraph(&regions[1]);
        assert_eq!(body.first_prefix, "   ");
        assert_eq!(body.lines, vec!["plain text"]);
        assert_eq!(regions[2], verbatim_region(2, 3));
    }

    #[test]
    fn one_line_block_comment_is_verbatim() {
        let regions = split_source_regions(&["/* x */", "let y = 1;"], FileKind::Rust);
        assert_eq!(regions, vec![verbatim_region(0, 1), verbatim_region(1, 2)]);
    }

    #[test]
    fn regions_tile_a_mixed_rust_file() {
        let lines = [
            "//! Module docs.",
            "",
            "use std::fs;",
            "",
            "/// Does a thing.",
            "///",
            "/// # Errors",
            "///",
            "/// Fails when x.",
            "pub fn thing() {",
            "    // Local comment",
            "    // continues here.",
            "    let x = 1;",
            "    /*",
            "     * Block text.",
            "     */",
            "}",
        ];
        let regions = split_source_regions(&lines, FileKind::Rust);
        assert_tiles(&regions, lines.len());
        let paragraph_count = regions
            .iter()
            .filter(|region| matches!(region, Region::Paragraph(_)))
            .count();
        assert_eq!(paragraph_count, 5);
    }
}

#[cfg(test)]
mod test_docstrings {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn docstring_with_summary_on_opening_line() {
        let lines = ["def f():", "    \"\"\"Summary line.", "    More body.", "    \"\"\""];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0], verbatim_region(0, 1));
        let body = paragraph(&regions[1]);
        assert_eq!(body.first_prefix, "    \"\"\"");
        assert_eq!(body.rest_prefix, "    ");
        assert_eq!(body.last_suffix, "");
        assert_eq!(body.lines, vec!["Summary line.", "More body."]);
        assert_eq!((body.start_line, body.end_line), (1, 3));
        assert_eq!(regions[2], verbatim_region(3, 4));
    }

    #[test]
    fn docstring_with_quotes_on_their_own_lines() {
        let lines = ["    \"\"\"", "    Body text.", "    \"\"\""];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0], verbatim_region(0, 1));
        let body = paragraph(&regions[1]);
        assert_eq!(body.first_prefix, "    ");
        assert_eq!(body.lines, vec!["Body text."]);
        assert_eq!(regions[2], verbatim_region(2, 3));
    }

    #[test]
    fn closing_quotes_on_content_line_become_suffix() {
        let lines = ["    \"\"\"Summary line.", "    more text.\"\"\""];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_eq!(regions.len(), 1);
        let body = paragraph(&regions[0]);
        assert_eq!(body.first_prefix, "    \"\"\"");
        assert_eq!(body.last_suffix, "\"\"\"");
        assert_eq!(body.lines, vec!["Summary line.", "more text."]);
        assert_eq!((body.start_line, body.end_line), (0, 2));
    }

    #[test]
    fn one_line_docstring_is_verbatim() {
        let regions = split_source_regions(&["    \"\"\"Just this.\"\"\""], FileKind::Python);
        assert_eq!(regions, vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn raw_docstring_prefix_is_kept_in_first_prefix() {
        let lines = ["    r\"\"\"Summary.", "    \"\"\""];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_eq!(paragraph(&regions[0]).first_prefix, "    r\"\"\"");
    }

    #[test]
    fn single_quoted_docstring_is_recognized() {
        let lines = ["'''Summary.", "More.", "'''"];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_eq!(paragraph(&regions[0]).first_prefix, "'''");
        assert_eq!(paragraph(&regions[0]).lines, vec!["Summary.", "More."]);
        assert_eq!(regions[1], verbatim_region(2, 3));
    }

    #[test]
    fn deeper_indented_body_lines_after_blank_are_verbatim() {
        let lines = [
            "    \"\"\"Summary.",
            "",
            "    Args:",
            "",
            "        x: The value.",
            "    \"\"\"",
        ];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_tiles(&regions, lines.len());
        assert_eq!(paragraph(&regions[0]).lines, vec!["Summary."]);
        assert_eq!(regions[1], verbatim_region(1, 2));
        assert_eq!(paragraph(&regions[2]).lines, vec!["Args:"]);
        assert_eq!(regions[3], verbatim_region(3, 4));
        assert_eq!(regions[4], verbatim_region(4, 5));
        assert_eq!(regions[5], verbatim_region(5, 6));
    }

    #[test]
    fn deeper_indented_body_lines_directly_after_section_header_are_verbatim() {
        let lines = [
            "    \"\"\"Summary.",
            "",
            "    Args:",
            "        x: The value.",
            "    \"\"\"",
        ];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_tiles(&regions, lines.len());
        assert_eq!(paragraph(&regions[2]).lines, vec!["Args:"]);
        assert_eq!(regions[3], verbatim_region(3, 4));
    }

    #[test]
    fn python_comment_block_is_a_paragraph() {
        let regions = split_source_regions(&["# a", "# b", "x = 1"], FileKind::Python);
        assert_eq!(paragraph(&regions[0]).first_prefix, "# ");
        assert_eq!(paragraph(&regions[0]).lines, vec!["a", "b"]);
        assert_eq!(regions[1], verbatim_region(2, 3));
    }
}

#[cfg(test)]
mod test_comment_above_code {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_comment_directly_above_the_code_blocks_the_move() {
        let lines = [
            "// Various dotted prefixes all below threshold of 15",
            "let x = 1; // 6 chars",
        ];
        let (replacements, violations) = fix(&lines, FileKind::Rust);
        assert!(replacements.iter().all(Option::is_none));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].kind, ViolationKind::TrailingComment);
        assert_eq!(violations[0].line, 2);
        assert!(!violations[0].fixable);
        assert!(violations[0].message.contains("a comment already sits above it"));
    }

    #[test]
    fn a_doc_comment_above_the_code_also_blocks_the_move() {
        let lines = ["/// documented", "let x = 1; // note"];
        let (replacements, violations) = fix(&lines, FileKind::Rust);
        assert!(replacements.iter().all(Option::is_none));
        assert!(!violations[0].fixable);
    }

    #[test]
    fn a_blank_line_above_the_code_still_allows_the_move() {
        let lines = ["// a note", "", "let x = 1; // 6 chars"];
        let (replacements, violations) = fix(&lines, FileKind::Rust);
        assert_eq!(replacements[2], Some(pair("// 6 chars", "let x = 1;")));
        assert!(violations[0].fixable);
    }

    #[test]
    fn code_above_the_code_still_allows_the_move() {
        let lines = ["let y = 2;", "let x = 1; // 6 chars"];
        let (replacements, _) = fix(&lines, FileKind::Rust);
        assert_eq!(replacements[1], Some(pair("// 6 chars", "let x = 1;")));
    }

    #[test]
    fn the_first_line_of_a_file_still_allows_the_move() {
        assert_eq!(
            replacement("let x = 1; // one", FileKind::Rust),
            Some(pair("// one", "let x = 1;"))
        );
    }

    #[test]
    fn a_hash_comment_above_the_code_blocks_the_move() {
        let lines = ["# a note", "value = 1  # 6 chars"];
        let (replacements, violations) = fix(&lines, FileKind::Python);
        assert!(replacements.iter().all(Option::is_none));
        assert!(!violations[0].fixable);
    }

    #[test]
    fn a_comment_inside_a_string_above_the_code_does_not_block_the_move() {
        let lines = ["let text = \"", "// not a comment", "\";", "let x = 1; // note"];
        let (replacements, _) = fix(&lines, FileKind::Rust);
        assert_eq!(replacements[3], Some(pair("// note", "let x = 1;")));
    }
}

#[cfg(test)]
mod test_trailing_rust {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn simple_trailing_comment_is_split_and_reported() {
        let (replacements, violations) = fix(&["let x = 1; // one"], FileKind::Rust);
        assert_eq!(replacements, vec![Some(pair("// one", "let x = 1;"))]);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].line, 1);
        assert_eq!(violations[0].column, Some(12));
        assert_eq!(violations[0].kind, ViolationKind::TrailingComment);
        assert!(violations[0].fixable);
    }

    #[test]
    fn indentation_is_preserved_in_both_lines() {
        assert_eq!(
            replacement("    let x = 1; // one", FileKind::Rust),
            Some(pair("    // one", "    let x = 1;"))
        );
    }

    #[test]
    fn comment_after_string_containing_url_is_detected() {
        assert_eq!(
            replacement("let s = \"http://x\"; // c", FileKind::Rust),
            Some(pair("// c", "let s = \"http://x\";"))
        );
    }

    #[test]
    fn markers_inside_strings_are_ignored() {
        assert_eq!(replacement("let s = \"a // b\";", FileKind::Rust), None);
        assert_eq!(replacement("foo(\"a://b\")", FileKind::Rust), None);
        assert_eq!(replacement("let s = \"a \\\" // b\";", FileKind::Rust), None);
    }

    #[test]
    fn url_in_code_is_not_a_comment() {
        assert_eq!(replacement("let u = http://x;", FileKind::Rust), None);
    }

    #[test]
    fn raw_string_across_lines_hides_markers_until_closed() {
        let lines = ["let s = r#\"first // no", "second // no", "\"#; // c"];
        assert_eq!(replaced_indices(&lines, FileKind::Rust), vec![2]);
        let (replacements, violations) = fix(&lines, FileKind::Rust);
        assert_eq!(replacements[2], Some(pair("// c", "\"#;")));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].line, 3);
    }

    #[test]
    fn plain_string_across_lines_hides_markers() {
        let lines = ["let s = \"first // no", "second\"; // c"];
        assert_eq!(replaced_indices(&lines, FileKind::Rust), vec![1]);
    }

    #[test]
    fn char_literal_and_lifetime_do_not_confuse_scanner() {
        assert_eq!(
            replacement("if c == '/' { } // x", FileKind::Rust),
            Some(pair("// x", "if c == '/' { }"))
        );
        assert_eq!(
            replacement("fn f<'a>(x: &'a str) {} // x", FileKind::Rust),
            Some(pair("// x", "fn f<'a>(x: &'a str) {}"))
        );
        assert_eq!(
            replacement("let q = '\\''; // x", FileKind::Rust),
            Some(pair("// x", "let q = '\\'';"))
        );
    }

    #[test]
    fn directives_are_left_alone() {
        assert_eq!(replacement("x(); // clippy::foo", FileKind::Rust), None);
        assert_eq!(replacement("x(); // SAFETY: fine", FileKind::Rust), None);
        assert_eq!(replacement("x(); // noqa", FileKind::Rust), None);
        assert_eq!(replacement("x(); // rustfmt::skip", FileKind::Rust), None);
        let (_, violations) = fix(&["x(); // clippy::foo"], FileKind::Rust);
        assert!(violations.is_empty());
    }

    #[test]
    fn pure_comment_lines_produce_no_replacement() {
        assert_eq!(replacement("// only a comment", FileKind::Rust), None);
        assert_eq!(replacement("    /// doc comment", FileKind::Rust), None);
        assert_eq!(replacement("let x = 1; //", FileKind::Rust), None);
    }

    #[test]
    fn closing_brace_with_comment_is_detected() {
        assert_eq!(replacement("} // end", FileKind::Rust), Some(pair("// end", "}")));
    }

    #[test]
    fn block_comment_before_line_comment_is_skipped() {
        assert_eq!(
            replacement("let x = 1; /* a */ // c", FileKind::Rust),
            Some(pair("// c", "let x = 1; /* a */"))
        );
        assert_eq!(replacement("let x = 1; /* // not */", FileKind::Rust), None);
    }

    #[test]
    fn nested_block_comment_across_lines_hides_markers() {
        let lines = ["/* outer /* inner", "*/ still // no", "*/ let x = 1; // c"];
        assert_eq!(replaced_indices(&lines, FileKind::Rust), vec![2]);
    }

    #[test]
    fn one_entry_per_input_line() {
        let lines = ["let a = 1; // a", "let b = 2;", "let c = 3; // c"];
        let (replacements, violations) = fix(&lines, FileKind::Rust);
        assert_eq!(replacements.len(), 3);
        assert!(replacements[0].is_some());
        assert!(replacements[1].is_none());
        assert!(replacements[2].is_some());
        assert_eq!(
            violations.iter().map(|violation| violation.line).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }
}

#[cfg(test)]
mod test_trailing_python {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn hash_inside_string_is_ignored_and_comment_after_it_detected() {
        assert_eq!(
            replacement("s = \"#nope\"  # yes", FileKind::Python),
            Some(pair("# yes", "s = \"#nope\""))
        );
        assert_eq!(
            replacement("s = '#nope'  # yes", FileKind::Python),
            Some(pair("# yes", "s = '#nope'"))
        );
        assert_eq!(replacement("s = \"#nope\"", FileKind::Python), None);
    }

    #[test]
    fn triple_quoted_string_across_lines_hides_hashes() {
        let lines = ["x = \"\"\"", "# not a comment", "\"\"\"", "y = 1  # c"];
        assert_eq!(replaced_indices(&lines, FileKind::Python), vec![3]);
        let single = ["x = '''", "# not a comment", "'''", "y = 1  # c"];
        assert_eq!(replaced_indices(&single, FileKind::Python), vec![3]);
    }

    #[test]
    fn type_ignore_directive_is_skipped() {
        assert_eq!(replacement("x = 1  # type: ignore", FileKind::Python), None);
        assert_eq!(replacement("x = 1  # noqa: E501", FileKind::Python), None);
        assert_eq!(replacement("x = 1  # pragma: no cover", FileKind::Python), None);
    }

    #[test]
    fn shebang_first_line_is_skipped() {
        let lines = ["#!/usr/bin/env python3", "x = 1  # c"];
        assert_eq!(replaced_indices(&lines, FileKind::Python), vec![1]);
    }

    #[test]
    fn violation_column_counts_characters() {
        let (_, violations) = fix(&["ä = 1  # c"], FileKind::Python);
        assert_eq!(violations[0].column, Some(8));
    }
}

#[cfg(test)]
mod test_trailing_other_languages {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn shell_hash_needs_leading_space() {
        assert_eq!(
            replacement("echo $# # count", FileKind::Shell),
            Some(pair("# count", "echo $#"))
        );
        assert_eq!(replacement("echo $#", FileKind::Shell), None);
        assert_eq!(replacement("echo ${#arr[@]}", FileKind::Shell), None);
        assert_eq!(replacement("x=1 # c", FileKind::Shell), Some(pair("# c", "x=1")));
    }

    #[test]
    fn shell_heredoc_hides_hashes_until_terminator() {
        let lines = ["cat <<EOF", "# not a comment", "EOF", "x=1 # c"];
        assert_eq!(replaced_indices(&lines, FileKind::Shell), vec![3]);
        let quoted = ["cat <<'END'", "# not a comment", "END", "x=1 # c"];
        assert_eq!(replaced_indices(&quoted, FileKind::Shell), vec![3]);
    }

    #[test]
    fn shell_shebang_is_skipped() {
        let lines = ["#!/bin/bash", "x=1 # c"];
        let (replacements, violations) = fix(&lines, FileKind::Shell);
        assert_eq!(replacements[0], None);
        assert_eq!(replacements[1], Some(pair("# c", "x=1")));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].line, 2);
    }

    #[test]
    fn shell_single_quotes_have_no_escapes() {
        assert_eq!(
            replacement("echo 'a\\' # c", FileKind::Shell),
            Some(pair("# c", "echo 'a\\'"))
        );
    }

    #[test]
    fn yaml_fragment_in_url_is_not_a_comment() {
        assert_eq!(replacement("url: http://x#frag", FileKind::Yaml), None);
        assert_eq!(
            replacement("key: value # c", FileKind::Yaml),
            Some(pair("# c", "key: value"))
        );
    }

    #[test]
    fn yaml_block_scalar_hides_hashes_in_indented_lines() {
        let lines = ["key: |", "  # inside", "  more # inside", "other: 1 # c"];
        assert_eq!(replaced_indices(&lines, FileKind::Yaml), vec![3]);
        let folded = ["key: >-", "  # inside", "", "  still # inside", "other: 1 # c"];
        assert_eq!(replaced_indices(&folded, FileKind::Yaml), vec![4]);
    }

    #[test]
    fn yaml_doubled_quote_escape_is_understood() {
        assert_eq!(
            replacement("key: 'it''s # not' # c", FileKind::Yaml),
            Some(pair("# c", "key: 'it''s # not'"))
        );
    }

    #[test]
    fn toml_literal_string_keeps_backslash_and_hash() {
        let (replacements, violations) = fix(&["p = 'C:\\#x' # c"], FileKind::Toml);
        assert_eq!(replacements, vec![Some(pair("# c", "p = 'C:\\#x'"))]);
        assert_eq!(violations[0].column, Some(13));
    }

    #[test]
    fn toml_multi_line_string_hides_hashes() {
        let lines = ["s = \"\"\"", "# inside", "\"\"\"", "t = 1 # c"];
        assert_eq!(replaced_indices(&lines, FileKind::Toml), vec![3]);
    }

    #[test]
    fn javascript_template_literal_hides_slashes() {
        assert_eq!(
            replacement("const t = `a ${a} // not`; // c", FileKind::JavaScript),
            Some(pair("// c", "const t = `a ${a} // not`;"))
        );
        let lines = ["const t = `line", "${a} // not a comment", "end`; // c"];
        assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![2]);
    }

    #[test]
    fn javascript_single_quoted_strings_are_strings() {
        assert_eq!(
            replacement("const s = 'a // b'; // c", FileKind::JavaScript),
            Some(pair("// c", "const s = 'a // b';"))
        );
    }

    #[test]
    fn regex_escapes_do_not_hide_real_comments() {
        assert_eq!(
            replacement("const r = /a\\/b/; // c", FileKind::JavaScript),
            Some(pair("// c", "const r = /a\\/b/;"))
        );
        assert_eq!(
            replacement("const d = a / b; // c", FileKind::JavaScript),
            Some(pair("// c", "const d = a / b;"))
        );
        assert_eq!(
            replacement("const r = /a\\/\\/b/; // c", FileKind::JavaScript),
            Some(pair("// c", "const r = /a\\/\\/b/;"))
        );
    }

    #[test]
    fn go_backtick_raw_string_across_lines_hides_markers() {
        let lines = ["const s = `start", "// inside", "end` // c"];
        assert_eq!(replaced_indices(&lines, FileKind::Go), vec![2]);
        let (replacements, _) = fix(&lines, FileKind::Go);
        assert_eq!(replacements[2], Some(pair("// c", "end`")));
    }

    #[test]
    fn c_like_detects_trailing_comment() {
        assert_eq!(
            replacement("int x = 1; // c", FileKind::CLike),
            Some(pair("// c", "int x = 1;"))
        );
        assert_eq!(
            replacement("char c = '/'; // c", FileKind::CLike),
            Some(pair("// c", "char c = '/';"))
        );
    }

    #[test]
    fn unsupported_kinds_produce_no_replacements() {
        for kind in [
            FileKind::Dockerfile,
            FileKind::Makefile,
            FileKind::Ruby,
            FileKind::Markdown,
        ] {
            let (replacements, violations) = fix(&["RUN echo hi # c", "x = 1 # c"], kind);
            assert_eq!(replacements, vec![None, None], "{kind:?}");
            assert!(violations.is_empty(), "{kind:?}");
        }
    }
}

#[cfg(test)]
mod test_strings_are_not_comments {
    use super::*;
    use crate::semantic_line_breaks::paragraph::Paragraph;

    #[test]
    fn comment_like_lines_inside_raw_string_are_verbatim() {
        let lines = [
            "const SOURCE: &str = r\"//! Module docs that were hard wrapped",
            "//! at eighty columns by an editor",
            "//! for no good reason.",
            "\";",
            "/// real doc that was wrapped",
            "/// mid clause.",
            "fn f() {}",
        ];
        let regions = split_source_regions(&lines, FileKind::Rust);
        let paragraphs: Vec<&Paragraph> = regions
            .iter()
            .filter_map(|region| match region {
                Region::Paragraph(paragraph) => Some(paragraph),
                Region::Verbatim { .. } => None,
            })
            .collect();
        assert_eq!(paragraphs.len(), 1);
        assert_eq!(paragraphs[0].start_line, 4);
        assert_eq!(paragraphs[0].end_line, 6);
    }

    #[test]
    fn hash_lines_inside_python_triple_quoted_string_are_verbatim() {
        let lines = [
            "query = \"\"\"",
            "# not a comment",
            "# still not",
            "\"\"\"",
            "# real comment",
        ];
        let regions = split_source_regions(&lines, FileKind::Python);
        let paragraph_starts: Vec<usize> = regions
            .iter()
            .filter_map(|region| match region {
                Region::Paragraph(paragraph) => Some(paragraph.start_line),
                Region::Verbatim { .. } => None,
            })
            .collect();
        assert_eq!(paragraph_starts, vec![4]);
    }
}
