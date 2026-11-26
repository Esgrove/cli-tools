//! Markdown aware paragraph splitting for the semantic line breaks formatter.
//!
//! Splits a run of content lines into prose paragraphs that may be reflowed
//! and verbatim regions that must never change,
//! such as fenced code, headings, tables, list markers, and lines that look like code.
//! The same splitter is used for Markdown documents and for the content of comment blocks.
//! The heuristic that decides whether a line looks like code lives in [`super::looks_like_code`].

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use super::paragraph::{HardBreak, Paragraph, Region};
use super::tokenizer::has_multiline_code_span;
use cli_tools_common::leading_whitespace;

pub use super::looks_like_code::looks_like_code;

/// Classification of one content line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineClass {
    /// Empty or whitespace only.
    Blank,
    /// Opens a fenced code block with the given marker character and length.
    FenceOpen(char, usize),
    /// Opens a fence inside a list item, with indentation measured through its marker and spacing.
    ListFence(char, usize, usize),
    /// Opens a math block.
    MathFence,
    /// Opens an HTML comment.
    HtmlComment,
    /// Starts an HTML block terminated by a blank line.
    HtmlBlock,
    /// Starts a preformatted HTML block terminated by a matching closing tag.
    PreformattedHtml,
    /// Starts a pipe table (header row followed by a table separator row).
    Table,
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

/// A place a paragraph was kept verbatim by a heuristic rather than by Markdown structure.
///
/// The paragraphs behind a fenced code block, a heading, a table,
/// or a real list marker are meant to stay untouched, and skipping them is not worth a word.
/// A paragraph skipped because a line merely looks like code, has aligned columns,
/// or uses box drawing characters is different: the heuristic can be wrong,
/// and a paragraph skipped this way produces no violation at all, so nothing else says it happened.
/// A notice fills that gap without turning the skip into a violation of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipNotice {
    /// Zero based index of the line the notice applies to.
    pub line: usize,
    /// Reason the paragraph was kept verbatim.
    pub message: Cow<'static, str>,
}

/// Base prefix, start line, and document mode shared by every paragraph split from one run of lines.
#[derive(Clone, Copy)]
struct SplitContext<'a> {
    /// Prefix prepended to every paragraph prefix, for example the comment marker.
    base_prefix: &'a str,
    /// Zero based index of the first line in the text buffer.
    start_line: usize,
    /// Whether this is the top level of a Markdown document.
    is_document: bool,
    /// Whether Markdown hard breaks are preserved in this container.
    hard_breaks: bool,
}

/// Marker text that excludes the surrounding paragraph from formatting.
pub const IGNORE_MARKER: &str = "slb-ignore";

/// Marker text in the first lines of a file that excludes the whole file from formatting.
pub const IGNORE_FILE_MARKER: &str = "slb-ignore-file";

/// Number of leading lines searched for the file ignore marker.
pub const IGNORE_FILE_SEARCH_LINES: usize = 5;

/// Indentation in spaces that turns a line into an indented code block.
const CODE_INDENT: usize = 4;

/// Indentation in spaces that marks a block of its own inside a comment.
///
/// A comment keeps only what follows its marker and one space,
/// so a source line such as `#   $1 - the branch name` arrives indented by two.
/// Four columns is the Markdown rule for a document and too coarse for a comment,
/// where two columns past the line above already says the text is laid out by hand.
const COMMENT_INDENT: usize = 2;

