//! Markdown aware paragraph splitting for the semantic line breaks formatter.
//!
//! Splits a run of content lines into prose paragraphs that may be reflowed
//! and verbatim regions that must never change,
//! such as fenced code, headings, tables, list markers, and lines that look like code.
//! The same splitter is used for Markdown documents and for the content of comment blocks.

use std::sync::LazyLock;

use regex::Regex;

use super::types::{HardBreak, Paragraph, Region};

/// Marker text that excludes the surrounding paragraph from formatting.
pub const IGNORE_MARKER: &str = "slb-ignore";

/// Marker text in the first lines of a file that excludes the whole file from formatting.
pub const IGNORE_FILE_MARKER: &str = "slb-ignore-file";

/// Number of leading lines searched for the file ignore marker.
pub const IGNORE_FILE_SEARCH_LINES: usize = 5;

/// Indentation in spaces that turns a line into an indented code block.
const CODE_INDENT: usize = 4;

/// Matches the opening of a fenced code block.
static RE_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}(`{3,}|~{3,})").expect("Invalid fence regex"));

/// Matches an ATX heading.
static RE_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}#{1,6}(?:\s|$)").expect("Invalid heading regex"));

/// Matches a horizontal rule or a decorative divider line.
static RE_RULE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:[-*_=~#/+](?:\s*[-*_=~#/+]){2,}|[-=*#/+]{3,}.*[-=*#/+]{3,})\s*$").expect("Invalid rule regex")
});

/// Matches a setext heading underline.
static RE_SETEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}(?:=+|-+)\s*$").expect("Invalid setext regex"));

/// Matches a table delimiter row such as `| --- | :---: |`.
static RE_TABLE_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*)*\|?\s*$").expect("Invalid table separator regex")
});

/// Matches the start of an HTML block.
static RE_HTML_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}<(?:[A-Za-z][\w-]*|/[A-Za-z][\w-]*|!)").expect("Invalid HTML regex"));

/// Matches a link reference definition.
static RE_LINK_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}\[[^\]]+\]:\s").expect("Invalid link reference regex"));

/// Matches a line that consists only of links, images, badges, or URLs.
static RE_STANDALONE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    const PART: &str = r"(?:\[!\[[^\]]*\]\([^)]*\)\]\([^)]*\)|!?\[[^\]]*\]\([^)]*\)|!?\[[^\]]*\]\[[^\]]*\]|<?[A-Za-z][A-Za-z0-9+.-]*://\S+>?)";
    Regex::new(&format!(r"^\s*{PART}(?:\s+{PART})*\s*$")).expect("Invalid standalone link regex")
});

/// Matches a list item marker and captures the indentation, marker, spacing, and optional checkbox.
static RE_LIST_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\s*)([-*+]|\d{1,9}[.)])(\s+)(\[[ xX]\]\s+)?(\S.*)?$").expect("Invalid list item regex")
});

/// Matches a blockquote prefix.
static RE_BLOCKQUOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}>\s?").expect("Invalid blockquote regex"));

/// Matches a reST field or a doc tag such as `:param x:` or `@param x`.
static RE_FIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?::\w[^:]*:|@\w+)(?:\s|$)").expect("Invalid field regex"));

/// Matches a backtick code span for removal before code detection.
static RE_CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`]*`").expect("Invalid code span regex"));

/// Matches a call expression that fills the whole line.
static RE_CALL_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.:]+\(.*\)\s*[;:,]?$").expect("Invalid call regex"));

/// Matches a line that starts with a code keyword.
static RE_CODE_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:let|fn|use|impl|pub|mod|struct|enum|import|from|def|class|const|static|return|println!|eprintln!|assert(?:_eq)?!|dbg!|print|self\.|\$ |#\[|#!|//|#)\b",
    )
    .expect("Invalid keyword regex")
});

/// Matches two or more runs of aligned column spacing.
static RE_ALIGNED_COLUMNS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\S\s{3,}\S.*\S\s{3,}\S").expect("Invalid aligned columns regex"));

/// Classification of one content line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineClass {
    /// Empty or whitespace only.
    Blank,
    /// Opens a fenced code block with the given marker character and length.
    FenceOpen(char, usize),
    /// Opens a math block.
    MathFence,
    /// Opens an HTML comment.
    HtmlComment,
    /// Starts an HTML block.
    HtmlBlock,
    /// Single verbatim line such as a heading, rule, table row, or link reference.
    Verbatim,
    /// Blockquote line.
    Blockquote,
    /// List item with the content starting at the given character column.
    ListItem(usize),
    /// A field or tag line that always starts a new paragraph.
    Field,
    /// Ordinary prose.
    Text,
}

/// Split content lines into verbatim regions and prose paragraphs.
///
/// `base_prefix` is prepended to every paragraph prefix, for example the comment marker.
/// `start_line` is the zero based index of the first line in the text buffer.
/// `is_document` enables front matter detection and hard break markers.
#[must_use]
pub fn split_paragraphs(lines: &[&str], base_prefix: &str, start_line: usize, is_document: bool) -> Vec<Region> {
    let mut regions = Vec::new();
    let count = lines.len();
    let mut index = 0;

    if is_document && lines.first().is_some_and(|line| line.trim_end() == "---") {
        let close = lines
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, line)| matches!(line.trim_end(), "---" | "..."))
            .map(|(position, _)| position);
        if let Some(close) = close {
            regions.push(verbatim(start_line, 0, close + 1));
            index = close + 1;
        }
    }

    let mut ignore_next = false;
    while index < count {
        let Some(line) = lines.get(index) else {
            break;
        };
        let next = lines.get(index + 1).copied();
        let class = classify(line, next);
        if line.contains(IGNORE_MARKER) && !matches!(class, LineClass::Text | LineClass::Field | LineClass::ListItem(_))
        {
            ignore_next = true;
        }
        let end = match class {
            LineClass::Blank | LineClass::Verbatim => index + 1,
            LineClass::FenceOpen(marker, length) => find_fence_close(lines, index, marker, length),
            LineClass::MathFence => find_line_containing(lines, index + 1, "$$").map_or(count, |close| close + 1),
            LineClass::HtmlComment => {
                if line.contains("-->") {
                    index + 1
                } else {
                    find_line_containing(lines, index + 1, "-->").map_or(count, |close| close + 1)
                }
            }
            LineClass::HtmlBlock => lines
                .iter()
                .enumerate()
                .skip(index + 1)
                .find(|(_, line)| line.trim().is_empty())
                .map_or(count, |(position, _)| position),
            LineClass::Blockquote => {
                let end = lines
                    .iter()
                    .enumerate()
                    .skip(index)
                    .find(|(_, line)| classify(line, None) != LineClass::Blockquote)
                    .map_or(count, |(position, _)| position);
                let inner: Vec<String> = lines
                    .get(index..end)
                    .unwrap_or_default()
                    .iter()
                    .map(|line| RE_BLOCKQUOTE.replace(line, "").into_owned())
                    .collect();
                let inner_refs: Vec<&str> = inner.iter().map(String::as_str).collect();
                let prefix = format!("{base_prefix}> ");
                regions.extend(split_paragraphs(&inner_refs, &prefix, start_line + index, false));
                index = end;
                continue;
            }
            LineClass::ListItem(content_start) => {
                let end = collect_list_item(lines, index, content_start);
                let region =
                    list_item_paragraph(lines, index, end, content_start, base_prefix, start_line, is_document);
                regions.push(apply_ignore(region, &mut ignore_next, start_line, index, end));
                index = end;
                continue;
            }
            LineClass::Field | LineClass::Text => {
                if leading_width(line) >= CODE_INDENT {
                    index + 1
                } else {
                    let (end, is_setext) = collect_text(lines, index);
                    let region = if is_setext {
                        verbatim(start_line, index, end)
                    } else {
                        text_paragraph(lines, index, end, base_prefix, start_line, is_document)
                    };
                    regions.push(apply_ignore(region, &mut ignore_next, start_line, index, end));
                    index = end;
                    continue;
                }
            }
        };
        regions.push(verbatim(start_line, index, end));
        index = end.max(index + 1);
    }
    regions
}

/// Whether the file opts out of formatting with a marker in its first lines.
#[must_use]
pub fn has_ignore_file_marker(lines: &[&str]) -> bool {
    lines
        .iter()
        .take(IGNORE_FILE_SEARCH_LINES)
        .any(|line| line.contains(IGNORE_FILE_MARKER))
}

/// Whether a content line looks like source code rather than prose.
#[must_use]
pub fn looks_like_code(line: &str) -> bool {
    let stripped = RE_CODE_SPAN.replace_all(line, "");
    let text = stripped.trim();
    if text.is_empty() {
        return false;
    }
    if text.ends_with(['{', '}']) || text.ends_with(");") || text.ends_with("),") {
        return true;
    }
    let has_code_characters = text.contains(['=', '(', '{']) || text.contains("::");
    if text.ends_with(';') && has_code_characters {
        return true;
    }
    if RE_CODE_KEYWORD.is_match(text)
        && (has_code_characters || text.ends_with(';') || text.starts_with(['#', '/', '$']))
    {
        return true;
    }
    if text.contains("::")
        || text.contains(" = ")
        || text.contains(" == ")
        || text.contains("->")
        || text.contains("=>")
        || text.contains("&&")
        || text.contains("||")
    {
        return true;
    }
    RE_CALL_LINE.is_match(text)
}

/// Build the paragraph region for a list item spanning `index..end`.
fn list_item_paragraph(
    lines: &[&str],
    index: usize,
    end: usize,
    content_start: usize,
    base_prefix: &str,
    start_line: usize,
    is_document: bool,
) -> Region {
    let item_lines = lines.get(index..end).unwrap_or_default();
    let first_line = item_lines.first().copied().unwrap_or_default();
    let first_prefix = format!("{base_prefix}{}", take_chars(first_line, content_start));
    let rest_prefix = format!("{base_prefix}{}", " ".repeat(content_start));
    let contents: Vec<&str> = item_lines
        .iter()
        .enumerate()
        .map(|(offset, item_line)| {
            if offset == 0 {
                skip_chars(item_line, content_start)
            } else {
                strip_indent(item_line, content_start)
            }
        })
        .collect();
    make_paragraph(
        &contents,
        first_prefix,
        rest_prefix,
        start_line + index,
        start_line + end,
        is_document,
    )
}

/// Build the paragraph region for plain text lines spanning `index..end`.
fn text_paragraph(
    lines: &[&str],
    index: usize,
    end: usize,
    base_prefix: &str,
    start_line: usize,
    is_document: bool,
) -> Region {
    let text_lines = lines.get(index..end).unwrap_or_default();
    let first_line = text_lines.first().copied().unwrap_or_default();
    let first_indent = leading_whitespace(first_line);
    let rest_indent = text_lines
        .get(1)
        .map_or(first_indent, |second| leading_whitespace(second));
    let contents: Vec<&str> = text_lines.iter().map(|text| text.trim_start()).collect();
    make_paragraph(
        &contents,
        format!("{base_prefix}{first_indent}"),
        format!("{base_prefix}{rest_indent}"),
        start_line + index,
        start_line + end,
        is_document,
    )
}

/// Turn the region verbatim when an ignore marker preceded it, and clear the marker.
fn apply_ignore(region: Region, ignore_next: &mut bool, start_line: usize, from: usize, to: usize) -> Region {
    if *ignore_next {
        *ignore_next = false;
        verbatim(start_line, from, to)
    } else {
        region
    }
}

/// Create a verbatim region for the given local line range.
const fn verbatim(start_line: usize, from: usize, to: usize) -> Region {
    Region::Verbatim {
        start: start_line + from,
        end: start_line + to,
    }
}

/// Classify one line, looking at the following line for table detection.
fn classify(line: &str, next: Option<&str>) -> LineClass {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LineClass::Blank;
    }
    if let Some(captures) = RE_FENCE.captures(line)
        && let Some(fence) = captures.get(1)
    {
        let marker = fence.as_str().chars().next().unwrap_or('`');
        return LineClass::FenceOpen(marker, fence.as_str().chars().count());
    }
    if trimmed.starts_with("$$") {
        return LineClass::MathFence;
    }
    if trimmed.starts_with("<!--") {
        return LineClass::HtmlComment;
    }
    if RE_HEADING.is_match(line) || RE_RULE.is_match(line) || RE_LINK_REFERENCE.is_match(line) {
        return LineClass::Verbatim;
    }
    if trimmed.starts_with('|') || next.is_some_and(|next| line.contains('|') && RE_TABLE_SEPARATOR.is_match(next)) {
        return LineClass::Verbatim;
    }
    if RE_TABLE_SEPARATOR.is_match(line) && line.contains('|') {
        return LineClass::Verbatim;
    }
    if RE_HTML_BLOCK.is_match(line) {
        return LineClass::HtmlBlock;
    }
    if RE_BLOCKQUOTE.is_match(line) {
        return LineClass::Blockquote;
    }
    if let Some(captures) = RE_LIST_ITEM.captures(line) {
        if captures.get(5).is_none() {
            return LineClass::Verbatim;
        }
        let content_start = captures.get(5).map_or(0, |content| {
            line.get(..content.start()).unwrap_or_default().chars().count()
        });
        return LineClass::ListItem(content_start);
    }
    if RE_STANDALONE_LINK.is_match(line) {
        return LineClass::Verbatim;
    }
    if RE_FIELD.is_match(line) {
        return LineClass::Field;
    }
    LineClass::Text
}

