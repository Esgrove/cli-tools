//! Formatting shared by the build script and the library.
//!
//! The build script includes this file with a `#[path]` module,
//! so it uses only the standard library.

/// Join the non-empty values with single spaces.
///
/// Values that could not be read at build time are empty strings,
/// and skipping them keeps the joined line free of double and trailing spaces.
pub fn join_present(values: &[&str]) -> String {
    values
        .iter()
        .filter(|value| !value.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}
