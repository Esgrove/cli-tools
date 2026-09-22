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
use super::string_syntax::{StringSyntax, first_byte_or_slash};
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
    if state == ScanState::Normal && !crate::simd::contains_byte_of(line.as_bytes(), &syntax.trigger_bytes()) {
        return ScanResult {
            state: normal_line_end_state(line, syntax),
            comment_start: None,
            uncertain: false,
        };
    }
    scan_characters(line, state, syntax, characters, true)
}

/// Scan the line character by character, starting in a state the line level checks left in place.
///
/// With `skip_ahead`, the cursor jumps over characters that cannot change the state,
/// found with a SIMD search for the bytes that can.
fn scan_characters(
    line: &str,
    mut state: ScanState,
    syntax: &StringSyntax,
    characters: &mut Vec<(usize, char)>,
    skip_ahead: bool,
) -> ScanResult {
    characters.clear();
    characters.extend(line.char_indices());
    let chars = characters.as_slice();
    let mut cursor = Cursor::default();
    while cursor.index < chars.len() {
        if skip_ahead {
            cursor.index = next_significant_index(line, chars, cursor.index, &state, syntax);
            if cursor.index >= chars.len() {
                break;
            }
        }
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
        } else {
            state = normal_line_end_state(line, syntax);
        }
    }
    ScanResult {
        state,
        comment_start: None,
        uncertain: cursor.uncertain,
    }
}

/// State after a line that ends in code, which opens a YAML block scalar when the line introduces one.
fn normal_line_end_state(line: &str, syntax: &StringSyntax) -> ScanState {
    if syntax.block_scalars && RE_BLOCK_SCALAR.is_match(line) {
        ScanState::InBlockScalar(leading_whitespace(line).chars().count())
    } else {
        ScanState::Normal
    }
}

/// Index of the first character at or after `index` that can change the state or needs a closer look.
///
/// Every character before it only advances the cursor by one in the given state.
/// Returns `chars.len()` when no such character remains.
fn next_significant_index(
    line: &str,
    chars: &[(usize, char)],
    index: usize,
    state: &ScanState,
    syntax: &StringSyntax,
) -> usize {
    let Some((set, length)) = significant_bytes(state, syntax) else {
        return index;
    };
    let Some(&(start, _)) = chars.get(index) else {
        return index;
    };
    let rest = line.as_bytes().get(start..).unwrap_or_default();
    // The significant bytes are ASCII, so a match sits on a character boundary.
    crate::simd::find_byte_of(rest, set.get(..length).unwrap_or_default()).map_or(chars.len(), |offset| {
        let target = start + offset;
        index
            + chars
                .get(index..)
                .unwrap_or_default()
                .partition_point(|&(byte, _)| byte < target)
    })
}

/// Bytes that can change the given state, padded into a fixed array with the used length,
/// or `None` when every character needs a look.
fn significant_bytes(state: &ScanState, syntax: &StringSyntax) -> Option<([u8; 8], usize)> {
    match state {
        ScanState::Normal => {
            let [marker, block, double, single, backtick, heredoc, slash] = syntax.trigger_bytes();
            let raw = if syntax.rust_raw_strings { b'r' } else { slash };
            Some(([marker, block, double, single, backtick, heredoc, slash, raw], 8))
        }
        ScanState::InBlockComment(_) => syntax.block_comment.map(|(open, close)| {
            let close = first_byte_or_slash(close);
            let open = if syntax.nested_block_comments {
                first_byte_or_slash(open)
            } else {
                close
            };
            padded([close, open])
        }),
        ScanState::InString(quote) | ScanState::InTripleQuote(quote) => u8::try_from(*quote)
            .ok()
            .filter(u8::is_ascii)
            .map(|quote| padded([quote, b'\\'])),
        ScanState::InRawString(_) => Some(padded(*b"\"")),
        ScanState::InTemplate => Some(padded(*b"\\`")),
        ScanState::InBacktickRaw => Some(padded(*b"`")),
        ScanState::InHeredoc(_) | ScanState::InBlockScalar(_) | ScanState::Uncertain => None,
    }
}

/// The bytes copied into an eight byte array, with how many of them are used.
fn padded<const LENGTH: usize>(bytes: [u8; LENGTH]) -> ([u8; 8], usize) {
    let mut padded = [0u8; 8];
    for (slot, byte) in padded.iter_mut().zip(bytes) {
        *slot = byte;
    }
    (padded, LENGTH.min(8))
}