/// Matches the opening of a fenced code block.
static RE_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}(`{3,}|~{3,})").expect("Invalid fence regex"));

/// Most indentation an opening or closing code fence may have.
const MAX_FENCE_INDENT: usize = 3;

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

/// Captures the tag name of an HTML start or end tag at the start of a line.
static RE_HTML_TAG_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}</?([A-Za-z][\w-]*)").expect("Invalid HTML tag name regex"));

/// Tag names used inline within prose, such as Javadoc and `JSDoc` markup,
/// that must not turn the whole paragraph into an untouched HTML block.
const INLINE_HTML_TAGS: &[&str] = &[
    "a", "abbr", "b", "br", "code", "em", "i", "kbd", "mark", "p", "small", "span", "strong", "sub", "sup", "u",
];

/// Matches a preformatted HTML opening tag where a raw block can start.
static RE_PRE_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s{0,3}<pre(?:[\s/>]|$)").expect("Invalid preformatted HTML opener"));

/// Matches a preformatted HTML closing tag.
static RE_PRE_CLOSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</pre\s*>").expect("Invalid preformatted HTML closer"));

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

/// Matches a run of aligned column spacing, such as the gap between a name and its description.
static RE_ALIGNED_COLUMNS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\S\s{3,}\S").expect("Invalid aligned columns regex"));

/// Split content lines into verbatim regions and prose paragraphs.
///
/// `base_prefix` is prepended to every paragraph prefix, for example the comment marker.
/// `start_line` is the zero based index of the first line in the text buffer.
/// `is_document` enables front matter detection and hard break markers.
#[must_use]
pub fn split_paragraphs(lines: &[&str], base_prefix: &str, start_line: usize, is_document: bool) -> Vec<Region> {
    split_paragraphs_with_notices(lines, base_prefix, start_line, is_document).0
}

/// Split content lines the same way [`split_paragraphs`] does, and also return the skip notices.
#[must_use]
pub fn split_paragraphs_with_notices(
    lines: &[&str],
    base_prefix: &str,
    start_line: usize,
    is_document: bool,
) -> (Vec<Region>, Vec<SkipNotice>) {
    let code_indent = if is_document { CODE_INDENT } else { COMMENT_INDENT };
    let context = SplitContext {
        base_prefix,
        start_line,
        is_document,
        hard_breaks: is_document,
    };
    let mut notices = Vec::new();
    let regions = split_paragraphs_with(lines, context, code_indent, &mut notices);
    (regions, notices)
}

/// Whether the file opts out of formatting with a marker in its first lines or right after YAML front matter.
#[must_use]
pub fn has_ignore_file_marker(lines: &[&str]) -> bool {
    if lines
        .iter()
        .take(IGNORE_FILE_SEARCH_LINES)
        .any(|line| line.contains(IGNORE_FILE_MARKER))
    {
        return true;
    }
    let after_front_matter = front_matter_end(lines);
    after_front_matter > 0
        && lines
            .get(after_front_matter..)
            .unwrap_or_default()
            .iter()
            .take(IGNORE_FILE_SEARCH_LINES)
            .any(|line| line.contains(IGNORE_FILE_MARKER))
}

/// Split content lines, using the given indentation as the mark of a block of its own.
///
/// The indentation belongs to the text the call started from, not to the nesting inside it,
/// so a blockquote keeps the rule of the document or comment that holds it.
fn split_paragraphs_with(
    lines: &[&str],
    context: SplitContext<'_>,
    code_indent: usize,
    notices: &mut Vec<SkipNotice>,
) -> Vec<Region> {
    let SplitContext {
        start_line,
        is_document,
        ..
    } = context;
    let mut regions = Vec::new();
    let count = lines.len();
    let mut index = if is_document { front_matter_end(lines) } else { 0 };
    if index > 0 {
        regions.push(verbatim(start_line, 0, index));
    }

    let mut ignore_next = false;
    let mut in_list_context = false;
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
        if !matches!(
            class,
            LineClass::Blank | LineClass::ListItem(_) | LineClass::ListFence(..)
        ) {
            in_list_context = false;
        }
        let end = match class {
            LineClass::Blank | LineClass::Verbatim => index + 1,
            LineClass::Table => {
                let table_end = find_table_end(lines, index);
                for row in index..table_end {
                    regions.push(verbatim(start_line, row, row + 1));
                }
                index = table_end;
                continue;
            }
            LineClass::FenceOpen(marker, length) => find_fence_close(lines, index, marker, length, 0),
            LineClass::ListFence(marker, length, indent) => {
                in_list_context = true;
                find_fence_close(lines, index, marker, length, indent)
            }
            LineClass::MathFence => find_math_fence_end(lines, index, line),
            LineClass::HtmlComment => find_html_comment_end(lines, index, line),
            LineClass::PreformattedHtml | LineClass::HtmlBlock => html_block_end(lines, index, &class),
            LineClass::Blockquote => {
                let end = collect_blockquote(
                    lines,
                    index,
                    context,
                    code_indent,
                    &mut ignore_next,
                    &mut regions,
                    notices,
                );
                index = end;
                continue;
            }
            LineClass::ListItem(content_start) => {
                if is_document && !in_list_context && leading_width(line) >= code_indent {
                    index + 1
                } else {
                    in_list_context = true;
                    let end = collect_list_item(lines, index, content_start);
                    let region = list_item_paragraph(lines, index, end, content_start, context, notices);
                    regions.push(apply_ignore(region, &mut ignore_next, start_line, index, end));
                    index = end;
                    continue;
                }
            }
            LineClass::Field | LineClass::Text => {
                if leading_width(line) >= code_indent {
                    index + 1
                } else {
                    let (end, is_setext) = collect_text(lines, index, code_indent);
                    let region = if is_setext {
                        verbatim(start_line, index, end)
                    } else {
                        text_paragraph(lines, index, end, context, notices)
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

/// Index past an HTML block, using the closing tag for preformatted HTML and a blank line otherwise.
fn html_block_end(lines: &[&str], index: usize, class: &LineClass) -> usize {
    let preformatted = *class == LineClass::PreformattedHtml;
    lines
        .iter()
        .enumerate()
        .skip(if preformatted { index } else { index + 1 })
        .find(|(_, line)| {
            if preformatted {
                RE_PRE_CLOSE.is_match(line)
            } else {
                line.trim().is_empty()
            }
        })
        .map_or(lines.len(), |(position, _)| position + usize::from(preformatted))
}

/// Index after a leading front matter block, or zero when the document has none.
fn front_matter_end(lines: &[&str]) -> usize {
    if lines.first().is_none_or(|line| line.trim_end() != "---") {
        return 0;
    }
    lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| matches!(line.trim_end(), "---" | "..."))
        .map_or(0, |(position, _)| position + 1)
}

/// Collect a contiguous blockquote starting at `index` and append its inner regions.
///
/// Honors a preceding `slb-ignore` marker by keeping the entire blockquote verbatim,
/// and preserves the leading indentation before `>` on the blockquote.
fn collect_blockquote(
    lines: &[&str],
    index: usize,
    context: SplitContext<'_>,
    code_indent: usize,
    ignore_next: &mut bool,
    regions: &mut Vec<Region>,
    notices: &mut Vec<SkipNotice>,
) -> usize {
    let end = lines
        .iter()
        .enumerate()
        .skip(index)
        .find(|(_, line)| classify(line, None) != LineClass::Blockquote)
        .map_or(lines.len(), |(position, _)| position);
    if *ignore_next {
        *ignore_next = false;
        regions.push(verbatim(context.start_line, index, end));
        return end;
    }
    let quote_indent = leading_whitespace(lines.get(index).copied().unwrap_or_default());
    let inner: Vec<String> = lines
        .get(index..end)
        .unwrap_or_default()
        .iter()
        .map(|line| RE_BLOCKQUOTE.replace(line, "").into_owned())
        .collect();
    let inner_refs: Vec<&str> = inner.iter().map(String::as_str).collect();
    let prefix = format!("{}{quote_indent}> ", context.base_prefix);
    let inner_context = SplitContext {
        base_prefix: &prefix,
        start_line: context.start_line + index,
        is_document: false,
        hard_breaks: context.hard_breaks,
    };
    regions.extend(split_paragraphs_with(&inner_refs, inner_context, code_indent, notices));
    end
}

/// Build the paragraph region for a list item spanning `index..end`.
fn list_item_paragraph(
    lines: &[&str],
    index: usize,
    end: usize,
    content_start: usize,
    context: SplitContext<'_>,
    notices: &mut Vec<SkipNotice>,
) -> Region {
    let SplitContext {
        base_prefix,
        start_line,
        ..
    } = context;
    let item_lines = lines.get(index..end).unwrap_or_default();
    let first_line = item_lines.first().copied().unwrap_or_default();
    // A lazy continuation carries the item on without lining up under it,
    // and a line the author did not indent may well be a note of its own,
    // so only an item written with its continuation lines indented is treated as one unit of prose.
    let indented = item_lines
        .iter()
        .skip(1)
        .all(|item_line| leading_width(item_line) >= content_start);
    let first_prefix = format!("{base_prefix}{}", take_chars(first_line, content_start));
    // A lazy item keeps its continuation lines unindented when it is reflowed,
    // since indenting them would turn it into an item the next run treats as one unit.
    let rest_prefix = if indented {
        format!("{base_prefix}{}", " ".repeat(content_start))
    } else {
        base_prefix.to_string()
    };
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
    let region = make_paragraph(
        &contents,
        first_prefix,
        rest_prefix,
        start_line + index,
        start_line + end,
        context.hard_breaks,
        notices,
    );
    if indented { mark_list_item(region) } else { region }
}

/// Build the paragraph region for plain text lines spanning `index..end`.
fn text_paragraph(
    lines: &[&str],
    index: usize,
    end: usize,
    context: SplitContext<'_>,
    notices: &mut Vec<SkipNotice>,
) -> Region {
    let SplitContext {
        base_prefix,
        start_line,
        ..
    } = context;
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
        context.hard_breaks,
        notices,
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

/// Classify one line, looking at the following line for table detection.
///
/// Every pattern below is anchored at the start of the line,
/// so the first character of the text decides which of them can match at all.
/// Checking that first keeps a line of ordinary prose, which is the common case,
/// from running the whole chain of patterns.
fn classify(line: &str, next: Option<&str>) -> LineClass {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LineClass::Blank;
    }
    let first = trimmed.as_bytes().first().copied().unwrap_or(b' ');
    if matches!(first, b'`' | b'~')
        && let Some((marker, length)) = opening_fence(line)
    {
        return LineClass::FenceOpen(marker, length);
    }
    if first == b'$' && trimmed.starts_with("$$") {
        return LineClass::MathFence;
    }
    if first == b'<' && trimmed.starts_with("<!--") {
        return LineClass::HtmlComment;
    }
    if first == b'#' && RE_HEADING.is_match(line)
        || matches!(first, b'-' | b'*' | b'_' | b'=' | b'~' | b'#' | b'/' | b'+') && RE_RULE.is_match(line)
        || first == b'[' && RE_LINK_REFERENCE.is_match(line)
    {
        return LineClass::Verbatim;
    }
    if next.is_some_and(|next| line.contains('|') && RE_TABLE_SEPARATOR.is_match(next)) {
        return LineClass::Table;
    }
    if first == b'|' {
        return LineClass::Verbatim;
    }
    if matches!(first, b'|' | b':' | b'-') && line.contains('|') && RE_TABLE_SEPARATOR.is_match(line) {
        return LineClass::Verbatim;
    }
    if first == b'<' && RE_PRE_OPEN.is_match(line) {
        return LineClass::PreformattedHtml;
    }
    if first == b'<' && RE_HTML_BLOCK.is_match(line) && !starts_with_inline_html_tag(line) {
        return LineClass::HtmlBlock;
    }
    if first == b'>' && RE_BLOCKQUOTE.is_match(line) {
        return LineClass::Blockquote;
    }
    if (matches!(first, b'-' | b'*' | b'+') || first.is_ascii_digit())
        && let Some(captures) = RE_LIST_ITEM.captures(line)
    {
        let Some(content) = captures.get(5) else {
            return LineClass::Verbatim;
        };
        let prefix = line.get(..content.start()).unwrap_or_default();
        let content_start = prefix.chars().count();
        if captures.get(4).is_none()
            && let Some((marker, length)) = opening_fence(content.as_str())
        {
            let indent = prefix
                .chars()
                .map(|character| if character == '\t' { CODE_INDENT } else { 1 })
                .sum();
            return LineClass::ListFence(marker, length, indent);
        }
        return LineClass::ListItem(content_start);
    }
    // Every alternative of the standalone link pattern needs a bracket or a scheme separator.
    if (line.contains('[') || line.contains("://")) && RE_STANDALONE_LINK.is_match(line) {
        return LineClass::Verbatim;
    }
    if matches!(first, b':' | b'@') && RE_FIELD.is_match(line) {
        return LineClass::Field;
    }
    LineClass::Text
}

