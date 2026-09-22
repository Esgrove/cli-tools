//! Tokenizing of a prose line for the semantic line breaks formatter.
//!
//! Splits a line into words and into unbreakable atoms
//! such as inline code spans, links, URLs, file names, version numbers, and identifiers,
//! and classifies every token so the boundary detection can read the line.
//! The token text is borrowed from the line, so tokenizing allocates only for a reworded token.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use super::options::FormatOptions;
use super::token::{Token, TokenKind, is_closer, is_opener};

/// The em dash, which the dash rewrite reads as a clause separator.
const EM_DASH: char = '\u{2014}';

/// ASCII whitespace and the lead bytes of every other whitespace character and of the em dash.
const CHUNK_END_CANDIDATES: &[u8] = b" \t\n\x0b\x0c\r\xc2\xe1\xe2\xe3";

/// Matches a URL such as `https://example.com/path` or `www.example.com`.
static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:[a-z][a-z0-9+.-]*://\S+|www\.\S+)$").expect("Invalid URL regex"));

/// Matches a file name with an extension such as `lib.rs`.
static RE_FILE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.-]+\.[A-Za-z0-9]{1,5}$").expect("Invalid file name regex"));

/// Matches a version number such as `1.2.3` or `v2.0-beta`.
static RE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?\d+(?:\.\d+){1,3}(?:[-+][\w.]+)?$").expect("Invalid version regex"));

/// Matches a plain number such as `42`, `3.14`, or `50%`.
static RE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[+-]?\d[\d_,]*(?:\.\d+)?%?$").expect("Invalid number regex"));

/// Matches identifiers: `snake_case`, `path::segments`, `camelCase`, `SCREAMING_CASE`,
/// and calls like `foo()`.
static RE_IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\w*_\w*|\w+(?:::\w+)+|[a-z]+[A-Z]\w*|[\w:.]+\(.*\))$").expect("Invalid identifier regex")
});

/// Matches dotted letter abbreviations such as `e.g` or `i.e` that would otherwise look like file names.
static RE_LETTER_ABBREVIATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[A-Za-z]\.)+[A-Za-z]$").expect("Invalid abbreviation regex"));

/// Matches a plain lowercase word that can safely be capitalized.
pub(super) static RE_LOWERCASE_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z']*(?:-[a-z][a-z']*)*$").expect("Invalid lowercase word regex"));

/// Matches a token that would start a Markdown list item or heading if placed at a line start.
static RE_MARKDOWN_STRUCTURE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[-+*>|]|#+|\d+[.)]|```.*|~~~.*)$").expect("Invalid Markdown structure regex"));

/// Tokenize one content line into words and atoms.
///
/// When `normalize_dashes` is set, em dashes attached to words are separated into standalone dash tokens
/// so the em dash rewrite can handle them uniformly.
#[must_use]
pub fn tokenize_line(content: &str, origin_line: usize, normalize_dashes: bool) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut position = 0;
    while position < content.len() {
        let rest = content.get(position..).unwrap_or_default();
        let Some(character) = rest.chars().next() else {
            break;
        };
        if character.is_whitespace() {
            position += character.len_utf8();
            continue;
        }
        // An em dash stands on its own, so the dash rewrite sees it whatever it was written against.
        let end = if normalize_dashes && character == EM_DASH {
            position + EM_DASH.len_utf8()
        } else {
            chunk_end(content, position, normalize_dashes)
        };
        let (token, next) = read_token(content, position, end, origin_line);
        tokens.push(token);
        position = next.max(position + character.len_utf8());
    }
    demote_command_separators(&mut tokens);
    tokens
}

/// Whether the character may be peeled off the end of a chunk as closing punctuation.
pub(super) const fn is_trailing_closer(character: char) -> bool {
    is_closer(character) || matches!(character, '.' | ',' | ';' | ':' | '!' | '?' | '…' | '—' | '–')
}

/// Whether the word list contains the word, ignoring ASCII case.
pub(super) fn contains_word(words: &[&str], word: &str) -> bool {
    words.iter().any(|candidate| candidate.eq_ignore_ascii_case(word))
}

