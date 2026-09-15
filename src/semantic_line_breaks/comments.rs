//! Comment extraction and trailing comment detection for the semantic line breaks formatter.
//!
//! Finds line comment blocks, block comments, and Python docstrings in source files
//! and hands their content to the Markdown splitter.
//! Also implements the string and regex aware scanner that detects comments sharing a line with code
//! and produces the replacement lines that move such comments above the code.

use std::sync::LazyLock;

use regex::Regex;

use super::markdown;
use super::types::{FileKind, FormatOptions, Region, Violation, ViolationKind};
use crate::{leading_whitespace, starts_with_ignore_case};

/// Matches the opening of a Python docstring and captures indentation, string prefix, quotes, and the rest.
static RE_DOCSTRING_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^(\s*)([rRuUbBfF]{0,2})("""|''')(.*)$"#).expect("Invalid docstring regex"));

/// Matches a shell heredoc start and captures the terminator word.
static RE_HEREDOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<<-?\s*(?:'(\w+)'|"(\w+)"|(\w+))"#).expect("Invalid heredoc regex"));

/// Matches a YAML block scalar indicator at the end of a key line.
static RE_BLOCK_SCALAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|:)\s*[|>][-+]?\d*\s*$").expect("Invalid block scalar regex"));

/// How single quotes behave in a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SingleQuote {
    /// Single quotes delimit strings.
    String,
    /// Single quotes delimit character literals, or lifetimes when unclosed.
    CharLiteral,
}

/// How backticks behave in a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backtick {
    /// Backticks have no string meaning.
    None,
    /// Backticks delimit raw strings that may span lines.
    RawString,
    /// Backticks delimit template literals that may span lines.
    Template,
}

/// Scanner state carried from one line to the next.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScanState {
    /// Regular code.
    #[default]
    Normal,
    /// Inside a block comment with the given nesting depth.
    InBlockComment(usize),
    /// Inside a string that may span lines, delimited by the given quote.
    InString(char),
    /// Inside a triple quoted string delimited by the given quote character.
    InTripleQuote(char),
    /// Inside a Rust raw string closed by a quote followed by the given number of hashes.
    InRawString(usize),
    /// Inside a JavaScript template literal.
    InTemplate,
    /// Inside a Go raw string.
    InBacktickRaw,
    /// Inside a shell heredoc ending at a line equal to the terminator.
    InHeredoc(String),
    /// Inside a YAML block scalar with lines indented more than the given width.
    InBlockScalar(usize),
    /// An ambiguous slash may hide a multiline construct, so the remaining source is protected.
    Uncertain,
}

/// String and comment syntax of a language for the trailing comment scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringSyntax {
    /// Line comment marker.
    line_marker: &'static str,
    /// Block comment delimiters.
    block_comment: Option<(&'static str, &'static str)>,
    /// Whether block comments nest.
    nested_block_comments: bool,
    /// Whether double quotes delimit strings.
    double_quote: bool,
    /// Single quote behaviour.
    single_quote: SingleQuote,
    /// Backtick behaviour.
    backtick: Backtick,
    /// Whether triple quotes delimit multi-line strings.
    triple_quotes: bool,
    /// Whether Rust style raw strings exist.
    rust_raw_strings: bool,
    /// Whether backslash escapes quotes inside double quoted strings.
    backslash_escapes: bool,
    /// Whether backslash escapes quotes inside single quoted strings.
    single_quote_escapes: bool,
    /// Whether a doubled quote escapes itself inside a string.
    doubled_quote_escape: bool,
    /// Whether plain double quoted strings may span lines.
    multiline_strings: bool,
    /// Whether the comment marker only counts after whitespace.
    marker_needs_leading_space: bool,
    /// Whether shell heredocs exist.
    heredoc: bool,
    /// Whether YAML block scalars exist.
    block_scalars: bool,
}

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

/// Result of scanning one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    /// State after the line.
    pub state: ScanState,
    /// Byte offset of a line comment marker following code, when found with certainty.
    pub comment_start: Option<usize>,
    /// Whether the scanner gave up on the line.
    pub uncertain: bool,
}

/// What one pass of the scanner found about every line of a file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineScan {
    /// Whether the line starts inside a multi line string, template, heredoc, or an uncertain region.
    inside_string: Vec<bool>,
    /// Byte offset where a trailing comment starts, for the lines that have one.
    comment_start: Vec<Option<usize>>,
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
    split_source_regions_scanned(lines, kind, &scan_lines(lines, kind))
}

/// Split source lines into verbatim regions and comment paragraphs, reusing an existing scan.
#[must_use]
pub fn split_source_regions_scanned(lines: &[&str], kind: FileKind, scan: &LineScan) -> Vec<Region> {
    let style = kind.comment_style();
    let count = lines.len();
    let inside_string = &scan.inside_string;
    let mut regions = Vec::new();
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
            && let Some(end) = docstring_regions(lines, index, &mut regions)
        {
            index = end;
            continue;
        }

        if let Some(block) = style.block
            && trimmed.starts_with(block.open)
        {
            let end = block_comment_regions(lines, index, block.open, block.close, block.continuation, &mut regions);
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
            regions.extend(markdown::split_paragraphs(&contents, &prefix, index, false));
            index = end;
            continue;
        }

        regions.push(Region::Verbatim {
            start: index,
            end: index + 1,
        });
        index += 1;
    }
    regions
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
    for (index, line) in lines.iter().enumerate() {
        let Some(comment_start) = scan.comment_start.get(index).copied().flatten() else {
            continue;
        };
        let Some(trailing) = trailing_comment(line, comment_start, &syntax) else {
            continue;
        };
        if is_directive(&trailing.text, options) {
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

/// Scanner position within a line.
#[derive(Debug, Default)]
struct Cursor {
    /// Index into the character vector.
    index: usize,
    /// Whether an ambiguous construct prevents safe comment relocation.
    uncertain: bool,
    /// Heredoc terminator to enter after this line.
    pending_heredoc: Option<String>,
}

/// Outcome of scanning one character in the normal state.
#[derive(Debug, PartialEq, Eq)]
enum NormalStep {
    /// Continue scanning in the given state.
    Continue(ScanState),
    /// A line comment marker following code was found at the byte offset.
    Comment(usize),
    /// The line cannot be interpreted safely.
    Uncertain,
}

/// Scan one line of code for a comment marker that follows code.
#[must_use]
pub fn scan_line(line: &str, state: ScanState, syntax: &StringSyntax) -> ScanResult {
    let mut characters = Vec::new();
    scan_line_buffered(line, state, syntax, &mut characters)
}

/// Scan one line, reusing the given character buffer.
///
/// The buffer holds the character indices of the line.
/// Reusing it across the lines of a file avoids one allocation per line.
fn scan_line_buffered(
    line: &str,
    state: ScanState,
    syntax: &StringSyntax,
    characters: &mut Vec<(usize, char)>,
) -> ScanResult {
    let mut state = state;
    if let Some(result) = line_level_state(line, &mut state) {
        return result;
    }
    characters.clear();
    characters.extend(line.char_indices());
    let chars = characters.as_slice();
    let mut cursor = Cursor::default();
    while cursor.index < chars.len() {
        match state {
            ScanState::Normal => match scan_normal(line, chars, &mut cursor, syntax) {
                NormalStep::Continue(next_state) => state = next_state,
                NormalStep::Comment(byte) => {
                    return ScanResult {
                        state: ScanState::Normal,
                        comment_start: (!cursor.uncertain).then_some(byte),
                        uncertain: cursor.uncertain,
                    };
                }
                NormalStep::Uncertain => return uncertain_result(),
            },
            ScanState::InHeredoc(_) | ScanState::InBlockScalar(_) | ScanState::Uncertain => break,
            other => state = continue_state(other, chars, &mut cursor.index, syntax),
        }
    }
    if state == ScanState::Normal {
        if let Some(terminator) = cursor.pending_heredoc {
            state = ScanState::InHeredoc(terminator);
        } else if syntax.block_scalars && RE_BLOCK_SCALAR.is_match(line) {
            state = ScanState::InBlockScalar(leading_whitespace(line).chars().count());
        }
    }
    ScanResult {
        state,
        comment_start: None,
        uncertain: cursor.uncertain,
    }
}

/// Handle states that apply to whole lines, returning the result when the line needs no scanning.
fn line_level_state(line: &str, state: &mut ScanState) -> Option<ScanResult> {
    match state {
        ScanState::Uncertain => Some(ScanResult {
            state: ScanState::Uncertain,
            comment_start: None,
            uncertain: true,
        }),
        ScanState::InHeredoc(terminator) => {
            if line.trim() == terminator.as_str() {
                *state = ScanState::Normal;
            }
            Some(ScanResult {
                state: state.clone(),
                comment_start: None,
                uncertain: false,
            })
        }
        ScanState::InBlockScalar(indent) => {
            if line.trim().is_empty() || leading_whitespace(line).chars().count() > *indent {
                return Some(ScanResult {
                    state: state.clone(),
                    comment_start: None,
                    uncertain: false,
                });
            }
            *state = ScanState::Normal;
            None
        }
        _ => None,
    }
}

/// Advance through a string or block comment that started earlier, returning the new state.
fn continue_state(state: ScanState, chars: &[(usize, char)], index: &mut usize, syntax: &StringSyntax) -> ScanState {
    let at = |position: usize| chars.get(position).map(|(_, character)| *character);
    let Some(character) = at(*index) else {
        return state;
    };
    match state {
        ScanState::InBlockComment(depth) => {
            let Some((open, close)) = syntax.block_comment else {
                return ScanState::Normal;
            };
            if rest_starts_with(chars, *index, close) {
                *index += close.chars().count();
                if depth <= 1 {
                    ScanState::Normal
                } else {
                    ScanState::InBlockComment(depth - 1)
                }
            } else if syntax.nested_block_comments && rest_starts_with(chars, *index, open) {
                *index += open.chars().count();
                ScanState::InBlockComment(depth + 1)
            } else {
                *index += 1;
                state
            }
        }
        ScanState::InString(quote) => {
            if character == '\\' && syntax.backslash_escapes {
                *index += 2;
                state
            } else {
                *index += 1;
                if character == quote { ScanState::Normal } else { state }
            }
        }
        ScanState::InTripleQuote(quote) => {
            if is_triple(chars, *index, quote) {
                *index += 3;
                ScanState::Normal
            } else {
                *index += if character == '\\' { 2 } else { 1 };
                state
            }
        }
        ScanState::InRawString(hashes) => {
            if character == '"' && (1..=hashes).all(|offset| at(*index + offset) == Some('#')) {
                *index += 1 + hashes;
                ScanState::Normal
            } else {
                *index += 1;
                state
            }
        }
        ScanState::InTemplate => {
            if character == '\\' {
                *index += 2;
                state
            } else {
                *index += 1;
                if character == '`' { ScanState::Normal } else { state }
            }
        }
        ScanState::InBacktickRaw => {
            *index += 1;
            if character == '`' { ScanState::Normal } else { state }
        }
        ScanState::Normal | ScanState::InHeredoc(_) | ScanState::InBlockScalar(_) | ScanState::Uncertain => state,
    }
}

/// Scan one character in the normal state.
fn scan_normal(line: &str, chars: &[(usize, char)], cursor: &mut Cursor, syntax: &StringSyntax) -> NormalStep {
    let at = |position: usize| chars.get(position).map(|(_, character)| *character);
    let index = cursor.index;
    let Some(&(byte, character)) = chars.get(index) else {
        return NormalStep::Continue(ScanState::Normal);
    };

    if rest_starts_with(chars, index, syntax.line_marker) {
        let previous = index.checked_sub(1).and_then(at);
        let is_url = syntax.line_marker == "//" && previous == Some(':');
        let needs_space = syntax.marker_needs_leading_space
            && previous.is_some_and(|previous| !previous.is_whitespace() && previous != ';');
        if !is_url && !needs_space {
            return NormalStep::Comment(byte);
        }
        cursor.index += syntax.line_marker.chars().count();
        return NormalStep::Continue(ScanState::Normal);
    }
    if let Some((open, _)) = syntax.block_comment
        && rest_starts_with(chars, index, open)
    {
        cursor.index += open.chars().count();
        return NormalStep::Continue(ScanState::InBlockComment(1));
    }
    if character == '"' && syntax.double_quote || character == '\'' {
        return scan_quote(chars, cursor, syntax, character);
    }
    if character == '`' && syntax.backtick != Backtick::None {
        cursor.index += 1;
        let state = if syntax.backtick == Backtick::Template {
            ScanState::InTemplate
        } else {
            ScanState::InBacktickRaw
        };
        return NormalStep::Continue(state);
    }
    if syntax.rust_raw_strings && character == 'r' && is_raw_string_start(chars, index) {
        let mut hashes = 0;
        while at(index + 1 + hashes) == Some('#') {
            hashes += 1;
        }
        cursor.index += 2 + hashes;
        return NormalStep::Continue(ScanState::InRawString(hashes));
    }
    if syntax.heredoc && character == '<' && at(index + 1) == Some('<') && at(index + 2) != Some('<') {
        if let Some(rest) = line.get(byte..)
            && let Some(captures) = RE_HEREDOC.captures(rest)
        {
            let word = captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))
                .map(|group| group.as_str().to_string());
            cursor.pending_heredoc = word;
        }
        cursor.index += 2;
        return NormalStep::Continue(ScanState::Normal);
    }
    if character == '/' {
        return scan_slash(line, chars, cursor, syntax);
    }
    cursor.index += 1;
    NormalStep::Continue(ScanState::Normal)
}

/// Skip regex literals without mistaking division operators or ordinary paths for their contents.
fn scan_slash(line: &str, chars: &[(usize, char)], cursor: &mut Cursor, syntax: &StringSyntax) -> NormalStep {
    let Some(&(byte, _)) = chars.get(cursor.index) else {
        return NormalStep::Continue(ScanState::Normal);
    };
    let prefix = line.get(..byte).unwrap_or_default();
    if division_can_start(prefix) {
        cursor.index += 1;
        if chars
            .get(cursor.index)
            .is_some_and(|(_, character)| *character == '=' || *character == '/' && syntax.line_marker != "//")
        {
            cursor.index += 1;
        }
        return NormalStep::Continue(ScanState::Normal);
    }
    let rest = line.get(byte..).unwrap_or_default();
    if let Some(end) = find_regex_end(chars, cursor.index + 1) {
        let end_byte = chars.get(end).map_or(line.len(), |&(offset, _)| offset);
        let candidate = line.get(byte..end_byte).unwrap_or_default();
        if syntax.heredoc && candidate.contains("<<") {
            return uncertain_regex(rest, chars.len(), cursor, syntax);
        }
        if regex_can_start(prefix) {
            if prefix.trim_end().ends_with(char::is_alphanumeric)
                && regex_end_overlaps_comment(chars, end, candidate, syntax)
            {
                if comment_starts_at(chars, end, syntax) {
                    return uncertain_regex(rest, chars.len(), cursor, syntax);
                }
                cursor.index += 1;
                return NormalStep::Continue(ScanState::Normal);
            }
            cursor.index = end;
            return NormalStep::Continue(ScanState::Normal);
        }
        if candidate.contains(syntax.line_marker)
            || candidate.contains(['\\', '\'', '"', '`'])
            || syntax.block_comment.is_some_and(|(open, _)| candidate.contains(open))
        {
            return uncertain_regex(rest, chars.len(), cursor, syntax);
        }
    } else if rest.contains(['\\', '[', '\'', '"', '`'])
        || syntax.block_comment.is_some_and(|(open, _)| rest.contains(open))
    {
        return uncertain_regex(rest, chars.len(), cursor, syntax);
    }
    cursor.index += 1;
    NormalStep::Continue(ScanState::Normal)
}

/// Contextual words may be ordinary identifiers before division.
/// Do not consume a comment opener as a regex delimiter, but allow comments attached after a complete regex.
/// A statement separator makes an attached comment ambiguous, so the caller must leave that source unchanged.
fn regex_end_overlaps_comment(chars: &[(usize, char)], end: usize, candidate: &str, syntax: &StringSyntax) -> bool {
    end.checked_sub(1)
        .is_some_and(|index| comment_starts_at(chars, index, syntax))
        && (candidate.contains(';') || !comment_starts_at(chars, end, syntax))
}

/// Whether a line or block comment opener begins at a character index.
fn comment_starts_at(chars: &[(usize, char)], index: usize, syntax: &StringSyntax) -> bool {
    rest_starts_with(chars, index, syntax.line_marker)
        || syntax
            .block_comment
            .is_some_and(|(open, _)| rest_starts_with(chars, index, open))
}

/// Preserve subsequent source when an unsupported regex may hide a multiline construct.
fn uncertain_regex(rest: &str, line_length: usize, cursor: &mut Cursor, syntax: &StringSyntax) -> NormalStep {
    if rest.contains('`') && syntax.backtick != Backtick::None
        || syntax.block_comment.is_some_and(|(open, _)| rest.contains(open))
        || rest.ends_with('\\')
        || syntax.multiline_strings && rest.contains('"')
        || syntax.triple_quotes && (rest.contains("\"\"\"") || rest.contains("'''"))
        || syntax.heredoc && (rest.contains("<<") || cursor.pending_heredoc.is_some())
        || syntax.block_scalars && RE_BLOCK_SCALAR.is_match(rest)
    {
        cursor.index = line_length;
        cursor.uncertain = true;
        NormalStep::Continue(ScanState::Uncertain)
    } else {
        NormalStep::Uncertain
    }
}

/// Recognize definite operands before division so later strings and block comments are still tracked.
///
/// Contextual keywords can introduce regex expressions and must not be treated as operands here.
fn division_can_start(prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    let Some(previous) = prefix.chars().next_back() else {
        return false;
    };
    if matches!(previous, '\'' | '"' | '`' | ']') || prefix.ends_with("++") || prefix.ends_with("--") {
        return true;
    }
    if !previous.is_alphanumeric() && previous != '_' && previous != '$' {
        return false;
    }
    let word = prefix
        .rsplit(|character: char| !character.is_alphanumeric() && character != '_' && character != '$')
        .next()
        .unwrap_or_default();
    !matches!(
        word,
        "return"
            | "throw"
            | "case"
            | "delete"
            | "void"
            | "typeof"
            | "else"
            | "do"
            | "in"
            | "instanceof"
            | "yield"
            | "await"
            | "of"
            | "new"
            | "extends"
            | "default"
    )
}

/// Recognize contexts that require an expression rather than a division operator.
///
/// Closing parentheses and braces, line starts, and postfix operators are ambiguous.
/// Leaving those lines alone is safer than interpreting regex contents as comments or strings.
fn regex_can_start(prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    let Some(previous) = prefix.chars().next_back() else {
        return false;
    };
    match previous {
        '=' | '(' | '[' | '{' | ',' | ':' | ';' | '?' | '&' | '|' | '~' | '%' | '*' => true,
        '+' | '-' => !prefix
            .strip_suffix(previous)
            .is_some_and(|rest| rest.ends_with(previous)),
        '>' => prefix.ends_with("=>"),
        _ => {
            let word_start = prefix
                .rfind(|character: char| !character.is_alphanumeric() && character != '_' && character != '$')
                .map_or(0, |index| index + 1);
            let word = prefix.get(word_start..).unwrap_or_default();
            matches!(
                word,
                "return"
                    | "throw"
                    | "case"
                    | "delete"
                    | "void"
                    | "typeof"
                    | "else"
                    | "do"
                    | "new"
                    | "extends"
                    | "default"
            ) && !prefix.get(..word_start).unwrap_or_default().trim_end().ends_with('.')
        }
    }
}

/// Find the end of a same-line slash-delimited regex, including its flags.
///
/// Escapes and character classes hide slash characters.
/// Unescaped quotes can start strings after division or within paths, even inside a presumed regex class.
/// Nested classes may use Unicode set syntax, so leave them unchanged rather than guess at their boundaries.
fn find_regex_end(chars: &[(usize, char)], mut index: usize) -> Option<usize> {
    let mut in_class = false;
    while let Some(&(_, character)) = chars.get(index) {
        match character {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' | '\'' | '"' | '`' => return None,
            '\\' => {
                let &(_, escaped) = chars.get(index + 1)?;
                if matches!(escaped, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                    return None;
                }
                index += 2;
                continue;
            }
            '[' if in_class => return None,
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => {
                index += 1;
                while chars.get(index).is_some_and(|(_, flag)| flag.is_ascii_alphabetic()) {
                    index += 1;
                }
                return Some(index);
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Scan a string or character literal starting at the cursor.
fn scan_quote(chars: &[(usize, char)], cursor: &mut Cursor, syntax: &StringSyntax, quote: char) -> NormalStep {
    let index = cursor.index;
    if quote == '\'' && syntax.single_quote == SingleQuote::CharLiteral {
        cursor.index = skip_char_literal(chars, index);
        return NormalStep::Continue(ScanState::Normal);
    }
    if syntax.triple_quotes && is_triple(chars, index, quote) {
        cursor.index += 3;
        return NormalStep::Continue(ScanState::InTripleQuote(quote));
    }
    let escapes = if quote == '"' {
        syntax.backslash_escapes
    } else {
        syntax.single_quote_escapes
    };
    match find_string_end(chars, index + 1, quote, escapes, syntax.doubled_quote_escape) {
        Some(end) => {
            cursor.index = end;
            NormalStep::Continue(ScanState::Normal)
        }
        None if quote == '"' && syntax.multiline_strings => {
            cursor.index = chars.len();
            NormalStep::Continue(ScanState::InString(quote))
        }
        None => NormalStep::Uncertain,
    }
}

/// Whether the remaining line at the character index starts with `needle`.
fn rest_starts_with(chars: &[(usize, char)], index: usize, needle: &str) -> bool {
    let mut needle_chars = needle.chars();
    let mut position = index;
    loop {
        let Some(expected) = needle_chars.next() else {
            return true;
        };
        if chars.get(position).is_none_or(|(_, character)| *character != expected) {
            return false;
        }
        position += 1;
    }
}

/// Whether three consecutive `quote` characters start at `index`.
fn is_triple(chars: &[(usize, char)], index: usize, quote: char) -> bool {
    (0..3).all(|offset| {
        chars
            .get(index + offset)
            .is_some_and(|(_, character)| *character == quote)
    })
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

/// Scanner result for a line the scanner cannot interpret.
const fn uncertain_result() -> ScanResult {
    ScanResult {
        state: ScanState::Normal,
        comment_start: None,
        uncertain: true,
    }
}

/// Index one past the closing quote of a string starting after `start`, or `None` when unterminated.
fn find_string_end(
    chars: &[(usize, char)],
    start: usize,
    quote: char,
    backslash_escapes: bool,
    doubled_quote_escape: bool,
) -> Option<usize> {
    let mut index = start;
    while let Some(&(_, character)) = chars.get(index) {
        if character == '\\' && backslash_escapes {
            index += 2;
            continue;
        }
        if character == quote {
            if doubled_quote_escape && chars.get(index + 1).is_some_and(|(_, next)| *next == quote) {
                index += 2;
                continue;
            }
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

/// Index after a character literal starting at `start`, or `start + 1` for a lifetime.
fn skip_char_literal(chars: &[(usize, char)], start: usize) -> usize {
    let at = |position: usize| chars.get(position).map(|(_, character)| *character);
    if at(start + 1) == Some('\\') {
        let mut index = start + 2;
        while index < chars.len() && index < start + 12 {
            if at(index) == Some('\'') {
                return index + 1;
            }
            index += 1;
        }
        return start + 1;
    }
    if at(start + 2) == Some('\'') {
        return start + 3;
    }
    start + 1
}

/// Whether the `r` at `index` starts a Rust raw string such as `r"` or `br#"`.
fn is_raw_string_start(chars: &[(usize, char)], index: usize) -> bool {
    let at = |position: usize| chars.get(position).map(|(_, character)| *character);
    let mut offset = 1;
    while at(index + offset) == Some('#') {
        offset += 1;
    }
    if at(index + offset) != Some('"') {
        return false;
    }
    let is_identifier_char = |character: char| character.is_alphanumeric() || character == '_';
    match index.checked_sub(1).and_then(at) {
        None => true,
        Some('b' | 'c') => index
            .checked_sub(2)
            .and_then(at)
            .is_none_or(|before| !is_identifier_char(before)),
        Some(previous) => !is_identifier_char(previous),
    }
}

/// String syntax for languages the trailing comment scanner supports.
const fn string_syntax(kind: FileKind) -> Option<StringSyntax> {
    let base = StringSyntax {
        line_marker: "//",
        block_comment: Some(("/*", "*/")),
        nested_block_comments: false,
        double_quote: true,
        single_quote: SingleQuote::CharLiteral,
        backtick: Backtick::None,
        triple_quotes: false,
        rust_raw_strings: false,
        backslash_escapes: true,
        single_quote_escapes: true,
        doubled_quote_escape: false,
        multiline_strings: false,
        marker_needs_leading_space: false,
        heredoc: false,
        block_scalars: false,
    };
    let syntax = match kind {
        FileKind::Rust => StringSyntax {
            nested_block_comments: true,
            rust_raw_strings: true,
            multiline_strings: true,
            ..base
        },
        FileKind::CLike => StringSyntax {
            triple_quotes: true,
            ..base
        },
        FileKind::JavaScript => StringSyntax {
            single_quote: SingleQuote::String,
            backtick: Backtick::Template,
            ..base
        },
        FileKind::Go => StringSyntax {
            backtick: Backtick::RawString,
            ..base
        },
        FileKind::Python => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            triple_quotes: true,
            ..base
        },
        FileKind::Shell => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            single_quote_escapes: false,
            marker_needs_leading_space: true,
            heredoc: true,
            ..base
        },
        FileKind::Toml => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            single_quote_escapes: false,
            triple_quotes: true,
            ..base
        },
        FileKind::Yaml => StringSyntax {
            line_marker: "#",
            block_comment: None,
            single_quote: SingleQuote::String,
            single_quote_escapes: false,
            doubled_quote_escape: true,
            marker_needs_leading_space: true,
            block_scalars: true,
            ..base
        },
        FileKind::Dockerfile
        | FileKind::Makefile
        | FileKind::Ruby
        | FileKind::Sql
        | FileKind::Lua
        | FileKind::Markdown => return None,
    };
    Some(syntax)
}

/// The line comment marker the trimmed line starts with, when followed by whitespace or the end of the line.
fn line_marker<'a>(trimmed: &str, markers: &[&'a str]) -> Option<&'a str> {
    markers.iter().copied().find(|marker| {
        trimmed
            .strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    })
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
        regions.extend(markdown::split_paragraphs(&contents, &prefix, inner, false));
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
fn docstring_regions(lines: &[&str], index: usize, regions: &mut Vec<Region>) -> Option<usize> {
    let line = lines.get(index)?;
    let captures = RE_DOCSTRING_OPEN.captures(line)?;
    let indent = captures.get(1).map_or("", |group| group.as_str());
    let prefix = captures.get(2).map_or("", |group| group.as_str());
    let quote = captures.get(3).map_or("\"\"\"", |group| group.as_str());
    let rest = captures.get(4).map_or("", |group| group.as_str());
    if rest.contains(quote) {
        return None;
    }
    let close_index = lines
        .iter()
        .enumerate()
        .skip(index + 1)
        .find(|(_, candidate)| candidate.contains(quote))
        .map(|(position, _)| position)?;
    let close_line = lines.get(close_index)?;
    let (close_before, close_after) = close_line.split_once(quote)?;
    let closing_has_content = !close_before.trim().is_empty();
    if !close_after.trim().is_empty() {
        return None;
    }

    let mut contents: Vec<&str> = Vec::new();
    let content_start = if rest.trim().is_empty() {
        regions.push(Region::Verbatim {
            start: index,
            end: index + 1,
        });
        index + 1
    } else {
        contents.push(rest);
        index
    };
    for body_line in lines.get(index + 1..close_index).unwrap_or_default() {
        contents.push(body_line.strip_prefix(indent).unwrap_or_else(|| body_line.trim_start()));
    }
    if closing_has_content {
        contents.push(
            close_before
                .strip_prefix(indent)
                .unwrap_or_else(|| close_before.trim_start()),
        );
    }

    let mut inner = markdown::split_paragraphs(&contents, indent, content_start, false);
    for region in &mut inner {
        if let Region::Paragraph(paragraph) = region {
            if paragraph.start_line == index && !rest.trim().is_empty() {
                paragraph.first_prefix = format!("{indent}{prefix}{quote}");
            }
            if closing_has_content && paragraph.end_line == close_index + 1 {
                paragraph.last_suffix = quote.to_string();
            }
        }
    }
    regions.extend(inner);
    if !closing_has_content {
        regions.push(Region::Verbatim {
            start: close_index,
            end: close_index + 1,
        });
    }
    Some(close_index + 1)
}

#[cfg(test)]
mod test_helpers {
    use super::{FileKind, FormatOptions, Region, Violation, fix_trailing_comments};
    use crate::semantic_line_breaks::types::Paragraph;

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
mod test_scanner_edges {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn a_long_lifetime_is_not_a_character_literal() {
        assert_eq!(
            replacement("fn read<'a_very_long_lifetime>() {} // note", FileKind::Rust),
            Some(pair("// note", "fn read<'a_very_long_lifetime>() {}"))
        );
    }

    #[test]
    fn an_unterminated_character_escape_does_not_swallow_the_comment() {
        assert_eq!(
            replacement(r"let bad = '\not_a_real_escape; // note", FileKind::Rust),
            Some(pair("// note", r"let bad = '\not_a_real_escape;"))
        );
    }

    #[test]
    fn a_raw_string_at_the_start_of_a_line_hides_a_marker() {
        assert_eq!(
            replacement(r#"r"a // b".to_string(); // note"#, FileKind::Rust),
            Some(pair("// note", r#"r"a // b".to_string();"#))
        );
        assert_eq!(
            replacement(r#"br"a // b".to_vec(); // note"#, FileKind::Rust),
            Some(pair("// note", r#"br"a // b".to_vec();"#))
        );
    }

    #[test]
    fn a_block_comment_that_opens_on_the_last_line_is_verbatim() {
        let regions = split_source_regions(&["let x = 1;", "/* unterminated"], FileKind::Rust);
        assert_tiles(&regions, 2);
        assert!(regions.iter().all(|region| matches!(region, Region::Verbatim { .. })));
    }

    #[test]
    fn code_after_the_closing_quotes_leaves_a_docstring_verbatim() {
        let lines = [
            "def f():",
            "    \"\"\"Summary line that is long enough to matter.",
            "    \"\"\" + tail",
            "    return 1",
        ];
        let regions = split_source_regions(&lines, FileKind::Python);
        assert_tiles(&regions, 4);
        assert!(regions.iter().all(|region| matches!(region, Region::Verbatim { .. })));
    }
}

#[cfg(test)]
mod test_regex_literals {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn contextual_identifiers_preserve_real_trailing_comments() {
        for kind in [
            FileKind::Rust,
            FileKind::Go,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            let marker = syntax.line_marker;
            let suffixes: &[&str] = if marker == "//" { &["", "/", "//"] } else { &[""] };
            for identifier in ["new", "default", "typeof", "void", "delete", "do", "extends", "case"] {
                let code = format!("let value = {identifier} / divisor;");
                for suffix in suffixes {
                    let comment = format!("{marker}{suffix} note");
                    let line = format!("{code} {comment}");
                    let result = scan_line(&line, ScanState::Normal, &syntax);
                    if suffix.is_empty() {
                        assert_eq!(
                            replacement(&line, kind),
                            Some(pair(&comment, &code)),
                            "{kind:?}: {line}"
                        );
                        assert_eq!(result.comment_start, Some(code.len() + 1), "{kind:?}: {line}");
                        assert!(!result.uncertain, "{kind:?}: {line}");
                    } else {
                        assert_eq!(replacement(&line, kind), None, "{kind:?}: {line}");
                        assert_eq!(result.comment_start, None, "{kind:?}: {line}");
                        assert!(result.uncertain, "{kind:?}: {line}");
                    }
                    assert_eq!(result.state, ScanState::Normal, "{kind:?}: {line}");
                }
            }
        }
    }

    #[test]
    fn contextual_semicolon_regexes_leave_ambiguous_attached_comments_unchanged() {
        for kind in [FileKind::Rust, FileKind::Go, FileKind::CLike, FileKind::JavaScript] {
            let syntax = string_syntax(kind).expect("source syntax");
            for prefix in ["return", "new", "throw", "export default"] {
                for comment in ["// comment", "/// comment"] {
                    let line = format!("{prefix} /foo;/{comment}");
                    assert_eq!(replacement(&line, kind), None, "{kind:?}: {line}");
                    let result = scan_line(&line, ScanState::Normal, &syntax);
                    assert!(result.uncertain, "{kind:?}: {line}");
                    assert_eq!(result.comment_start, None, "{kind:?}: {line}");
                    assert_eq!(result.state, ScanState::Normal, "{kind:?}: {line}");
                    assert_eq!(
                        replaced_indices(&[&line, "let next = 1; // note"], kind),
                        vec![1],
                        "{kind:?}: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn contextual_identifiers_preserve_real_block_comment_state() {
        for kind in [FileKind::Rust, FileKind::Go, FileKind::CLike, FileKind::JavaScript] {
            let syntax = string_syntax(kind).expect("source syntax");
            for identifier in ["new", "default", "typeof", "void", "delete", "do", "extends", "case"] {
                let opening = format!("let value = {identifier} / divisor; /* block");
                let result = scan_line(&opening, ScanState::Normal, &syntax);
                assert_eq!(result.state, ScanState::InBlockComment(1), "{kind:?}: {opening}");
                assert!(!result.uncertain, "{kind:?}: {opening}");
                let lines = [
                    opening.as_str(),
                    "code // literal; not prose",
                    "code /// literal; not prose",
                    "code //// literal; not prose",
                    "*/ let next = 1; // note",
                ];
                assert_eq!(replaced_indices(&lines, kind), vec![4], "{kind:?}: {opening}");
            }
        }
    }

    #[test]
    fn contextual_regexes_preserve_attached_comments_across_syntaxes() {
        for kind in [
            FileKind::Rust,
            FileKind::Go,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            for prefix in ["return", "new", "throw", "export default"] {
                for pattern in [r"/foo/", r"/path\//", r"/[/*]/", r"/path\//g"] {
                    let code = format!("{prefix} {pattern}");
                    let comment = format!("{} note", syntax.line_marker);
                    assert_eq!(replacement(&code, kind), None, "{kind:?}: {code}");
                    assert_eq!(
                        replacement(&format!("{code} {comment}"), kind),
                        Some(pair(&comment, &code)),
                        "{kind:?}: {code}"
                    );
                    if syntax.line_marker == "//" {
                        assert_eq!(
                            replacement(&format!("{code}{comment}"), kind),
                            Some(pair(&comment, &code)),
                            "{kind:?}: {code}"
                        );
                        let opening = format!("{code}/* block");
                        assert_eq!(
                            scan_line(&opening, ScanState::Normal, &syntax).state,
                            ScanState::InBlockComment(1),
                            "{kind:?}: {opening}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn uncertain_slashes_protect_heredocs_and_block_scalars() {
        for (kind, opening, interior, closing) in [
            (
                FileKind::Shell,
                "DIR=/root cat <<EOF /dev/stdin",
                "literal # not a comment",
                "EOF",
            ),
            (
                FileKind::Shell,
                "DIR=/\"root\" cat <<'EOF'",
                "literal # not a comment",
                "EOF",
            ),
            (
                FileKind::Shell,
                "cat <<'EOF' DIR=/\"root\"",
                "literal # not a comment",
                "EOF",
            ),
            (
                FileKind::Yaml,
                "/path\"key\": |",
                "  literal # not a comment",
                "next: value",
            ),
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            let lines = [opening, interior, closing];
            assert_eq!(
                scan_line(opening, ScanState::Normal, &syntax).state,
                ScanState::Uncertain,
                "{opening}"
            );
            assert_eq!(lines_inside_strings(&lines, kind), vec![false, true, true], "{opening}");
            assert!(replaced_indices(&lines, kind).is_empty(), "{opening}");
        }
        let syntax = string_syntax(FileKind::Yaml).expect("YAML syntax");
        let lines = ["foo/\"bar\": |", "  literal # not a comment"];
        assert_eq!(
            scan_line(lines[0], ScanState::Normal, &syntax).state,
            ScanState::InBlockScalar(0)
        );
        assert!(replaced_indices(&lines, FileKind::Yaml).is_empty());
    }

    #[test]
    fn quoted_slashes_after_division_and_in_paths_are_not_regex_boundaries() {
        for (kind, code) in [
            (FileKind::Python, r#"value = default / len("/# not a comment")"#),
            (FileKind::Python, r#"value = default / len(["]/# not a comment"])"#),
            (FileKind::Shell, r#"path=/"foo/bar # literal""#),
            (FileKind::Shell, r#"path=/["]foo/bar # literal""#),
        ] {
            assert_eq!(replacement(code, kind), None, "{code}");
            assert_eq!(replaced_indices(&[code, "next = 1 # c"], kind), vec![1], "{code}");
        }
        for kind in [
            FileKind::Python,
            FileKind::Rust,
            FileKind::CLike,
            FileKind::Go,
            FileKind::Shell,
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            for identifier in ["default", "typeof", "void", "delete", "do", "new", "extends", "case"] {
                for argument in [
                    format!("\"/{marker} not a comment\""),
                    format!("[\"]/{marker} not a comment\"]"),
                ] {
                    let code = format!("value = {identifier} / len({argument})");
                    assert_eq!(replacement(&code, kind), None, "{kind:?}: {code}");
                }
            }
        }
    }

    #[test]
    fn regexes_with_unescaped_quotes_are_left_unchanged() {
        for kind in [
            FileKind::JavaScript,
            FileKind::CLike,
            FileKind::Rust,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            for code in [
                r#"pattern = /foo"bar/;"#,
                r"pattern = /foo'bar/;",
                r"pattern = /foo`bar/;",
                r#"pattern = /"'`\/\/*/dgimsuy;"#,
                r#"pattern = /["'`/]/;"#,
                r#"pattern = /[#"'/]/;"#,
                r#"pattern = /[/*"'`]/;"#,
            ] {
                assert_eq!(replacement(code, kind), None, "{kind:?}: {code}");
                assert_eq!(
                    replacement(&format!("{code} {marker} c"), kind),
                    None,
                    "{kind:?}: {code}"
                );
            }
        }
    }

    #[test]
    fn swift_bare_regex_and_hash_comment_syntax_are_protected() {
        let swift = FileKind::from_extension("swift").expect("Swift is supported");
        assert_eq!(swift, FileKind::CLike);
        let code = r"let pattern = /node_modules\/(react|react-dom)\//";
        assert_eq!(replacement(code, swift), None);
        assert_eq!(replacement(&format!("{code} // c"), swift), Some(pair("// c", code)));
        for kind in [FileKind::Python, FileKind::Shell, FileKind::Toml, FileKind::Yaml] {
            let code = r"pattern = /[ #/]/";
            assert_eq!(replacement(code, kind), None, "{kind:?}");
            assert_eq!(
                replacement(&format!("{code} # c"), kind),
                Some(pair("# c", code)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn ordinary_division_preserves_trailing_comments_across_syntaxes() {
        for kind in [
            FileKind::JavaScript,
            FileKind::CLike,
            FileKind::Rust,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            let comment = format!("{marker} c");
            for code in [
                "value = numerator / denominator",
                "value = numerator / denominator / divisor",
                "value = total() / count",
                "value = total /= count",
                "value = count++ / denominator",
                "value = count! / denominator",
                "value = generic<Type> / denominator",
            ] {
                assert_eq!(
                    replacement(&format!("{code} {comment}"), kind),
                    Some(pair(&comment, code)),
                    "{kind:?}: {code}"
                );
            }
        }
    }

    #[test]
    fn floor_division_paths_and_urls_are_not_regex_literals() {
        for code in [
            "value = 12 // 4",
            "value = total() // count",
            "value //= count",
            "value = total / count",
        ] {
            assert_eq!(
                replacement(&format!("{code} # c"), FileKind::Python),
                Some(pair("# c", code))
            );
        }
        for code in [
            "/usr/bin/true",
            "/bin/echo 'hello'",
            "cp /source /destination",
            "path=/root",
            "path=/usr/bin",
        ] {
            assert_eq!(
                replacement(&format!("{code} # c"), FileKind::Shell),
                Some(pair("# c", code)),
                "{code}"
            );
        }
        for kind in [
            FileKind::Rust,
            FileKind::Go,
            FileKind::CLike,
            FileKind::Shell,
            FileKind::Yaml,
        ] {
            let code = "url = https://example.com/path";
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            let comment = format!("{marker} c");
            assert_eq!(
                replacement(&format!("{code} {comment}"), kind),
                Some(pair(&comment, code)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn division_preserves_multiline_strings_across_syntaxes() {
        for (kind, opening, closing) in [
            (FileKind::Rust, "let value = total / count + \"", "\";"),
            (FileKind::Go, "value := total / count + `", "`"),
            (FileKind::Python, "value = total // count + \"\"\"", "\"\"\""),
            (FileKind::Shell, "cat /root <<'EOF'", "EOF"),
        ] {
            let marker = string_syntax(kind).expect("source syntax").line_marker;
            let interior = format!("{marker} literal; text must stay unchanged");
            let following = format!("next = 1 {marker} c");
            let lines = [opening, interior.as_str(), closing, following.as_str()];
            assert_eq!(
                lines_inside_strings(&lines, kind),
                vec![false, true, true, false],
                "{kind:?}"
            );
            assert_eq!(replaced_indices(&lines, kind), vec![3], "{kind:?}");
        }
    }

    #[test]
    fn unsupported_regexes_protect_following_multiline_source_across_syntaxes() {
        for (kind, delimiter) in [
            (FileKind::Rust, "\""),
            (FileKind::Go, "`"),
            (FileKind::CLike, "\"\"\""),
            (FileKind::Python, "\"\"\""),
            (FileKind::Toml, "\"\"\""),
        ] {
            let syntax = string_syntax(kind).expect("source syntax");
            let opening = format!("value = /[[a]--[/]]/ + {delimiter}");
            let interior = format!("{} literal; not prose", syntax.line_marker);
            let lines = [opening.as_str(), interior.as_str(), delimiter];
            assert_eq!(
                scan_line(&opening, ScanState::Normal, &syntax).state,
                ScanState::Uncertain
            );
            assert_eq!(lines_inside_strings(&lines, kind), vec![false, true, true], "{kind:?}");
            assert!(replaced_indices(&lines, kind).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn regex_literals_hide_comment_markers_and_escaped_delimiters() {
        for code in [
            r"test: /node_modules\/(react|react-dom)\//,",
            r"const expression = /a\/b/;",
            r"const expression = /a\/\/b/;",
            r"const expression = /a\\\//g;",
            r"const expression = /a\\/g;",
            r"const expression = /[/]/;",
            r"const expression = /[/*]/;",
            r"const expression = /[//]/;",
            r"const expression = /[\]\/]/;",
            r"const expression = /=/;",
            r"const expressions = [/a\//, /b\//];",
            r"const expression = /😀\//u;",
        ] {
            for kind in [
                FileKind::JavaScript,
                FileKind::CLike,
                FileKind::Rust,
                FileKind::Go,
                FileKind::Python,
                FileKind::Shell,
                FileKind::Toml,
                FileKind::Yaml,
            ] {
                let marker = string_syntax(kind).expect("source syntax").line_marker;
                let comment = format!("{marker} real comment");
                assert_eq!(replacement(code, kind), None, "{kind:?}: {code}");
                assert_eq!(
                    replacement(&format!("{code} {comment}"), kind),
                    Some(pair(&comment, code)),
                    "{kind:?}: {code}"
                );
            }
        }
    }

    #[test]
    fn expression_contexts_allow_regex_literals() {
        for prefix in [
            "return ",
            "throw ",
            "case ",
            "typeof ",
            "void ",
            "delete ",
            "new ",
            "export default ",
            "class Example extends ",
            "else ",
            "do ",
            "const expression = ",
            "test(",
            "const expressions = [",
            "test: ",
            "condition ? ",
            "condition && ",
            "condition || ",
            "const create = () => ",
            "value + ",
            "value - ",
            "value * ",
            "value % ",
        ] {
            let code = format!("{prefix}/path\\//g;");
            assert_eq!(
                replacement(&format!("{code} // c"), FileKind::JavaScript),
                Some(pair("// c", &code)),
                "{code}"
            );
        }
    }

    #[test]
    fn ambiguous_or_unclosed_regexes_leave_no_persistent_state() {
        let syntax = string_syntax(FileKind::JavaScript).expect("JavaScript syntax");
        for line in [
            r"const value = numerator / /path\//.source.length; // c",
            r"if (ready) /path\//.test(value); // c",
            r"if (ready) {} /path\//.test(value); // c",
            r"/path\//.test(value); // c",
            r"const expression = /[[a]--[/]]/v; // c",
        ] {
            let result = scan_line(line, ScanState::Normal, &syntax);
            assert!(result.uncertain, "{line}");
            assert_eq!(result.comment_start, None, "{line}");
            assert_eq!(result.state, ScanState::Normal, "{line}");
            assert_eq!(
                replaced_indices(&[line, "const next = 1; // c"], FileKind::JavaScript),
                vec![1],
                "{line}"
            );
        }
    }

    #[test]
    fn regex_boundaries_preserve_comments_and_block_comment_state() {
        assert_eq!(
            replacement(r"const expression = /path\//// c", FileKind::JavaScript),
            Some(pair("// c", r"const expression = /path\//"))
        );
        let lines = [
            r"const expression = /[/*]/; /* real block",
            "// inside the block",
            "*/ const next = 1; // c",
        ];
        assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![2]);
    }

    #[test]
    fn division_keeps_later_multiline_template_and_block_comment_state() {
        let syntax = string_syntax(FileKind::JavaScript).expect("JavaScript syntax");
        for operand in ["total", "42", "values[0]", "'total'", "\"total\"", "`total`", "total++"] {
            let opening = format!("const value = {operand} / count + `");
            let result = scan_line(&opening, ScanState::Normal, &syntax);
            assert!(!result.uncertain, "{opening}");
            assert_eq!(result.state, ScanState::InTemplate, "{opening}");
            let lines = [
                opening.as_str(),
                "// literal; text must stay unchanged",
                "`;",
                "const next = 1; // c",
            ];
            assert_eq!(
                lines_inside_strings(&lines, FileKind::JavaScript),
                vec![false, true, true, false]
            );
            assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![3]);
        }
        for opening in [
            "const value = total / count; /* block",
            "const value = total() / count; /* block",
        ] {
            let lines = [
                opening,
                "still inside; // not a trailing comment",
                "*/ const next = 1; // c",
            ];
            let result = scan_line(lines[0], ScanState::Normal, &syntax);
            assert_eq!(result.state, ScanState::InBlockComment(1));
            assert_eq!(replaced_indices(&lines, FileKind::JavaScript), vec![2]);
        }
    }

    #[test]
    fn ambiguous_slashes_protect_possible_multiline_constructs() {
        let syntax = string_syntax(FileKind::JavaScript).expect("JavaScript syntax");
        for opening in [
            "const value = total() / count + `",
            "const value = { total: 1 } / count + `",
            r#"if (ready) /["'`/]/.test(value);"#,
            r#"const value = total() / count + "\"#,
            r#"const expression = /[/"'`* // unclosed"#,
            r"const expression = /unfinished\",
            "const value = /[[a]--[/]]/v + `",
        ] {
            let result = scan_line(opening, ScanState::Normal, &syntax);
            assert!(result.uncertain, "{opening}");
            assert_eq!(result.state, ScanState::Uncertain, "{opening}");
            let lines = [
                opening,
                "// literal; not prose",
                "`; // still uncertain",
                "const next = 1; // c",
            ];
            assert_eq!(
                lines_inside_strings(&lines, FileKind::JavaScript),
                vec![false, true, true, true]
            );
            assert!(replaced_indices(&lines, FileKind::JavaScript).is_empty());
        }
    }
}

#[cfg(test)]
mod test_strings_are_not_comments {
    use super::*;
    use crate::semantic_line_breaks::types::Paragraph;

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
