//! String aware scanner for the semantic line breaks formatter.
//!
//! Walks a line character by character while tracking whether the cursor sits in code,
//! in a string, in a character literal, or in a block comment,
//! and carries that state to the next line so multi line strings and comments are followed.
//! Reports where a comment starts on a line, or that the line cannot be interpreted with certainty.

use std::sync::LazyLock;

use regex::Regex;

use super::regex_literals::{
    comment_starts_at, division_can_start, find_regex_end, regex_can_start, regex_end_overlaps_comment, uncertain_regex,
};
use super::string_syntax::StringSyntax;
use crate::leading_whitespace;

/// Matches a shell heredoc start and captures the terminator word.
static RE_HEREDOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<<-?\s*(?:'(\w+)'|"(\w+)"|(\w+))"#).expect("Invalid heredoc regex"));

/// Matches a YAML block scalar indicator at the end of a key line.
pub(super) static RE_BLOCK_SCALAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|:)\s*[|>][-+]?\d*\s*$").expect("Invalid block scalar regex"));

/// How single quotes behave in a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SingleQuote {
    /// Single quotes delimit strings.
    String,
    /// Single quotes delimit character literals, or lifetimes when unclosed.
    CharLiteral,
}

/// How backticks behave in a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Backtick {
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

/// Outcome of scanning one character in the normal state.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum NormalStep {
    /// Continue scanning in the given state.
    Continue(ScanState),
    /// A line comment marker following code was found at the byte offset.
    Comment(usize),
    /// The line cannot be interpreted safely.
    Uncertain,
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

/// Scanner position within a line.
#[derive(Debug, Default)]
pub(super) struct Cursor {
    /// Index into the character vector.
    pub(super) index: usize,
    /// Whether an ambiguous construct prevents safe comment relocation.
    pub(super) uncertain: bool,
    /// Heredoc terminator to enter after this line.
    pub(super) pending_heredoc: Option<String>,
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
pub(super) fn scan_line_buffered(
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
pub(super) fn rest_starts_with(chars: &[(usize, char)], index: usize, needle: &str) -> bool {
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

#[cfg(test)]
mod test_scanner_edges {
    use crate::semantic_line_breaks::comments::test_helpers::*;

    use super::super::comments::split_source_regions;
    use super::super::file_kind::FileKind;
    use super::super::paragraph::Region;

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