/// Index one past the closing fence, or the end of the lines when unterminated.
fn find_fence_close(lines: &[&str], open_index: usize, marker: char, length: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(open_index + 1)
        .find(|(_, line)| {
            let trimmed = line.trim();
            trimmed.chars().all(|character| character == marker) && trimmed.chars().count() >= length
        })
        .map_or(lines.len(), |(position, _)| position + 1)
}

/// Index of the first line at or after `from` that contains `needle`.
fn find_line_containing(lines: &[&str], from: usize, needle: &str) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, line)| line.contains(needle))
        .map(|(position, _)| position)
}

/// End index of a list item paragraph starting at `index`.
fn collect_list_item(lines: &[&str], index: usize, content_start: usize) -> usize {
    let mut end = index + 1;
    while let Some(line) = lines.get(end) {
        let class = classify(line, lines.get(end + 1).copied());
        if !matches!(class, LineClass::Text) || leading_width(line) >= content_start + CODE_INDENT {
            break;
        }
        end += 1;
    }
    end
}

/// End index of a text paragraph starting at `index`, and whether it turned out to be a setext heading.
fn collect_text(lines: &[&str], index: usize) -> (usize, bool) {
    let first_line = lines.get(index).copied().unwrap_or_default();
    let is_section_header = first_line.trim_end().ends_with(':');
    let base_indent = leading_width(first_line);
    let mut end = index + 1;
    while let Some(line) = lines.get(end) {
        if RE_SETEXT.is_match(line) {
            return (end + 1, true);
        }
        if classify(line, lines.get(end + 1).copied()) != LineClass::Text
            || is_section_header && leading_width(line) >= base_indent + CODE_INDENT
        {
            break;
        }
        end += 1;
    }
    (end, false)
}