/// Whether the tag that opens or closes `line` is one used inline within prose,
/// such as Javadoc and `JSDoc` markup, rather than a raw HTML block that must be preserved as is.
fn starts_with_inline_html_tag(line: &str) -> bool {
    RE_HTML_TAG_NAME
        .captures(line)
        .and_then(|captures| captures.get(1))
        .is_some_and(|name| {
            INLINE_HTML_TAGS
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(name.as_str()))
        })
}

/// Index one past the end of a pipe table starting at `header_index`.
///
/// Consumes the header row, separator row, and every consecutive non-blank row containing `|`.
fn find_table_end(lines: &[&str], header_index: usize) -> usize {
    let mut end = (header_index + 2).min(lines.len());
    while let Some(line) = lines.get(end) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains('|') {
            break;
        }
        if matches!(
            classify(line, None),
            LineClass::FenceOpen(_, _) | LineClass::MathFence | LineClass::HtmlComment | LineClass::Blockquote
        ) {
            break;
        }
        end += 1;
    }
    end
}

/// The marker and length of a valid fence opening, relative to its container.
fn opening_fence(line: &str) -> Option<(char, usize)> {
    if leading_width(line) > MAX_FENCE_INDENT {
        return None;
    }
    let fence = RE_FENCE.captures(line)?.get(1)?;
    let marker = fence.as_str().chars().next()?;
    let info = line.get(fence.end()..)?;
    // CommonMark forbids backticks in the info string of a backtick fence.
    (marker != '`' || !info.contains('`')).then_some((marker, fence.as_str().len()))
}

