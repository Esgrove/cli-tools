//! Prose token kind and the token built from tokenizing one line of prose.

use std::borrow::Cow;

/// Kind of a prose token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An ordinary word.
    Word,
    /// A backtick delimited code span.
    Code,
    /// A URL.
    Url,
    /// A file system path.
    Path,
    /// A version number such as "1.2.3".
    Version,
    /// A plain number.
    Number,
    /// An identifier such as `snake_case`, `camelCase`, or `a::b`.
    Identifier,
    /// A Markdown link or image.
    Link,
    /// An inline HTML tag.
    Html,
    /// A standalone em dash, en dash, or double hyphen.
    Dash,
}

/// A prose token: a word or an unbreakable atom with surrounding punctuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'text> {
    /// Opening punctuation such as `(` or `"`.
    pub leading: Cow<'text, str>,
    /// The word or atom body.
    pub core: Cow<'text, str>,
    /// Closing punctuation such as `.` or `,)`.
    pub trailing: Cow<'text, str>,
    /// Token classification.
    pub kind: TokenKind,
    /// Index of the paragraph line this token came from.
    pub origin_line: usize,
    /// Whether the formatter inserted a forced break after this token.
    pub force_break_after: bool,
    /// Width in characters, kept in step by the methods that change the text.
    width: u32,
}

impl<'text> Token<'text> {
    /// Create a token from the parts of one chunk of a line.
    #[must_use]
    pub fn new(
        leading: Cow<'text, str>,
        core: Cow<'text, str>,
        trailing: Cow<'text, str>,
        kind: TokenKind,
        origin_line: usize,
    ) -> Self {
        let width = token_width(&leading, &core, &trailing);
        Self {
            leading,
            core,
            trailing,
            kind,
            origin_line,
            force_break_after: false,
            width,
        }
    }

    /// Create a plain word token.
    #[must_use]
    pub fn word(core: &'text str, origin_line: usize) -> Self {
        Self::new(
            Cow::Borrowed(""),
            Cow::Borrowed(core),
            Cow::Borrowed(""),
            TokenKind::Word,
            origin_line,
        )
    }

    /// Replace the core text, for example with a capitalized word.
    pub fn set_core(&mut self, core: String) {
        self.core = Cow::Owned(core);
        self.width = token_width(&self.leading, &self.core, &self.trailing);
    }

    /// Add closing punctuation to the token.
    pub fn push_trailing(&mut self, character: char) {
        self.trailing.to_mut().push(character);
        self.width = token_width(&self.leading, &self.core, &self.trailing);
    }

    /// Replace the last character of the closing punctuation.
    pub fn replace_trailing_end(&mut self, character: char) {
        let trailing = self.trailing.to_mut();
        trailing.pop();
        trailing.push(character);
        self.width = token_width(&self.leading, &self.core, &self.trailing);
    }

    /// Full text of the token including surrounding punctuation.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}{}{}", self.leading, self.core, self.trailing)
    }

    /// Full text of the token, borrowing the core when there is no surrounding punctuation.
    #[must_use]
    pub fn text_cow(&self) -> Cow<'_, str> {
        if self.leading.is_empty() && self.trailing.is_empty() {
            Cow::Borrowed(&self.core)
        } else {
            Cow::Owned(self.text())
        }
    }

    /// Characters of the token in order, including surrounding punctuation.
    pub fn characters(&self) -> impl Iterator<Item = char> + '_ {
        self.leading
            .chars()
            .chain(self.core.chars())
            .chain(self.trailing.chars())
    }

    /// Width of the token in characters.
    ///
    /// The width is worked out once when the token is built,
    /// since it is asked for several times per token on every pass.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width as usize
    }

    /// Whether the token is an unbreakable atom whose inner punctuation carries no meaning.
    #[must_use]
    pub const fn is_atom(&self) -> bool {
        !matches!(self.kind, TokenKind::Word | TokenKind::Dash)
    }

    /// Whether the token ends with sentence punctuation, ignoring closing brackets and quotes.
    #[must_use]
    pub fn ends_sentence_punctuation(&self) -> bool {
        let trimmed = self.trailing.trim_end_matches(is_closer);
        trimmed.ends_with(['.', '!', '?'])
    }

    /// Whether the token ends with clause punctuation such as a comma, colon, semicolon, or dash.
    #[must_use]
    pub fn ends_clause_punctuation(&self) -> bool {
        if self.kind == TokenKind::Dash {
            return true;
        }
        let trimmed = self.trailing.trim_end_matches(is_closer);
        trimmed.ends_with([',', ':', ';', '—', '–'])
    }

    /// Whether the token ends with an opening bracket or quote.
    #[must_use]
    pub fn ends_with_opener(&self) -> bool {
        self.trailing
            .chars()
            .next_back()
            .or_else(|| self.core.chars().next_back())
            .or_else(|| self.leading.chars().next_back())
            .is_some_and(is_opener)
    }

    /// First visible character of the token.
    #[must_use]
    pub fn first_char(&self) -> Option<char> {
        self.leading.chars().next().or_else(|| self.core.chars().next())
    }
}

/// Width of the text in characters, counting bytes when every one of them is ASCII.
fn text_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        text.chars().count()
    }
}

/// Width of the three parts of a token in characters.
fn token_width(leading: &str, core: &str, trailing: &str) -> u32 {
    let width = text_width(leading) + text_width(core) + text_width(trailing);
    u32::try_from(width).unwrap_or(u32::MAX)
}