/// Whether the word is a configured abbreviation that never ends a sentence.
pub(super) fn is_abbreviation(core: &str, options: &FormatOptions) -> bool {
    if core.is_ascii() {
        return options.abbreviations.iter().any(|abbreviation| {
            abbreviation
                .strip_suffix('.')
                .is_some_and(|word| word.eq_ignore_ascii_case(core))
        });
    }
    let lowercase = format!("{}.", core.to_lowercase());
    options.abbreviations.contains(&lowercase)
}

/// Whether placing the token at a line start would change Markdown structure.
pub(super) fn starts_markdown_structure(token: &Token<'_>) -> bool {
    let text = token.text_cow();
    let can_be_structure = text
        .starts_with(|character: char| matches!(character, '-' | '+' | '*' | '>' | '|' | '#' | '`' | '~' | '0'..='9'));
    can_be_structure && (RE_MARKDOWN_STRUCTURE.is_match(&text) || text.starts_with('#'))
        || text.chars().all(is_trailing_closer)
}

/// Turn a double hyphen that precedes a command line flag into a plain word.
///
/// A line such as `pnpm run migrate -- --env dev` uses the double hyphen to separate arguments,
/// so rewriting it as a dash would corrupt the command.
fn demote_command_separators(tokens: &mut [Token<'_>]) {
    for index in 0..tokens.len() {
        let next_is_flag = tokens
            .get(index + 1)
            .is_some_and(|next| next.core.starts_with('-') || next.leading.starts_with('-'));
        if !next_is_flag {
            continue;
        }
        if let Some(token) = tokens.get_mut(index)
            && token.kind == TokenKind::Dash
        {
            token.kind = TokenKind::Word;
        }
    }
}

/// Byte index one past the last non-whitespace character of the chunk starting at `start`.
///
/// An em dash ends the chunk when the dash rewrite is on,
/// so a dash written against a word becomes a token of its own.
fn chunk_end(text: &str, start: usize, split_dashes: bool) -> usize {
    let rest = text.get(start..).unwrap_or_default();
    let mut offset = 0;
    while let Some(found) = rest
        .as_bytes()
        .get(offset..)
        .and_then(|tail| crate::simd::find_byte_of(tail, CHUNK_END_CANDIDATES))
    {
        let position = offset + found;
        // Candidates are ASCII or lead bytes, so the position is always a character boundary.
        let character = rest.get(position..).and_then(|tail| tail.chars().next());
        if character.is_some_and(|character| character.is_whitespace() || split_dashes && character == EM_DASH) {
            return start + position;
        }
        offset = position + 1;
    }
    text.len()
}

/// Read one token starting at `start`, returning the token and the byte index where it ends.
fn read_token(text: &str, start: usize, word_end: usize, origin_line: usize) -> (Token<'_>, usize) {
    let mut leading_end = start;
    while leading_end < word_end {
        let chunk = text.get(leading_end..word_end).unwrap_or_default();
        let Some(character) = chunk.chars().next() else {
            break;
        };
        // A marker opens emphasis only when the same one appears again in the chunk.
        let is_emphasis = matches!(character, '*' | '_')
            && chunk
                .get(character.len_utf8()..)
                .is_some_and(|tail| tail.contains(character));
        if atom_at(text, leading_end).is_some() {
            break;
        }
        if is_opener(character) || is_emphasis {
            leading_end += character.len_utf8();
        } else {
            break;
        }
    }
    if leading_end >= word_end {
        leading_end = start;
    }
    let leading = text.get(start..leading_end).unwrap_or_default();

    if let Some((atom_end, kind)) = atom_at(text, leading_end) {
        // The atom and the punctuation after it are one run of the line, so the core is one slice of it.
        let tail_end = chunk_end(text, atom_end, false);
        let tail = text.get(atom_end..tail_end).unwrap_or_default();
        let trailing_start = atom_end + trailing_punctuation_start(tail, 0);
        let core = text.get(leading_end..trailing_start).unwrap_or_default();
        let trailing = text.get(trailing_start..tail_end).unwrap_or_default();
        let token = Token::new(
            Cow::Borrowed(leading),
            Cow::Borrowed(core),
            Cow::Borrowed(trailing),
            kind,
            origin_line,
        );
        return (token, tail_end);
    }

    let chunk = text.get(leading_end..word_end).unwrap_or_default();
    let keep = chunk.chars().next().map_or(0, char::len_utf8);
    let core_end = trailing_punctuation_start(chunk, keep);
    let core = chunk.get(..core_end).unwrap_or_default();
    let trailing = chunk.get(core_end..).unwrap_or_default();
    let kind = classify_core(core, leading.is_empty() && trailing.is_empty());
    let token = Token::new(
        Cow::Borrowed(leading),
        Cow::Borrowed(core),
        Cow::Borrowed(trailing),
        kind,
        origin_line,
    );
    (token, word_end)
}

/// Byte offset where the closing punctuation at the end of the chunk starts.
///
/// At least `keep` bytes are left in front of it, so a token is never all punctuation,
/// and a closing bracket that belongs to an unclosed one inside the chunk stays in the core.
fn trailing_punctuation_start(chunk: &str, keep: usize) -> usize {
    let mut end = chunk.len();
    while end > keep {
        let Some((offset, character)) = chunk.get(..end).and_then(|head| head.char_indices().next_back()) else {
            break;
        };
        if !is_trailing_closer(character) {
            break;
        }
        if matches!(character, ')' | ']') && has_unclosed_bracket(chunk.get(..offset).unwrap_or_default()) {
            break;
        }
        end = offset;
    }
    end
}

/// Whether the text holds more opening than closing brackets.
fn has_unclosed_bracket(text: &str) -> bool {
    let depth = text.bytes().fold(0i64, |depth, byte| match byte {
        b'(' | b'[' => depth + 1,
        b')' | b']' => depth - 1,
        _ => depth,
    });
    depth > 0
}

/// Detect an unbreakable atom starting at `start`, returning its end byte index and kind.
fn atom_at(text: &str, start: usize) -> Option<(usize, TokenKind)> {
    let rest = text.get(start..)?;
    let bytes = rest.as_bytes();
    match *bytes.first()? {
        b'`' => backtick_span_end(text, start).map(|end| (end, TokenKind::Code)),
        b'[' => link_end(text, start).map(|end| (end, TokenKind::Link)),
        b'!' if bytes.get(1) == Some(&b'[') => link_end(text, start + 1).map(|end| (end, TokenKind::Link)),
        b'<' => {
            let next = *bytes.get(1)?;
            if next.is_ascii_alphabetic() || matches!(next, b'/' | b'!') {
                let end = rest.find('>').map(|offset| start + offset + 1)?;
                Some((end, TokenKind::Html))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// End byte index of a backtick code span starting at `start`, matching runs of equal length.
fn backtick_span_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let run = backtick_run(bytes, start);
    let mut index = start + run;
    while index < bytes.len() {
        if bytes.get(index) == Some(&b'`') {
            let closing = backtick_run(bytes, index);
            if closing == run {
                return Some(index + closing);
            }
            index += closing;
        } else {
            index += 1;
        }
    }
    None
}

/// Number of backticks in a row at `start`.
fn backtick_run(bytes: &[u8], start: usize) -> usize {
    let mut run = 0;
    while bytes.get(start + run) == Some(&b'`') {
        run += 1;
    }
    run
}

/// End byte index of a Markdown link or image starting at the `[` at `start`.
fn link_end(text: &str, start: usize) -> Option<usize> {
    let close_bracket = matching_close(text, start, b'[', b']')?;
    match text.as_bytes().get(close_bracket + 1) {
        Some(b'(') => matching_close(text, close_bracket + 1, b'(', b')').map(|end| end + 1),
        Some(b'[') => matching_close(text, close_bracket + 1, b'[', b']').map(|end| end + 1),
        _ => None,
    }
}

/// Byte index of the bracket closing the one at `start`, counting nesting.
fn matching_close(text: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for index in start..bytes.len() {
        let byte = *bytes.get(index)?;
        if byte == open {
            depth += 1;
        } else if byte == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Classify the core text of a chunk that is not a delimited atom.
fn classify_core(core: &str, bare: bool) -> TokenKind {
    if bare && matches!(core, "—" | "–" | "--") {
        return TokenKind::Dash;
    }
    let has_dot = core.contains('.');
    let has_colon = core.contains(':');
    let starts_with_number =
        core.starts_with(|character: char| character.is_ascii_digit() || character == '+' || character == '-');
    if has_dot && RE_LETTER_ABBREVIATION.is_match(core) {
        return TokenKind::Word;
    }
    if (has_colon || has_dot) && RE_URL.is_match(core) {
        return TokenKind::Url;
    }
    if starts_with_number && RE_NUMBER.is_match(core) {
        return TokenKind::Number;
    }
    if has_dot && (starts_with_number || core.starts_with('v')) && RE_VERSION.is_match(core) {
        return TokenKind::Version;
    }
    if core.contains('/') && !core.contains("//") || has_dot && RE_FILE_NAME.is_match(core) {
        return TokenKind::Path;
    }
    if (core.contains('_') || has_colon || core.contains('(') || core.as_bytes().iter().any(u8::is_ascii_uppercase))
        && RE_IDENTIFIER.is_match(core)
    {
        return TokenKind::Identifier;
    }
    TokenKind::Word
}

#[cfg(test)]
mod test_tokenizer {
    use super::super::reflow::{join_tokens, prefix_width, tokens_width};
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn an_unterminated_code_span_is_not_an_atom() {
        assert_eq!(
            kinds("run `unterminated code here"),
            vec![TokenKind::Word, TokenKind::Word, TokenKind::Word, TokenKind::Word,]
        );
    }

    #[test]
    fn a_reference_style_link_is_one_atom() {
        assert_eq!(
            kinds("see [the docs][docs] now"),
            vec![TokenKind::Word, TokenKind::Link, TokenKind::Word]
        );
    }

    #[test]
    fn an_unbalanced_bracket_is_not_a_link() {
        assert_eq!(
            kinds("see [the docs now"),
            vec![TokenKind::Word, TokenKind::Word, TokenKind::Word, TokenKind::Word,]
        );
    }

    #[test]
    fn the_width_of_no_tokens_is_zero() {
        assert_eq!(tokens_width(&[]), 0);
        assert!(join_tokens(&[]).is_empty());
    }

    #[test]
    fn a_code_span_with_spaces_stays_one_atom() {
        let tokens = tokens("run `a; b.c() and more` now");
        assert_eq!(
            kinds("run `a; b.c() and more` now"),
            vec![TokenKind::Word, TokenKind::Code, TokenKind::Word]
        );
        assert_eq!(tokens.get(1).map(Token::text), Some("`a; b.c() and more`".to_string()));
    }

    #[test]
    fn keeps_code_span_as_one_atom() {
        let tokens = tokens("call `a; b.c()` now");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].kind, TokenKind::Code);
        assert_eq!(tokens[1].core, "`a; b.c()`");
    }

    #[test]
    fn keeps_double_backtick_span_with_inner_backtick() {
        let tokens = tokens("x ``a ` b`` y");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].kind, TokenKind::Code);
        assert_eq!(tokens[1].core, "``a ` b``");
    }

    #[test]
    fn code_span_keeps_following_punctuation_in_trailing() {
        let tokens = tokens("(`num_cpus * 2`).");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].leading, "(");
        assert_eq!(tokens[0].core, "`num_cpus * 2`");
        assert_eq!(tokens[0].trailing, ").");
        assert_eq!(tokens[0].text(), "(`num_cpus * 2`).");
    }

    #[test]
    fn url_keeps_inner_dots_and_peels_final_period() {
        let tokens = tokens("See https://a.b/c.");
        assert_eq!(tokens[1].kind, TokenKind::Url);
        assert_eq!(tokens[1].core, "https://a.b/c");
        assert_eq!(tokens[1].trailing, ".");
    }

    #[test]
    fn classifies_paths_versions_numbers_and_identifiers() {
        assert_eq!(
            kinds("Edit src/lib.rs and bump 1.2.3."),
            vec![
                TokenKind::Word,
                TokenKind::Path,
                TokenKind::Word,
                TokenKind::Word,
                TokenKind::Version
            ]
        );
        assert_eq!(kinds("main.c"), vec![TokenKind::Path]);
        assert_eq!(kinds("42 3.14 50%"), vec![TokenKind::Number; 3]);
        assert_eq!(
            kinds("snake_case std::fs camelCase foo()"),
            vec![TokenKind::Identifier; 4]
        );
        assert_eq!(kinds("e.g. i.e."), vec![TokenKind::Word, TokenKind::Word]);
    }

    #[test]
    fn markdown_link_with_nested_parens_is_one_atom() {
        let tokens = tokens("see [x](https://a/b_(c)) now");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[1].kind, TokenKind::Link);
        assert_eq!(tokens[1].core, "[x](https://a/b_(c))");
    }

    #[test]
    fn peels_leading_and_trailing_punctuation() {
        let tokens = tokens("(the \"value\").");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].leading, "(");
        assert_eq!(tokens[0].core, "the");
        assert_eq!(tokens[1].leading, "\"");
        assert_eq!(tokens[1].core, "value");
        assert_eq!(tokens[1].trailing, "\").");
    }

    #[test]
    fn recognizes_dash_tokens() {
        assert_eq!(kinds("a — b"), vec![TokenKind::Word, TokenKind::Dash, TokenKind::Word]);
        assert_eq!(kinds("a—b"), vec![TokenKind::Word, TokenKind::Dash, TokenKind::Word]);
        assert_eq!(kinds("a -- b"), vec![TokenKind::Word, TokenKind::Dash, TokenKind::Word]);
        assert_eq!(kinds("--flag"), vec![TokenKind::Word]);
        assert_eq!(kinds("pages 1–5"), vec![TokenKind::Word, TokenKind::Word]);
    }

    #[test]
    fn emphasis_markers_are_peeled_only_when_paired() {
        let tokens = tokens("*strong* _x");
        assert_eq!(tokens[0].leading, "*");
        assert_eq!(tokens[0].core, "strong");
        assert_eq!(tokens[0].trailing, "*");
        assert_eq!(tokens[1].leading, "");
        assert_eq!(tokens[1].core, "_x");
    }

    #[test]
    fn width_counts_characters_not_bytes() {
        let umlauts = tokens("ääää");
        assert_eq!(umlauts[0].width(), 4);
        assert_eq!(tokens_width(&umlauts), 4);
        assert_eq!(tokens_width(&tokens("ab cd")), 5);
        assert_eq!(prefix_width("\t// ", 4), 7);
    }
}

