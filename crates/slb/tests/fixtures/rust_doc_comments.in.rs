//! Module docs that were hard wrapped at eighty
//! columns by an editor, which is exactly the kind
//! of text the formatter is supposed to repair.
//!
//! # Examples
//!
//! ```
//! let x = 1; // code stays
//! ```

/// Parse the header; the caller handles errors — always.
/// Returns the parsed
/// header struct.
///
/// - first item that
///   wraps early
/// - second item.
///
/// # Errors
/// Returns an error when the input is empty.
pub fn parse(input: &str) -> Header {
    // Use the default width
    // for now.
    let width = 80; // default width
    let url = "http://example.com"; // not a URL comment
    Header::new(input, width) // clippy::allow
}

/// Escaped quote character literals must not invert string state; otherwise the multiline string
/// below would look like regular code and its comment-like lines would be rewritten.
pub fn is_quote_and_banner(character: char) -> (bool, &'static str) {
    let is_quote = matches!(character, '\'' | '"');
    let banner = "
// Inside a Rust multiline string literal; semicolons must stay untouched.
// Wrapped lines inside the string
// remain verbatim.
";
    (is_quote, banner)
}

/// Nested Rust block comments track depth across lines; closing only the inner block must not
/// expose the remaining lines inside the outer comment to line-comment formatting.
pub fn nested_block_comment_example(items: &[u32]) -> usize {
    let mut total = 0;
    for &item in items {
        /* outer block comment /* inner block comment */
        // closes outer block comment */ total += item; continue;
    }
    let kept = total; /// preserve triple-slash doc marker when relocated
    kept as usize
}