/// Index one past the closing fence, or the end of its container when unterminated.
///
/// A closing fence can be indented up to three columns past its container's indentation.
fn find_fence_close(lines: &[&str], open_index: usize, marker: char, length: usize, indent: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(open_index + 1)
        .find_map(|(position, line)| {
            let trimmed = line.trim();
            let width = leading_width(line);
            if width < indent && !trimmed.is_empty() {
                Some(position)
            } else if width <= indent + MAX_FENCE_INDENT
                && trimmed.len() >= length
                && trimmed.chars().all(|character| character == marker)
            {
                Some(position + 1)
            } else {
                None
            }
        })
        .unwrap_or(lines.len())
}

/// Index one past a `$$` math block starting at `index`, recognizing single-line `$$...$$` blocks.
fn find_math_fence_end(lines: &[&str], index: usize, line: &str) -> usize {
    let trimmed = line.trim();
    if trimmed.len() >= 4 && trimmed.ends_with("$$") {
        index + 1
    } else {
        find_line_containing(lines, index + 1, "$$").map_or(lines.len(), |close| close + 1)
    }
}

/// Index one past an HTML comment starting at `index`.
fn find_html_comment_end(lines: &[&str], index: usize, line: &str) -> usize {
    if line.contains("-->") {
        index + 1
    } else {
        find_line_containing(lines, index + 1, "-->").map_or(lines.len(), |close| close + 1)
    }
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
///
/// A line that ends with a colon introduces what follows it,
/// so a line indented past it belongs to a block of its own rather than to the sentence.
/// The comparison is against the line directly above,
/// since the colon that introduces a block is often not the line the paragraph started from.
fn collect_text(lines: &[&str], index: usize, code_indent: usize) -> (usize, bool) {
    let mut end = index + 1;
    while let Some(line) = lines.get(end) {
        if RE_SETEXT.is_match(line) {
            return (end + 1, true);
        }
        let above = lines.get(end - 1).copied().unwrap_or_default();
        let introduces_block = above.trim_end().ends_with(':');
        if classify(line, lines.get(end + 1).copied()) != LineClass::Text
            || introduces_block && leading_width(line) >= leading_width(above) + code_indent
        {
            break;
        }
        end += 1;
    }
    (end, false)
}

/// Reason a content line marks its paragraph as code-like, aligned, or box drawn, when it does.
fn skip_reason(line: &str) -> Option<&'static str> {
    if looks_like_code(line) {
        Some("the line looks like code")
    } else if RE_ALIGNED_COLUMNS.is_match(line) {
        Some("the line has aligned columns")
    } else if !line.is_ascii()
        && line
            .chars()
            .any(|character| ('\u{2500}'..='\u{257F}').contains(&character))
    {
        Some("the line uses box drawing characters")
    } else {
        None
    }
}