/// Build a paragraph region, or a verbatim region when the content must not be touched.
fn make_paragraph(
    contents: &[&str],
    first_prefix: String,
    rest_prefix: String,
    start_line: usize,
    end_line: usize,
    hard_breaks_allowed: bool,
) -> Region {
    let must_skip = contents.iter().any(|line| {
        line.contains(IGNORE_MARKER)
            || looks_like_code(line)
            || RE_ALIGNED_COLUMNS.is_match(line)
            || line
                .chars()
                .any(|character| ('\u{2500}'..='\u{257F}').contains(&character))
    });
    if must_skip {
        return Region::Verbatim {
            start: start_line,
            end: end_line,
        };
    }
    let mut lines = Vec::with_capacity(contents.len());
    let mut hard_breaks = Vec::with_capacity(contents.len());
    for content in contents {
        let (text, hard_break) = strip_hard_break(content, hard_breaks_allowed);
        lines.push(text);
        hard_breaks.push(hard_break);
    }
    Region::Paragraph(Paragraph {
        start_line,
        end_line,
        first_prefix,
        rest_prefix,
        last_suffix: String::new(),
        lines,
        hard_breaks,
    })
}

/// Remove a trailing hard break marker and trailing whitespace from a content line.
fn strip_hard_break(content: &str, allowed: bool) -> (String, HardBreak) {
    if !allowed {
        return (content.trim_end().to_string(), HardBreak::None);
    }
    if content.ends_with('\\') && !content.ends_with("\\\\") {
        let without = content.strip_suffix('\\').unwrap_or(content);
        return (without.trim_end().to_string(), HardBreak::Backslash);
    }
    if content.ends_with("  ") && !content.trim().is_empty() {
        return (content.trim_end().to_string(), HardBreak::Spaces);
    }
    (content.trim_end().to_string(), HardBreak::None)
}

