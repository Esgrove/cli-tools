//! Module docs that were hard wrapped at eighty columns by an editor,
//! which is exactly the kind of text the formatter is supposed to repair.
//!
//! # Examples
//!
//! ```
//! let x = 1; // code stays
//! ```

/// Parse the header.
/// The caller handles errors, always.
/// Returns the parsed header struct.
///
/// - first item that wraps early
/// - second item.
///
/// # Errors
/// Returns an error when the input is empty.
pub fn parse(input: &str) -> Header {
    // Use the default width for now.
    // default width
    let width = 80;
    // not a URL comment
    let url = "http://example.com";
    Header::new(input, width) // clippy::allow
}
