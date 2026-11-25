//! Ignored prefix and ignored group name rules applied to file and directory names.
//!
//! `NameFilter` strips configured prefixes from names,
//! and decides whether a directory name is too short, numeric, or ignored to be a destination or a group.

use std::borrow::Cow;
use std::path::PathBuf;

use crate::types::DirectoryInfo;
use crate::utils;

/// Minimum number of normalized characters a destination or group name needs.
const MIN_DIRECTORY_NAME_CHARS: usize = 4;

/// Ignored prefix and ignored group name rules borrowed from a `Config`.
#[derive(Debug, Clone, Copy)]
pub struct NameFilter<'a> {
    /// Lowercase prefixes stripped from the start of names.
    prefix_ignores: &'a [String],
    /// Normalized group names that are never offered.
    ignored_group_names: &'a [String],
}

impl<'a> NameFilter<'a> {
    /// Create a filter from lowercase prefix ignores and normalized ignored group names.
    #[must_use]
    pub const fn new(prefix_ignores: &'a [String], ignored_group_names: &'a [String]) -> Self {
        Self {
            prefix_ignores,
            ignored_group_names,
        }
    }

    /// Return whether any prefix ignores are configured.
    #[must_use]
    pub const fn has_prefix_ignores(&self) -> bool {
        !self.prefix_ignores.is_empty()
    }

    /// Strip ignored prefixes followed by a dot from the start of a file name.
    ///
    /// Prefixes are removed repeatedly until none match, comparing case-insensitively.
    #[must_use]
    pub fn strip_ignored_dot_prefixes(&self, filename: &str) -> String {
        strip_prefixes_with_separator(filename, self.prefix_ignores, '.').into_owned()
    }

    /// Strip ignored prefixes from a normalized filename (spaces as separators).
    /// Recursively removes any matching prefix from the start of the filename.
    /// Comparison is case-insensitive.
    #[must_use]
    pub fn strip_ignored_prefixes<'name>(&self, filename: &'name str) -> Cow<'name, str> {
        strip_prefixes_with_separator(filename, self.prefix_ignores, ' ')
    }

    /// Check whether an existing destination directory should be skipped.
    ///
    /// `directory_name_normalized` must be precomputed by the caller to avoid repeated normalization
    /// while filtering the full destination directory list.
    #[must_use]
    pub fn should_skip_destination_directory(&self, directory_name: &str, directory_name_normalized: &str) -> bool {
        if self.is_unusable_normalized_name(directory_name_normalized) {
            return true;
        }

        let stripped_name = self.strip_ignored_prefixes(directory_name);
        if stripped_name.as_ref() == directory_name {
            return false;
        }

        self.is_unusable_normalized_name(&utils::normalize_name(stripped_name.as_ref()))
    }

    /// Split destination directories into the ones to keep and the paths of the ignored ones.
    ///
    /// A directory is ignored when `should_skip_destination_directory` skips its name.
    #[must_use]
    pub fn partition_destination_directories(
        &self,
        directories: Vec<DirectoryInfo>,
    ) -> (Vec<DirectoryInfo>, Vec<PathBuf>) {
        let mut kept = Vec::new();
        let mut ignored = Vec::new();
        for directory in directories {
            let normalized_name = utils::normalize_name(&directory.name);
            if self.should_skip_destination_directory(&directory.name, &normalized_name) {
                ignored.push(directory.path);
            } else {
                kept.push(directory);
            }
        }
        (kept, ignored)
    }

    /// Check whether a normalized name matches a configured ignored group name.
    #[must_use]
    pub fn matches_ignored_group_name(&self, normalized_name: &str) -> bool {
        self.ignored_group_names
            .iter()
            .any(|ignored_name| ignored_name == normalized_name)
    }

    /// Check if a name exactly matches one of the ignored prefixes.
    #[must_use]
    pub fn is_ignored_prefix(&self, name: &str) -> bool {
        self.prefix_ignores
            .iter()
            .any(|ignore| ignore.eq_ignore_ascii_case(name))
    }

    /// Return whether a normalized name is too short, only digits, or an ignored group name.
    fn is_unusable_normalized_name(&self, normalized_name: &str) -> bool {
        normalized_name.chars().count() < MIN_DIRECTORY_NAME_CHARS
            || normalized_name.chars().all(char::is_numeric)
            || self.matches_ignored_group_name(normalized_name)
    }
}