/// Whether the character closes a bracket or quote.
#[must_use]
pub(super) const fn is_closer(character: char) -> bool {
    matches!(
        character,
        ')' | ']' | '}' | '"' | '\'' | '”' | '’' | '»' | '`' | '*' | '_'
    )
}

/// Whether the character opens a bracket or quote.
#[must_use]
pub(super) const fn is_opener(character: char) -> bool {
    matches!(character, '(' | '[' | '{' | '"' | '\'' | '“' | '‘' | '«')
}

#[cfg(test)]
mod test_token_builders {
    use super::*;

    /// Build a token with the given punctuation around a word.
    pub fn wrapped(leading: &'static str, core: &'static str, trailing: &'static str) -> Token<'static> {
        Token::new(
            Cow::Borrowed(leading),
            Cow::Borrowed(core),
            Cow::Borrowed(trailing),
            TokenKind::Word,
            0,
        )
    }
}

#[cfg(test)]
mod test_token {
    use super::*;

    #[test]
    fn width_and_text_include_punctuation() {
        let token = Token::new(
            Cow::Borrowed("("),
            Cow::Borrowed("ääkkönen"),
            Cow::Borrowed(")."),
            TokenKind::Word,
            0,
        );
        assert_eq!(token.text(), "(ääkkönen).");
        assert_eq!(token.width(), 11);
        assert_eq!(
            token.width(),
            token.characters().count(),
            "the cached width has to match"
        );
        assert!(token.ends_sentence_punctuation());
        assert!(!token.ends_clause_punctuation());
    }

    #[test]
    fn clause_punctuation_ignores_closers() {
        let mut token = Token::word("value", 0);
        token.push_trailing(',');
        token.push_trailing('"');
        assert_eq!(token.trailing, ",\"");
        assert!(token.ends_clause_punctuation());
        assert!(!super::test_token_builders::wrapped("", "value", "\")").ends_clause_punctuation());
    }

    #[test]
    fn the_cached_width_follows_every_change() {
        let mut token = Token::word("value", 0);
        assert_eq!(token.width(), 5);
        token.push_trailing('.');
        assert_eq!(token.width(), 6);
        assert_eq!(token.width(), token.characters().count());
        token.set_core("ääkkönen".to_string());
        assert_eq!(token.width(), 9);
        assert_eq!(token.width(), token.characters().count());
        token.replace_trailing_end('!');
        assert_eq!(token.width(), token.characters().count());
    }
}

#[cfg(test)]
mod test_token_helpers {
    use super::*;

    #[test]
    fn text_cow_borrows_a_bare_core() {
        let bare = Token::word("value", 0);
        assert!(matches!(bare.text_cow(), Cow::Borrowed("value")));
        assert_eq!(bare.text(), "value");

        let wrapped = super::test_token_builders::wrapped("(", "value", ").");
        assert!(matches!(wrapped.text_cow(), Cow::Owned(_)));
        assert_eq!(wrapped.text_cow(), "(value).");
    }

    #[test]
    fn characters_yield_the_whole_token_in_order() {
        let token = super::test_token_builders::wrapped("[", "value", "],");
        assert_eq!(token.characters().collect::<String>(), "[value],");
        assert_eq!(token.width(), 8);
    }

    #[test]
    fn first_char_falls_back_to_the_core() {
        let bare = Token::word("word", 3);
        assert_eq!(bare.first_char(), Some('w'));
        assert_eq!(bare.origin_line, 3);

        let wrapped = super::test_token_builders::wrapped("\"", "word", "");
        assert_eq!(wrapped.first_char(), Some('"'));

        let empty = Token::word("", 0);
        assert_eq!(empty.first_char(), None);
    }

    #[test]
    fn ends_with_opener_checks_the_last_character_of_the_token() {
        let token = Token::word("call", 0);
        assert!(!token.ends_with_opener());
        assert!(super::test_token_builders::wrapped("", "call", "(").ends_with_opener());

        assert!(Token::word("(", 0).ends_with_opener());
        assert!(super::test_token_builders::wrapped("[", "", "").ends_with_opener());
    }

    #[test]
    fn atoms_are_every_kind_except_words_and_dashes() {
        let kinds = [
            (TokenKind::Word, false),
            (TokenKind::Dash, false),
            (TokenKind::Code, true),
            (TokenKind::Url, true),
            (TokenKind::Path, true),
            (TokenKind::Version, true),
            (TokenKind::Number, true),
            (TokenKind::Identifier, true),
            (TokenKind::Link, true),
            (TokenKind::Html, true),
        ];
        for (kind, is_atom) in kinds {
            let token = Token::new(Cow::Borrowed(""), Cow::Borrowed("x"), Cow::Borrowed(""), kind, 0);
            assert_eq!(token.is_atom(), is_atom, "{kind:?}");
        }
    }
}

#[cfg(test)]
mod test_openers_and_closers {
    use super::*;

    #[test]
    fn openers_and_closers_are_recognized() {
        for character in ['(', '[', '{', '"', '\'', '“', '‘', '«'] {
            assert!(is_opener(character), "{character} should open");
        }
        for character in [')', ']', '}', '"', '\'', '”', '’', '»', '`', '*', '_'] {
            assert!(is_closer(character), "{character} should close");
        }
        assert!(!is_opener('a'));
        assert!(!is_closer('a'));
    }
}