#[cfg(test)]
mod test_chunk_end {
    use super::*;

    /// The character by character scan `chunk_end` must agree with.
    fn chunk_end_by_characters(text: &str, start: usize, split_dashes: bool) -> usize {
        let rest = text.get(start..).unwrap_or_default();
        rest.char_indices()
            .find(|(_, character)| character.is_whitespace() || split_dashes && *character == EM_DASH)
            .map_or(text.len(), |(offset, _)| start + offset)
    }

    #[test]
    fn agrees_with_the_character_scan_on_every_start() {
        let lines = [
            "plain words separated by spaces",
            "tabs\tand\u{a0}no-break\u{2009}thin\u{3000}ideographic\u{1680}ogham\u{85}next line",
            "dash—joined words – en dash “quoted” ’apostrophe’ … ellipsis €uro ‘single’",
            "a_very_long_identifier_without_any_whitespace_that_runs_past_one_vector_of_bytes_and_more",
            "",
            "trailing space ",
        ];
        for line in lines {
            for (start, _) in line.char_indices() {
                for split_dashes in [false, true] {
                    assert_eq!(
                        chunk_end(line, start, split_dashes),
                        chunk_end_by_characters(line, start, split_dashes),
                        "{line:?} from {start} split {split_dashes}"
                    );
                }
            }
        }
    }
}