/// Strip lowercase prefixes, each followed by `separator`, from the start of `name` until none match.
fn strip_prefixes_with_separator<'name>(name: &'name str, prefixes: &[String], separator: char) -> Cow<'name, str> {
    if prefixes.is_empty() {
        return Cow::Borrowed(name);
    }

    let patterns: Vec<String> = prefixes.iter().map(|prefix| format!("{prefix}{separator}")).collect();

    let mut result = name;
    let mut changed = true;
    while changed {
        changed = false;
        let result_lower = result.to_lowercase();
        for pattern in &patterns {
            if result_lower.starts_with(pattern.as_str())
                && let Some(prefix_end) = original_end_for_lowercase_prefix(result, pattern)
                && let Some(stripped) = result.get(prefix_end..)
            {
                result = stripped;
                changed = true;
                break;
            }
        }
    }

    if result == name {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(result.to_string())
    }
}

/// Map a lowercase prefix length to its ending byte boundary in the original string.
fn original_end_for_lowercase_prefix(text: &str, lowercase_prefix: &str) -> Option<usize> {
    let mut lowercase_length = 0;

    for (start, character) in text.char_indices() {
        lowercase_length += character.to_lowercase().map(char::len_utf8).sum::<usize>();
        match lowercase_length.cmp(&lowercase_prefix.len()) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => return Some(start + character.len_utf8()),
            std::cmp::Ordering::Greater => return None,
        }
    }

    lowercase_prefix.is_empty().then_some(0)
}

#[cfg(test)]
mod test_prefix_ignores {
    use crate::test_helpers::*;

    #[test]
    fn no_ignores() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), Vec::new());
        let result = fixture.name_filter().strip_ignored_prefixes("www example com test");
        assert_eq!(result, "www example com test");
    }

    #[test]
    fn single_ignore() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www"]);
        let result = fixture.name_filter().strip_ignored_prefixes("www example com test");
        assert_eq!(result, "example com test");
    }

    #[test]
    fn multiple_ignores() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www", "example"]);
        let result = fixture.name_filter().strip_ignored_prefixes("www example com test");
        assert_eq!(result, "com test");
    }

    #[test]
    fn no_match() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["xyz"]);
        let result = fixture.name_filter().strip_ignored_prefixes("www example com test");
        assert_eq!(result, "www example com test");
    }

    #[test]
    fn dot_prefix_single_ignore() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www"]);
        let result = fixture.name_filter().strip_ignored_dot_prefixes("www.example.com.test");
        assert_eq!(result, "example.com.test");
    }

    #[test]
    fn dot_prefix_multiple_ignores() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www", "example"]);
        let result = fixture.name_filter().strip_ignored_dot_prefixes("www.example.com.test");
        assert_eq!(result, "com.test");
    }

    #[test]
    fn case_insensitive_ignore() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["WWW"]);
        let result = fixture.name_filter().strip_ignored_dot_prefixes("www.example.com");
        assert_eq!(result, "example.com");
    }

    #[test]
    fn ignore_in_middle_not_stripped() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["example"]);
        let result = fixture.name_filter().strip_ignored_dot_prefixes("www.example.com");
        assert_eq!(result, "www.example.com");
    }

    #[test]
    fn is_ignored_prefix_exact_match() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["www"]);
        assert!(fixture.name_filter().is_ignored_prefix("www"));
        assert!(fixture.name_filter().is_ignored_prefix("WWW"));
        assert!(!fixture.name_filter().is_ignored_prefix("www2"));
    }

    #[test]
    fn repeated_prefix_in_name() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["prefix"]);
        let result = fixture
            .name_filter()
            .strip_ignored_dot_prefixes("prefix.prefix.name.mp4");
        assert_eq!(result, "name.mp4");
    }

    #[test]
    fn all_parts_are_ignored() {
        let fixture = make_test_fixture_with_ignores(Vec::new(), vec!["a", "b"]);
        let result = fixture.name_filter().strip_ignored_dot_prefixes("a.b.c");
        assert_eq!(result, "c");
    }
}
