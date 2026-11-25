//! Resolved `dirmove` options shared by the library components.
//!
//! Holds the `Config` combined from command line arguments and the user config file,
//! and the `CustomMapping` pairs that route matching files to a fixed directory.

use crate::name_filter::NameFilter;
#[cfg(test)]
use crate::utils;

/// Final config combined from CLI arguments and user config file.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// Confirm every prompt automatically.
    pub auto: bool,
    /// Create directories for files that share a name prefix.
    pub create: bool,
    /// Patterns that route matching files to a fixed directory.
    pub custom_mappings: Vec<CustomMapping>,
    /// Print debug information.
    pub debug: bool,
    /// Only print what would be done.
    pub dryrun: bool,
    /// Skip whole directory merge offers and only move files.
    pub files_only: bool,
    /// Lowercase substrings a file name must contain.
    pub include: Vec<String>,
    /// Lowercase substrings that exclude a file or directory name.
    pub exclude: Vec<String>,
    /// Normalized group names that are never offered as directories.
    pub ignored_group_names: Vec<String>,
    /// Lowercase name parts that disqualify a group.
    pub ignored_group_parts: Vec<String>,
    /// Minimum number of files in a group.
    pub min_group_size: usize,
    /// Minimum number of characters in a group prefix.
    pub min_prefix_chars: usize,
    /// Overwrite existing files and directories.
    pub overwrite: bool,
    /// Lowercase prefixes stripped from names before matching.
    pub prefix_ignores: Vec<String>,
    /// Prefixes that replace any group prefix they overlap with.
    pub prefix_overrides: Vec<String>,
    /// Scan nested directories.
    pub recurse: bool,
    /// Print verbose output.
    pub verbose: bool,
    /// Lowercase directory names whose contents are moved up into the parent directory.
    pub unpack_directory_names: Vec<String>,
    /// Print the database contents and exit.
    pub show_db: bool,
}

/// A custom mapping pair that maps files containing a pattern to a specific directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomMapping {
    /// Pattern to match in filename (case-insensitive, normalized).
    pub pattern: String,
    /// Target directory name for matching files.
    pub directory: String,
}

impl Config {
    /// Return the filter for ignored prefixes and ignored group names.
    #[must_use]
    pub fn name_filter(&self) -> NameFilter<'_> {
        NameFilter::new(&self.prefix_ignores, &self.ignored_group_names)
    }

    /// Return whether a lowercase name contains any include pattern, or there are no include patterns.
    #[must_use]
    pub fn is_included(&self, name_lowercase: &str) -> bool {
        self.include.is_empty() || self.include.iter().any(|pattern| name_lowercase.contains(pattern))
    }

    /// Return whether a lowercase name contains any exclude pattern.
    #[must_use]
    pub fn is_excluded(&self, name_lowercase: &str) -> bool {
        self.exclude.iter().any(|pattern| name_lowercase.contains(pattern))
    }

    /// Return whether any part of a name matches an ignored group part, ignoring ASCII case.
    #[must_use]
    pub fn has_ignored_group_part<'a>(&self, mut parts: impl Iterator<Item = &'a str>) -> bool {
        parts.any(|part| {
            self.ignored_group_parts
                .iter()
                .any(|ignored| part.eq_ignore_ascii_case(ignored))
        })
    }
}

impl CustomMapping {
    /// Create a new custom mapping with normalized pattern (lowercase, no dots/spaces).
    #[must_use]
    pub fn new(pattern: &str, directory: &str) -> Self {
        Self {
            pattern: pattern.to_lowercase().replace(['.', ' '], ""),
            directory: directory.to_string(),
        }
    }
}