/// Build a paragraph region, or a verbatim region when the content must not be touched.
fn make_paragraph(
    contents: &[&str],
    first_prefix: String,
    rest_prefix: String,
    start_line: usize,
    end_line: usize,
    hard_breaks_allowed: bool,
    notices: &mut Vec<SkipNotice>,
) -> Region {
    let ignored = contents.iter().any(|line| line.contains(IGNORE_MARKER));
    let heuristic_skip = contents.iter().enumerate().find_map(|(offset, line)| {
        skip_reason(line).map(|message| SkipNotice {
            line: start_line + offset,
            message: Cow::Borrowed(message),
        })
    });
    if ignored || heuristic_skip.is_some() || has_multiline_code_span(contents) {
        if let Some(notice) = heuristic_skip {
            notices.push(notice);
        }
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
        list_item: false,
        lines,
        hard_breaks,
    })
}

/// Mark a paragraph region as the content of a list item.
fn mark_list_item(region: Region) -> Region {
    match region {
        Region::Paragraph(paragraph) => Region::Paragraph(Paragraph {
            list_item: true,
            ..paragraph
        }),
        verbatim @ Region::Verbatim { .. } => verbatim,
    }
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

/// Create a verbatim region for the given local line range.
const fn verbatim(start_line: usize, from: usize, to: usize) -> Region {
    Region::Verbatim {
        start: start_line + from,
        end: start_line + to,
    }
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
    fn a_table_separator_without_a_row_above_it_is_verbatim() {
        let regions = split_comment(&[":---: | :---:", "| a   | b   |"]);
        assert_eq!(regions, vec![verbatim_region(0, 1), verbatim_region(1, 2)]);
    }

    #[test]
    fn a_two_column_usage_block_is_verbatim() {
        let lines = [
            "Environment:",
            "  SERVER_PORT       game port to expect (default 9339)",
            "  PUBLIC_HOST_IP    address handed to clients for the realtime connection",
            "  SKIP_DOCKER       set to 1 to assume that the database is already up",
        ];
        let regions = split_comment(&lines);
        // The heading introduces the block and reads as prose.
        // The entries are indented past it, so each one keeps the columns it was written with.
        assert_eq!(paragraph(&regions[0]).lines, vec!["Environment:".to_string()]);
        assert_eq!(
            regions.get(1..),
            Some([verbatim_region(1, 2), verbatim_region(2, 3), verbatim_region(3, 4)].as_slice())
        );
        assert_tiles(&regions, 4);
    }

    #[test]
    fn two_columns_of_indentation_start_a_block_in_a_comment() {
        // A comment keeps only what follows the marker and one space,
        // so "#   $1 - the branch name" arrives indented by two and has to hold its own line.
        let lines = ["Arguments:", "  $1 - the branch name", "  $2 - the title"];
        let regions = split_comment(&lines);
        assert_eq!(
            regions.get(1..),
            Some([verbatim_region(1, 2), verbatim_region(2, 3)].as_slice())
        );
        assert_tiles(&regions, 3);
        // A document keeps the Markdown rule, where two columns is a lazy continuation.
        let regions = split_document(&lines);
        assert_eq!(regions.len(), 1);
        assert_eq!(paragraph(&regions[0]).lines.len(), 3);
    }

    #[test]
    fn a_block_is_introduced_by_the_line_above_it_not_by_the_first_one() {
        // The colon that introduces the entries is the second line, not the line the run began at.
        let lines = ["Read the notes first.", "Arguments:", "  $1 - the branch name"];
        let regions = split_comment(&lines);
        assert_eq!(regions.last(), Some(&verbatim_region(2, 3)));
        assert_tiles(&regions, 3);
    }

    #[test]
    fn a_single_wide_gap_marks_a_line_as_aligned() {
        let regions = split_comment(&["-f, --fix       rewrite files in place"]);
        assert_eq!(regions, vec![verbatim_region(0, 1)]);
    }

    #[test]
    fn a_double_space_inside_prose_is_still_prose() {
        let regions = split_comment(&["one sentence ends.  Another one starts."]);
        assert_eq!(
            paragraph(&regions[0]).lines,
            vec!["one sentence ends.  Another one starts."]
        );
    }

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
    fn a_javadoc_paragraph_opened_with_inline_tags_is_prose() {
        let regions = split_comment(&["<p><b>Description.</b> More text follows here."]);
        assert_eq!(
            paragraph(&regions[0]).lines,
            vec!["<p><b>Description.</b> More text follows here."]
        );
    }

    #[test]
    fn a_lone_line_break_tag_is_prose() {
        let regions = split_comment(&["<br>"]);
        assert_eq!(paragraph(&regions[0]).lines, vec!["<br>"]);
    }

    #[test]
    fn a_code_tag_starting_a_line_is_prose() {
        let regions = split_comment(&["<code>value</code> is set."]);
        assert_eq!(paragraph(&regions[0]).lines, vec!["<code>value</code> is set."]);
    }

    #[test]
    fn an_inline_tag_is_recognized_regardless_of_case() {
        let regions = split_comment(&["<B>bold</B> text."]);
        assert_eq!(paragraph(&regions[0]).lines, vec!["<B>bold</B> text."]);
    }

    #[test]
    fn a_table_tag_still_starts_an_html_block() {
        let regions = split_comment(&["<table>", "<tr><td>cell</td></tr>", "</table>", "", "after"]);
        assert_eq!(regions[0], verbatim_region(0, 3));
        assert_eq!(regions[1], verbatim_region(3, 4));
        assert_eq!(paragraph(&regions[2]).lines, vec!["after"]);
    }

    #[test]
    fn a_script_tag_still_starts_an_html_block() {
        let regions = split_comment(&["<script>", "doStuff();", "</script>", "", "after"]);
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
    fn preserve_markup_preformatted_html_across_blank_lines() {
        let source = "<PRE class=\"example\">\nfirst line\n\nKeep the punctuation; it is literal.\n</pRE>\n\nAfter this; format the prose.\n";
        let expected = source.replace("After this; format", "After this.\nFormat");
        let formatted = crate::format(source, crate::FileKind::Markdown, &crate::FormatOptions::default());
        assert_eq!(formatted.fixed_text.as_deref().unwrap_or(source), expected);
    }

    #[test]
    fn preserve_markup_blockquote_hard_breaks() {
        for marker in ["  ", "\\"] {
            let source = format!("> First part; this is still the first part{marker}\n> second part of the quote.\n");
            let expected =
                format!("> First part.\n> This is still the first part{marker}\n> second part of the quote.\n");
            let formatted = crate::format(&source, crate::FileKind::Markdown, &crate::FormatOptions::default());
            assert_eq!(formatted.fixed_text.as_deref().unwrap_or(&source), expected);
        }
    }

    #[test]
    fn preserve_markup_inline_html_code() {
        let source = "Use <code class=\"expr\">foo; bar — baz</CoDe> in the example.\n";
        let formatted = crate::format(source, crate::FileKind::Markdown, &crate::FormatOptions::default());
        assert_eq!(formatted.fixed_text.as_deref().unwrap_or(source), source);
        assert_eq!(formatted.violations, [] as [crate::Violation; 0]);
    }

    #[test]
    fn preserve_markup_em_dash_in_bare_url() {
        for url in [
            "https://example.com/alpha—beta",
            "www.example.com/alpha—beta",
            "https://example.com/alpha—",
        ] {
            let source = format!("Read ({url}) for the details.\n");
            let formatted = crate::format(&source, crate::FileKind::Markdown, &crate::FormatOptions::default());
            assert_eq!(formatted.fixed_text.as_deref().unwrap_or(&source), source);
            assert_eq!(formatted.violations, [] as [crate::Violation; 0]);
        }
    }

    #[test]
    fn a_fence_at_the_start_of_a_list_item_is_verbatim() {
        for source in [
            "- ```text\n  Keep this; keep it unchanged.\n  ```\n\nAfter this; format the prose.\n",
            "10. ~~~~text\n    Keep this; keep it unchanged.\n    ~~~\n    Still code; do not format it.\n    ~~~~\n\nAfter this; format the prose.\n",
            "- ```text\n  Keep this; keep it unchanged.\n\nAfter this; format the prose.\n",
        ] {
            let expected = source.replace("After this; format", "After this.\nFormat");
            let formatted = crate::format(source, crate::FileKind::Markdown, &crate::FormatOptions::default());
            assert_eq!(formatted.fixed_text.as_deref().unwrap_or(source), expected);
        }
    }

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
    fn an_item_continued_under_its_marker_is_marked_as_a_list_item() {
        let regions = split_comment(&["- foo", "  bar"]);
        assert!(paragraph(&regions[0]).list_item);
    }

    #[test]
    fn an_item_continued_by_a_lazy_line_is_not_marked_as_a_list_item() {
        let regions = split_comment(&["- foo", "bar"]);
        let item = paragraph(&regions[0]);
        assert_eq!(item.lines, vec!["foo", "bar"]);
        assert!(!item.list_item);
    }

    #[test]
    fn an_item_continued_by_a_lazy_line_keeps_its_continuation_lines_unindented() {
        let regions = split_comment(&["- foo", "bar"]);
        let item = paragraph(&regions[0]);
        assert_eq!(item.first_prefix, "- ");
        assert_eq!(item.rest_prefix, "");
    }

    #[test]
    fn a_text_paragraph_is_no_list_item() {
        let regions = split_comment(&["foo", "bar"]);
        assert!(!paragraph(&regions[0]).list_item);
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
mod test_skip_notices {
    use super::*;

    #[test]
    fn a_code_like_line_produces_a_skip_notice() {
        let (_, notices) = split_paragraphs_with_notices(&["Some prose here.", "let x = foo(bar);"], "", 0, false);
        assert_eq!(
            notices,
            vec![SkipNotice {
                line: 1,
                message: "the line looks like code".into(),
            }]
        );
    }

    #[test]
    fn aligned_columns_produce_a_skip_notice() {
        let (_, notices) = split_paragraphs_with_notices(&["foo     bar     baz"], "", 0, false);
        assert_eq!(
            notices,
            vec![SkipNotice {
                line: 0,
                message: "the line has aligned columns".into(),
            }]
        );
    }

    #[test]
    fn the_ignore_marker_produces_no_skip_notice() {
        let (_, notices) = split_paragraphs_with_notices(&["This is prose. slb-ignore"], "", 0, false);
        assert_eq!(notices, [] as [SkipNotice; 0]);
    }

    #[test]
    fn a_fenced_code_block_produces_no_skip_notice() {
        let (_, notices) = split_paragraphs_with_notices(&["```", "let x = 1;", "```"], "", 0, false);
        assert_eq!(notices, [] as [SkipNotice; 0]);
    }

    #[test]
    fn a_heading_produces_no_skip_notice() {
        let (_, notices) = split_paragraphs_with_notices(&["# Title"], "", 0, false);
        assert_eq!(notices, [] as [SkipNotice; 0]);
    }

    #[test]
    fn a_real_list_marker_produces_no_skip_notice() {
        let (_, notices) = split_paragraphs_with_notices(&["- one", "- two"], "", 0, false);
        assert_eq!(notices, [] as [SkipNotice; 0]);
    }
}

#[cfg(test)]
mod test_fixture_coverage {
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn every_inline_html_tag_opens_a_paragraph_line_in_a_fixture() {
        let openings: Vec<String> = INLINE_HTML_TAGS.iter().map(|tag| format!("<{tag}")).collect();
        let openings: Vec<&str> = openings.iter().map(String::as_str).collect();
        assert_fixture_lines_start_with(&openings, "INLINE_HTML_TAGS");
    }
}

#[cfg(test)]
mod test_markdown_edge_cases {
    use super::*;

    fn has_any_paragraph(regions: &[Region]) -> bool {
        regions.iter().any(|region| matches!(region, Region::Paragraph(_)))
    }

    #[test]
    fn pipe_table_without_leading_pipes_stays_verbatim() {
        let lines = ["Name | Value", "--- | ---", "foo | bar; baz", "qux | quux"];
        let regions = split_paragraphs(&lines, "", 0, true);
        assert!(
            !has_any_paragraph(&regions),
            "pipe table rows without leading | must remain verbatim: {regions:?}"
        );
    }

    #[test]
    fn indented_code_block_with_list_marker_stays_verbatim_while_nested_list_formats() {
        let code_lines = [
            "Intro paragraph.",
            "",
            "    - code item; not prose",
            "    1. second line",
        ];
        let code_regions = split_paragraphs(&code_lines, "", 0, true);
        let paragraph_lines: Vec<usize> = code_regions
            .iter()
            .filter_map(|region| match region {
                Region::Paragraph(paragraph) => Some(paragraph.start_line),
                Region::Verbatim { .. } => None,
            })
            .collect();
        assert_eq!(paragraph_lines, vec![0], "indented code lines must stay verbatim");

        let nested_list = ["- Outer item", "    - Inner item; second clause"];
        let nested_regions = split_paragraphs(&nested_list, "", 0, true);
        assert_eq!(
            nested_regions
                .iter()
                .filter(|region| matches!(region, Region::Paragraph(_)))
                .count(),
            2,
            "nested sub-list after list item should still be parsed as list items"
        );
    }

    #[test]
    fn tab_indented_fence_and_inline_triple_backticks_are_not_fences() {
        let tab_fence = ["\t```", "First sentence. Second sentence."];
        let tab_regions = split_paragraphs(&tab_fence, "", 0, true);
        assert!(
            tab_regions
                .iter()
                .any(|region| matches!(region, Region::Paragraph(paragraph) if paragraph.start_line == 1)),
            "tab-indented ``` is an indented code block, not a fence opener: {tab_regions:?}"
        );

        let inline_span = [
            "```foo``` bar. Second sentence.",
            "",
            "Third sentence. Fourth sentence.",
        ];
        let span_regions = split_paragraphs(&inline_span, "", 0, true);
        assert_eq!(
            span_regions
                .iter()
                .filter(|region| matches!(region, Region::Paragraph(_)))
                .count(),
            2,
            "inline triple-backtick code span must not swallow the rest of the document: {span_regions:?}"
        );

        let tab_close_inside_fence = [
            "```",
            "\t```",
            "inside fence. still code.",
            "```",
            "Real prose. Second.",
        ];
        let close_regions = split_paragraphs(&tab_close_inside_fence, "", 0, true);
        let para_starts: Vec<usize> = close_regions
            .iter()
            .filter_map(|region| match region {
                Region::Paragraph(paragraph) => Some(paragraph.start_line),
                Region::Verbatim { .. } => None,
            })
            .collect();
        assert_eq!(para_starts, vec![4], "tab-indented ``` inside fence must not close it");
    }

    #[test]
    fn single_line_math_block_does_not_swallow_following_prose() {
        let lines = ["$$x + y = z$$", "", "First sentence. Second sentence."];
        let regions = split_paragraphs(&lines, "", 0, true);
        assert!(
            regions
                .iter()
                .any(|region| matches!(region, Region::Paragraph(paragraph) if paragraph.start_line == 2)),
            "prose after single-line $$...$$ must still be formatted: {regions:?}"
        );
    }

    #[test]
    fn ignore_marker_applies_to_blockquote_and_indented_blockquote_keeps_indent() {
        let ignored = [
            "<!-- slb-ignore -->",
            "> Quoted clause; second clause.",
            "",
            "After quote; should still format.",
        ];
        let ignored_regions = split_paragraphs(&ignored, "", 0, true);
        let para_starts: Vec<usize> = ignored_regions
            .iter()
            .filter_map(|region| match region {
                Region::Paragraph(paragraph) => Some(paragraph.start_line),
                Region::Verbatim { .. } => None,
            })
            .collect();
        assert_eq!(
            para_starts,
            vec![3],
            "slb-ignore must skip the blockquote itself and not leak to the next paragraph: {ignored_regions:?}"
        );

        let indented = ["  > First clause. Second clause."];
        let indented_regions = split_paragraphs(&indented, "", 0, true);
        let Some(Region::Paragraph(paragraph)) = indented_regions.first() else {
            panic!("expected blockquote paragraph");
        };
        assert_eq!(paragraph.first_prefix, "  > ");
    }
}