/// Leading whitespace of a line.
fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start();
    line.get(..line.len() - trimmed.len()).unwrap_or_default()
}

/// Width of the leading whitespace, counting a tab as four columns.
fn leading_width(line: &str) -> usize {
    leading_whitespace(line)
        .chars()
        .map(|character| if character == '\t' { CODE_INDENT } else { 1 })
        .sum()
}

/// The first `count` characters of the line.
fn take_chars(line: &str, count: usize) -> &str {
    let end = line.char_indices().nth(count).map_or(line.len(), |(byte, _)| byte);
    line.get(..end).unwrap_or_default()
}

/// The line without its first `count` characters.
fn skip_chars(line: &str, count: usize) -> &str {
    let start = line.char_indices().nth(count).map_or(line.len(), |(byte, _)| byte);
    line.get(start..).unwrap_or_default()
}

/// Remove up to `count` leading whitespace characters, or all leading whitespace when there is less.
fn strip_indent(line: &str, count: usize) -> &str {
    let mut start = line.len();
    for (removed, (byte, character)) in line.char_indices().enumerate() {
        if removed >= count || !character.is_whitespace() {
            start = byte;
            break;
        }
    }
    line.get(start..).unwrap_or_default()
}

#[cfg(test)]
mod test_helpers {
    use super::{Paragraph, Region};