#[cfg(test)]
impl Config {
    /// Create a test config with specified `min_group_size` and default values.
    #[must_use]
    pub fn test_with_group_size(min_group_size: usize) -> Self {
        Self {
            min_group_size,
            min_prefix_chars: 1,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with prefix ignores (automatically lowercased).
    pub fn test_with_prefix_ignores(prefix_ignores: Vec<&str>) -> Self {
        Self {
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with prefix overrides and ignores (ignores automatically lowercased).
    pub fn test_with_overrides_and_ignores(prefix_overrides: Vec<&str>, prefix_ignores: Vec<&str>) -> Self {
        Self {
            prefix_overrides: prefix_overrides.into_iter().map(String::from).collect(),
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 5,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with ignored group names (automatically lowercased).
    pub fn test_with_ignored_group_names(ignored_group_names: Vec<&str>) -> Self {
        Self {
            ignored_group_names: ignored_group_names.into_iter().map(utils::normalize_name).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            create: true,
            ..Default::default()
        }
    }

    /// Create a test config with ignored group parts (automatically lowercased).
    pub fn test_with_ignored_group_parts(ignored_group_parts: Vec<&str>) -> Self {
        Self {
            ignored_group_parts: ignored_group_parts.into_iter().map(str::to_lowercase).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            create: true,
            ..Default::default()
        }
    }

    /// Create a test config with specified `min_group_size` and `min_prefix_chars`.
    #[must_use]
    pub fn test_with_group_size_and_min_chars(min_group_size: usize, min_prefix_chars: usize) -> Self {
        Self {
            min_group_size,
            min_prefix_chars,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with prefix ignores and specified `min_group_size` (ignores automatically lowercased).
    pub fn test_with_ignores_and_group_size(prefix_ignores: Vec<&str>, min_group_size: usize) -> Self {
        Self {
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            min_group_size,
            min_prefix_chars: 1,
            dryrun: true,
            ..Default::default()
        }
    }

    /// Create a test config with both prefix ignores and ignored group names (both automatically lowercased).
    pub fn test_with_prefix_ignores_and_ignored_group_names(
        prefix_ignores: Vec<&str>,
        ignored_group_names: Vec<&str>,
    ) -> Self {
        Self {
            prefix_ignores: prefix_ignores.into_iter().map(str::to_lowercase).collect(),
            ignored_group_names: ignored_group_names.into_iter().map(utils::normalize_name).collect(),
            min_group_size: 3,
            min_prefix_chars: 1,
            dryrun: true,
            create: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod test_custom_mapping {
    use super::*;

    #[test]
    fn new_normalizes_pattern_to_lowercase() {
        let mapping = CustomMapping::new("PATTERN", "Directory");
        assert_eq!(mapping.pattern, "pattern");
    }

    #[test]
    fn new_removes_dots_and_spaces_from_pattern() {
        let mapping = CustomMapping::new("Some.Pattern Name", "Directory");
        assert_eq!(mapping.pattern, "somepatternname");
    }

    #[test]
    fn new_preserves_directory_name_case() {
        let mapping = CustomMapping::new("pattern", "My Directory");
        assert_eq!(mapping.directory, "My Directory");
    }
}

#[cfg(test)]
mod test_config_filters {
    use super::*;

    #[test]
    fn everything_is_included_without_include_patterns() {
        assert!(Config::default().is_included("anything.mp4"));
    }

    #[test]
    fn include_patterns_require_a_match() {
        let config = Config {
            include: vec!["show".to_string()],
            ..Default::default()
        };
        assert!(config.is_included("my.show.mp4"));
        assert!(!config.is_included("other.mp4"));
    }

    #[test]
    fn exclude_patterns_exclude_matching_names() {
        let config = Config {
            exclude: vec!["sample".to_string()],
            ..Default::default()
        };
        assert!(config.is_excluded("show.sample.mp4"));
        assert!(!config.is_excluded("show.mp4"));
    }

    #[test]
    fn ignored_group_parts_match_ignoring_case() {
        let config = Config {
            ignored_group_parts: vec!["x265".to_string()],
            ..Default::default()
        };
        assert!(config.has_ignored_group_part(["DL", "X265"].into_iter()));
        assert!(!config.has_ignored_group_part(["DL", "x264"].into_iter()));
    }
}
