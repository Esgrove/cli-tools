//! Heuristic that treats a content line as source code rather than prose.
//!
//! Used by the Markdown splitter to keep code-like paragraphs verbatim,
//! so a line that only looks like code is not reflowed as if it were a sentence.

use std::sync::LazyLock;

use regex::Regex;

/// Matches a backtick code span for removal before code detection.
static RE_CODE_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`]*`").expect("Invalid code span regex"));

/// Matches a call expression that fills the whole line.
static RE_CALL_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w.:]+\(.*\)\s*[;:,]?$").expect("Invalid call regex"));

/// Keywords and symbols that mark a line starting with them as code when the line also looks like code otherwise.
const CODE_KEYWORDS: &[&str] = &[
    "let",
    "fn",
    "use",
    "impl",
    "pub",
    "mod",
    "struct",
    "enum",
    "import",
    "from",
    "def",
    "class",
    "const",
    "static",
    "return",
    "println!",
    "eprintln!",
    "assert!",
    "assert_eq!",
    "dbg!",
    "print",
    "self.",
    "$ ",
    "#[",
    "#!",
    "//",
    "#",
];

/// Matches a line that starts with a code keyword.
static RE_CODE_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    let alternatives: Vec<String> = CODE_KEYWORDS.iter().map(|keyword| regex::escape(keyword)).collect();
    Regex::new(&format!(r"^(?:{})\b", alternatives.join("|"))).expect("Invalid keyword regex")
});

/// Matches a long command line flag such as `--env`, which marks the line as a command.
static RE_COMMAND_FLAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)--[A-Za-z][\w-]*").expect("Invalid command flag regex"));

/// Whether a content line looks like source code rather than prose.
#[must_use]
pub fn looks_like_code(line: &str) -> bool {
    let stripped = RE_CODE_SPAN.replace_all(line, "");
    let text = stripped.trim();
    if text.is_empty() {
        return false;
    }
    if text.ends_with(['{', '}']) || ((text.ends_with(");") || text.ends_with("),")) && !is_parenthetical_aside(text)) {
        return true;
    }
    let has_code_characters = crate::simd::contains_byte_of(text.as_bytes(), b"=({") || text.contains("::");
    if text.ends_with(';') && has_code_characters {
        return true;
    }
    if (has_code_characters || text.ends_with(';') || text.starts_with(['#', '/', '$']))
        && RE_CODE_KEYWORD.is_match(text)
    {
        return true;
    }
    if text.contains("--") && RE_COMMAND_FLAG.is_match(text) {
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
    text.contains('(') && RE_CALL_LINE.is_match(text)
}

/// Whether the line is one bracketed aside, opened by its first character and closed by the bracket that ends it.
///
/// A reflowed sentence puts such an aside on a line of its own,
/// so its closing bracket and the punctuation after it do not mark a call the code makes.
fn is_parenthetical_aside(text: &str) -> bool {
    let Some(body) = text
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix([';', ',']))
        .and_then(|inner| inner.strip_suffix(')'))
    else {
        return false;
    };
    let mut depth = 0_usize;
    for byte in body.bytes() {
        match byte {
            b'(' => depth += 1,
            b')' if depth == 0 => return false,
            b')' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
mod test_looks_like_code {
    use super::*;

    #[test]
    fn a_command_line_flag_marks_the_line_as_code() {
        assert!(looks_like_code(
            "pnpm exec tsx scripts/import.ts --env dev --file data.csv"
        ));
        assert!(looks_like_code("cargo build --release"));
        assert!(!looks_like_code("Use the `--fix` option to rewrite the files"));
        assert!(!looks_like_code("the value is set -- always"));
    }

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
        assert!(!looks_like_code(
            "(the first backend, the editor preview, older engines),"
        ));
        assert!(!looks_like_code("(the editor preview (and its tests)),"));
        assert!(looks_like_code("(a + b);"));
        assert!(looks_like_code("foo(bar),"));
        assert!(looks_like_code("(a) foo(bar),"));
        assert!(looks_like_code("(void)close(handle),"));
        assert!(!looks_like_code("If the file exists, skip it."));
        assert!(!looks_like_code(""));
        assert!(!looks_like_code("   "));
    }
}

#[cfg(test)]
mod test_fixture_coverage {
    use super::*;
    use crate::semantic_line_breaks::test_helpers::*;

    #[test]
    fn every_code_keyword_starts_a_line_of_a_fixture() {
        assert_fixture_lines_start_with(CODE_KEYWORDS, "CODE_KEYWORDS");
    }
}