    /// Split with an empty base prefix, a zero start line, and comment mode.
    pub fn split_comment(lines: &[&str]) -> Vec<Region> {
        super::split_paragraphs(lines, "", 0, false)
    }

    /// Split with an empty base prefix, a zero start line, and document mode.
    pub fn split_document(lines: &[&str]) -> Vec<Region> {
        super::split_paragraphs(lines, "", 0, true)
    }

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
}

#[cfg(test)]
mod test_markdown_verbatim {
    use super::test_helpers::*;

    #[test]
    fn blank_lines_are_verbatim_between_paragraphs() {
        let regions = split_comment(&["foo", "", "bar"]);
        assert_eq!(regions.len(), 3);
        assert_eq!(range(&regions[0]), (0, 1));
        assert_eq!(regions[1], verbatim_region(1, 2));
        assert_eq!(range(&regions[2]), (2, 3));
        assert_eq!(paragraph(&regions[0]).lines, vec!["foo"]);
        assert_eq!(paragraph(&regions[2]).lines, vec!["bar"]);
    }

    #[test]
    fn backtick_fence_is_verbatim_including_code_with_semicolons() {
        let regions = split_comment(&["```rust", "let x = 1; let y = 2;", "```", "after"]);
        assert_eq!(regions[0], verbatim_region(0, 3));
        assert_eq!(paragraph(&regions[1]).lines, vec!["after"]);
    }

    #[test]
    fn tilde_fence_closes_with_a_longer_fence() {
        let regions = split_comment(&["~~~", "a; b;", "~~~~", "after"]);
        assert_eq!(regions[0], verbatim_region(0, 3));
        assert_eq!(paragraph(&regions[1]).lines, vec!["after"]);
    }