/// Whether the remaining line at the character index starts with `needle`.
pub(super) fn rest_starts_with(chars: &[(usize, char)], index: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, expected)| char_at(chars, index + offset) == Some(expected))
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
    let Some(character) = char_at(chars, *index) else {
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
            if character == '"' && (1..=hashes).all(|offset| char_at(chars, *index + offset) == Some('#')) {
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
    let index = cursor.index;
    let Some(&(byte, character)) = chars.get(index) else {
        return NormalStep::Continue(ScanState::Normal);
    };

    if rest_starts_with(chars, index, syntax.line_marker) {
        let previous = index.checked_sub(1).and_then(|position| char_at(chars, position));
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
        while char_at(chars, index + 1 + hashes) == Some('#') {
            hashes += 1;
        }
        cursor.index += 2 + hashes;
        return NormalStep::Continue(ScanState::InRawString(hashes));
    }
    if syntax.heredoc
        && character == '<'
        && char_at(chars, index + 1) == Some('<')
        && char_at(chars, index + 2) != Some('<')
    {
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

/// The character at the byte-indexed position, if any.
fn char_at(chars: &[(usize, char)], position: usize) -> Option<char> {
    chars.get(position).map(|(_, character)| *character)
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
    if char_at(chars, start + 1) == Some('\\') {
        let mut index = start + 2;
        while index < chars.len() && index < start + 12 {
            if char_at(chars, index) == Some('\'') {
                return index + 1;
            }
            index += 1;
        }
        return start + 1;
    }
    if char_at(chars, start + 2) == Some('\'') {
        return start + 3;
    }
    start + 1
}

/// Whether the `r` at `index` starts a Rust raw string such as `r"` or `br#"`.
fn is_raw_string_start(chars: &[(usize, char)], index: usize) -> bool {
    let mut offset = 1;
    while char_at(chars, index + offset) == Some('#') {
        offset += 1;
    }
    if char_at(chars, index + offset) != Some('"') {
        return false;
    }
    let is_identifier_char = |character: char| character.is_alphanumeric() || character == '_';
    match index.checked_sub(1).and_then(|position| char_at(chars, position)) {
        None => true,
        Some('b' | 'c') => index
            .checked_sub(2)
            .and_then(|position| char_at(chars, position))
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

#[cfg(test)]
mod test_trigger_prefilter {
    use std::path::Path;

    use super::super::file_kind::FileKind;
    use super::super::string_syntax::string_syntax;
    use super::*;

    /// Scan one line without skipping lines that hold no trigger byte, stepping over every character.
    fn scan_line_unfiltered(
        line: &str,
        state: ScanState,
        syntax: &StringSyntax,
        characters: &mut Vec<(usize, char)>,
    ) -> ScanResult {
        let mut state = state;
        if let Some(result) = line_level_state(line, &mut state) {
            return result;
        }
        scan_characters(line, state, syntax, characters, false)
    }

    /// Assert that both scans agree on every line of the text, carrying the state between lines.
    fn assert_same_scan(text: &str, kind: FileKind, label: &str) {
        let Some(syntax) = string_syntax(kind) else {
            return;
        };
        let mut characters = Vec::new();
        let mut filtered_state = ScanState::Normal;
        let mut unfiltered_state = ScanState::Normal;
        for (index, line) in text.lines().enumerate() {
            let filtered = scan_line_buffered(line, filtered_state, &syntax, &mut characters);
            let unfiltered = scan_line_unfiltered(line, unfiltered_state, &syntax, &mut characters);
            assert_eq!(filtered, unfiltered, "{label}:{} {line:?}", index + 1);
            filtered_state = filtered.state;
            unfiltered_state = unfiltered.state;
        }
    }

    #[test]
    fn skipping_lines_without_trigger_bytes_changes_no_repository_scan() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut scanned = 0;
        for directory in ["src", "benches", "tests", ".github", "."] {
            let depth = if directory == "." { 1 } else { usize::MAX };
            for entry in walkdir::WalkDir::new(root.join(directory))
                .max_depth(depth)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
            {
                let path = entry.path();
                let Some(kind) = FileKind::from_path(path) else {
                    continue;
                };
                let Ok(text) = std::fs::read_to_string(path) else {
                    continue;
                };
                assert_same_scan(&text, kind, &path.display().to_string());
                scanned += 1;
            }
        }
        assert!(scanned > 100, "only {scanned} files were scanned");
    }

    #[test]
    fn skipped_lines_still_open_block_scalars() {
        let text = "key: |\n  let value = 1\n  # not a comment\nnext: value # comment\n";
        assert_same_scan(text, FileKind::Yaml, "block scalar");
        let syntax = string_syntax(FileKind::Yaml).expect("YAML should have a string syntax");
        let result = scan_line("key: >-", ScanState::Normal, &syntax);
        assert_eq!(result.state, ScanState::InBlockScalar(0));
    }

    #[test]
    fn lines_with_only_plain_code_agree_for_every_kind() {
        let text = "let value = compute(input) + 1;\nvalue := other * 2 <- here\ncat <<EOF\nplain text\nEOF\n\
                    x = `echo` # tail\nfn run<'a>(value: &'a str) {} // note\n/* open\nstill open\n*/ done\n";
        for kind in [
            FileKind::Rust,
            FileKind::CLike,
            FileKind::JavaScript,
            FileKind::Go,
            FileKind::Python,
            FileKind::Shell,
            FileKind::Toml,
            FileKind::Yaml,
        ] {
            assert_same_scan(text, kind, &format!("{kind:?}"));
        }
    }
}