    #[test]
    fn unterminated_fence_runs_to_the_end() {
        let regions = split_comment(&["text", "```", "code;", "more;"]);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[1], verbatim_region(1, 4));
    }

    #[test]
    fn atx_heading_is_verbatim() {
        let regions = split_comment(&["# Errors", "Returns an error when x."]);
        assert_eq!(regions[0], verbatim_region(0, 1));
        assert_eq!(paragraph(&regions[1]).lines, vec!["Returns an error when x."]);
    }

    #[test]
    fn setext_heading_makes_the_text_line_verbatim() {
        assert_eq!(split_comment(&["Title", "====="]), vec![verbatim_region(0, 2)]);
        assert_eq!(split_comment(&["Title", "---"]), vec![verbatim_region(0, 2)]);
    }

    #[test]
    fn horizontal_rules_and_decorative_dividers_are_verbatim() {
        assert_eq!(split_comment(&["---"]), vec![verbatim_region(0, 1)]);
        assert_eq!(split_comment(&["* * *"]), vec![verbatim_region(0, 1)]);
        assert_eq!(split_comment(&["---- Section ----"]), vec![verbatim_region(0, 1)]);
        assert_eq!(split_comment(&["==== Helpers ===="]), vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn table_rows_and_separator_rows_are_verbatim() {
        let regions = split_comment(&["| a | b |", "| --- | :---: |", "| 1 | 2 |"]);
        assert_eq!(
            regions,
            vec![verbatim_region(0, 1), verbatim_region(1, 2), verbatim_region(2, 3)]
        );
        let without_pipes = split_comment(&["a | b", "--- | ---", "1 | 2"]);
        assert_eq!(without_pipes[0], verbatim_region(0, 1));
        assert_eq!(without_pipes[1], verbatim_region(1, 2));
    }

    #[test]
    fn html_block_runs_until_a_blank_line() {
        let regions = split_comment(&["<div align=\"center\">", "text inside", "</div>", "", "after"]);
        assert_eq!(regions[0], verbatim_region(0, 3));
        assert_eq!(regions[1], verbatim_region(3, 4));
        assert_eq!(paragraph(&regions[2]).lines, vec!["after"]);
    }

    #[test]
    fn multi_line_html_comment_is_verbatim() {
        let regions = split_comment(&["<!-- start", "middle", "end -->", "after"]);
        assert_eq!(regions[0], verbatim_region(0, 3));
        assert_eq!(paragraph(&regions[1]).lines, vec!["after"]);
        assert_eq!(split_comment(&["<!-- one line -->"]), vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn link_reference_definition_is_verbatim() {
        let regions = split_comment(&["[docs]: https://example.com/docs", "See the docs."]);
        assert_eq!(regions[0], verbatim_region(0, 1));
        assert_eq!(paragraph(&regions[1]).lines, vec!["See the docs."]);
    }

    #[test]
    fn standalone_badge_link_and_url_lines_are_verbatim() {
        let badge = "[![Build](https://img.shields.io/x.svg)](https://ci.example.com)";
        assert_eq!(split_comment(&[badge]), vec![verbatim_region(0, 1)]);
        assert_eq!(
            split_comment(&["[docs](https://example.com)"]),
            vec![verbatim_region(0, 1)]
        );
        assert_eq!(
            split_comment(&["https://example.com/path"]),
            vec![verbatim_region(0, 1)]
        );
        assert_eq!(
            split_comment(&["![img](a.png) ![img](b.png)"]),
            vec![verbatim_region(0, 1)]
        );
    }

    #[test]
    fn front_matter_is_verbatim_only_in_documents() {
        let lines = ["---", "title: x", "---", "Body text"];
        let document = split_document(&lines);
        assert_eq!(document[0], verbatim_region(0, 3));
        assert_eq!(paragraph(&document[1]).lines, vec!["Body text"]);
        let comment = split_comment(&lines);
        assert_eq!(comment[0], verbatim_region(0, 1));
    }

    #[test]
    fn indented_code_lines_are_verbatim() {
        let regions = split_comment(&["Intro", "", "    indented code", "    second line", "", "After"]);
        assert_eq!(range(&regions[0]), (0, 1));
        assert_eq!(regions[1], verbatim_region(1, 2));
        assert_eq!(regions[2], verbatim_region(2, 3));
        assert_eq!(regions[3], verbatim_region(3, 4));
        assert_eq!(regions[4], verbatim_region(4, 5));
        assert_eq!(paragraph(&regions[5]).lines, vec!["After"]);
        assert_eq!(split_comment(&["\tcode"]), vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn math_block_is_verbatim() {
        let regions = split_comment(&["$$", "x = y", "$$", "after"]);
        assert_eq!(regions[0], verbatim_region(0, 3));
        assert_eq!(paragraph(&regions[1]).lines, vec!["after"]);
    }
}

#[cfg(test)]
mod test_markdown_lists {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn bullet_item_with_indented_continuation() {
        let regions = split_comment(&["- foo", "  bar"]);
        assert_eq!(regions.len(), 1);
        let item = paragraph(&regions[0]);
        assert_eq!(item.first_prefix, "- ");
        assert_eq!(item.rest_prefix, "  ");
        assert_eq!(item.lines, vec!["foo", "bar"]);
        assert_eq!((item.start_line, item.end_line), (0, 2));
    }

    #[test]
    fn ordered_item_uses_marker_width_for_rest_prefix() {
        let regions = split_comment(&["10. foo", "    bar"]);
        let item = paragraph(&regions[0]);
        assert_eq!(item.first_prefix, "10. ");
        assert_eq!(item.rest_prefix, "    ");
        assert_eq!(item.lines, vec!["foo", "bar"]);
    }

    #[test]
    fn task_item_includes_checkbox_in_first_prefix() {
        let regions = split_comment(&["- [ ] foo", "- [x] done"]);
        assert_eq!(regions.len(), 2);
        assert_eq!(paragraph(&regions[0]).first_prefix, "- [ ] ");
        assert_eq!(paragraph(&regions[0]).rest_prefix, "      ");
        assert_eq!(paragraph(&regions[0]).lines, vec!["foo"]);
        assert_eq!(paragraph(&regions[1]).first_prefix, "- [x] ");
    }

    #[test]
    fn nested_items_start_new_paragraphs() {
        let regions = split_comment(&["- foo", "  - bar", "    baz"]);
        assert_eq!(regions.len(), 2);
        assert_eq!(paragraph(&regions[0]).lines, vec!["foo"]);
        let nested = paragraph(&regions[1]);
        assert_eq!(nested.first_prefix, "  - ");
        assert_eq!(nested.rest_prefix, "    ");
        assert_eq!(nested.lines, vec!["bar", "baz"]);
    }

    #[test]
    fn sibling_items_are_separate_paragraphs() {
        let regions = split_comment(&["- foo", "- bar", "* baz"]);
        assert_eq!(regions.len(), 3);
        assert_eq!(paragraph(&regions[2]).first_prefix, "* ");
    }

    #[test]
    fn lazy_continuation_joins_the_item() {
        let regions = split_comment(&["- foo", "bar"]);
        assert_eq!(regions.len(), 1);
        assert_eq!(paragraph(&regions[0]).lines, vec!["foo", "bar"]);
    }

    #[test]
    fn deeply_indented_line_after_item_is_not_part_of_it() {
        let regions = split_comment(&["- foo", "      code"]);
        assert_eq!(regions.len(), 2);
        assert_eq!(paragraph(&regions[0]).lines, vec!["foo"]);
        assert_eq!(regions[1], verbatim_region(1, 2));
    }

    #[test]
    fn bare_marker_without_content_is_verbatim() {
        assert_eq!(split_comment(&["- "]), vec![verbatim_region(0, 1)]);
        assert_eq!(split_comment(&["1. "]), vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn base_prefix_is_prepended_to_list_prefixes() {
        let regions = split_paragraphs(&["- foo", "  bar"], "/// ", 3, false);
        let item = paragraph(&regions[0]);
        assert_eq!(item.first_prefix, "/// - ");
        assert_eq!(item.rest_prefix, "///   ");
        assert_eq!((item.start_line, item.end_line), (3, 5));
    }
}

#[cfg(test)]
mod test_markdown_paragraphs {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn blockquote_lines_form_a_paragraph_with_quote_prefix() {
        let regions = split_comment(&["> foo", "> bar"]);
        assert_eq!(regions.len(), 1);
        let quote = paragraph(&regions[0]);
        assert_eq!(quote.first_prefix, "> ");
        assert_eq!(quote.rest_prefix, "> ");
        assert_eq!(quote.lines, vec!["foo", "bar"]);
        assert_eq!((quote.start_line, quote.end_line), (0, 2));
    }

    #[test]
    fn blockquote_prefix_includes_base_prefix() {
        let regions = split_paragraphs(&["> foo", "> bar"], "/// ", 0, false);
        let quote = paragraph(&regions[0]);
        assert_eq!(quote.first_prefix, "/// > ");
        assert_eq!(quote.rest_prefix, "/// > ");
    }

    #[test]
    fn blockquote_ends_at_a_plain_line() {
        let regions = split_comment(&["> quoted", "plain"]);
        assert_eq!(regions.len(), 2);
        assert_eq!(paragraph(&regions[0]).first_prefix, "> ");
        assert_eq!(paragraph(&regions[1]).first_prefix, "");
        assert_eq!(range(&regions[1]), (1, 2));
    }

    #[test]
    fn plain_paragraph_gets_base_prefix_and_start_line_offset() {
        let regions = split_paragraphs(&["foo", "bar"], "    /// ", 7, false);
        assert_eq!(regions.len(), 1);
        let text = paragraph(&regions[0]);
        assert_eq!(text.start_line, 7);
        assert_eq!(text.end_line, 9);
        assert_eq!(text.first_prefix, "    /// ");
        assert_eq!(text.rest_prefix, "    /// ");
        assert_eq!(text.lines, vec!["foo", "bar"]);
        assert_eq!(text.last_suffix, "");
        assert_eq!(text.hard_breaks, vec![HardBreak::None, HardBreak::None]);
    }

    #[test]
    fn trailing_spaces_are_a_hard_break_in_documents() {
        let regions = split_document(&["foo  ", "bar"]);
        let text = paragraph(&regions[0]);
        assert_eq!(text.lines, vec!["foo", "bar"]);
        assert_eq!(text.hard_breaks, vec![HardBreak::Spaces, HardBreak::None]);
    }

    #[test]
    fn trailing_backslash_is_a_hard_break_in_documents() {
        let regions = split_document(&["foo\\", "bar"]);
        let text = paragraph(&regions[0]);
        assert_eq!(text.lines, vec!["foo", "bar"]);
        assert_eq!(text.hard_breaks, vec![HardBreak::Backslash, HardBreak::None]);
    }

    #[test]
    fn hard_break_markers_are_ignored_in_comments() {
        let spaces = paragraph(&split_comment(&["foo  ", "bar"])[0]).clone();
        assert_eq!(spaces.lines, vec!["foo", "bar"]);
        assert_eq!(spaces.hard_breaks, vec![HardBreak::None, HardBreak::None]);
        let backslash = paragraph(&split_comment(&["foo\\", "bar"])[0]).clone();
        assert_eq!(backslash.lines, vec!["foo\\", "bar"]);
        assert_eq!(backslash.hard_breaks, vec![HardBreak::None, HardBreak::None]);
    }

    #[test]
    fn paragraph_containing_code_like_line_is_verbatim() {
        let regions = split_comment(&["Some prose here.", "let x = foo(bar);"]);
        assert_eq!(regions, vec![verbatim_region(0, 2)]);
    }

    #[test]
    fn aligned_columns_are_verbatim() {
        assert_eq!(split_comment(&["foo     bar     baz"]), vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn box_drawing_characters_are_verbatim() {
        assert_eq!(
            split_comment(&["Layout: ┌──┐", "then └──┘"]),
            vec![verbatim_region(0, 2)]
        );
    }

    #[test]
    fn ignore_marker_makes_the_paragraph_verbatim() {
        let regions = split_comment(&["This is prose. slb-ignore", "more prose", "", "formatted"]);
        assert_eq!(regions[0], verbatim_region(0, 2));
        assert_eq!(regions[1], verbatim_region(2, 3));
        assert_eq!(paragraph(&regions[2]).lines, vec!["formatted"]);
    }

    #[test]
    fn field_line_starts_a_new_paragraph() {
        let regions = split_comment(&["Summary.", ":param x: the value", "@return nothing"]);
        assert_eq!(regions.len(), 3);
        assert_eq!(paragraph(&regions[1]).lines, vec![":param x: the value"]);
        assert_eq!(paragraph(&regions[2]).lines, vec!["@return nothing"]);
    }

    #[test]
    fn ignore_file_marker_is_only_found_in_the_first_five_lines() {
        assert!(has_ignore_file_marker(&["// slb-ignore-file"]));
        assert!(has_ignore_file_marker(&[
            "a",
            "b",
            "c",
            "d",
            "<!-- slb-ignore-file -->"
        ]));
        assert!(!has_ignore_file_marker(&["a", "b", "c", "d", "e", "slb-ignore-file"]));
        assert!(!has_ignore_file_marker(&["slb-ignore only"]));
        assert!(!has_ignore_file_marker(&[]));
    }

    #[test]
    fn regions_tile_a_mixed_document() {
        let lines = [
            "---",
            "title: Demo",
            "---",
            "",
            "# Heading",
            "",
            "Intro paragraph line one.",
            "Intro paragraph line two.",
            "",
            "- item one",
            "  continued",
            "- item two",
            "",
            "> quoted line",
            "> another quoted line",
            "",
            "```",
            "let code = 1;",
            "```",
            "",
            "| a | b |",
            "| - | - |",
            "",
            "    indented code",
            "",
            "[ref]: https://example.com",
            "Final words.",
        ];
        let regions = split_document(&lines);
        assert_tiles(&regions, lines.len());
        assert_eq!(regions[0], verbatim_region(0, 3));
        let paragraph_count = regions
            .iter()
            .filter(|region| matches!(region, Region::Paragraph(_)))
            .count();
        assert_eq!(paragraph_count, 5);
    }
}

#[cfg(test)]
mod test_looks_like_code {
    use super::*;

    #[test]
    fn detects_code_lines() {
        assert!(looks_like_code("let x = 1;"));
        assert!(looks_like_code("foo(bar);"));
        assert!(looks_like_code("use std::fs;"));
        assert!(looks_like_code("if x { y }"));
        assert!(looks_like_code("println!(\"hi\");"));
        assert!(looks_like_code("#[derive(Debug)]"));
        assert!(looks_like_code("$ cargo build"));
        assert!(looks_like_code("a -> b"));
        assert!(looks_like_code("x == y"));
        assert!(looks_like_code("fn main() {"));
        assert!(looks_like_code("a && b || c"));
    }

    #[test]
    fn accepts_prose_lines() {
        assert!(!looks_like_code("This is prose."));
        assert!(!looks_like_code("Call foo now."));
        assert!(!looks_like_code("Use the \"y\" option, or \"n\" to skip."));
        assert!(!looks_like_code("Values like a, b, and c."));
        assert!(!looks_like_code("Run `let x = 1;` first."));
        assert!(!looks_like_code("If the file exists, skip it."));
        assert!(!looks_like_code(""));
        assert!(!looks_like_code("   "));
    }
}
