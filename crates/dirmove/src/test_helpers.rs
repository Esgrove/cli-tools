//! Shared builders for the library unit tests.
//!
//! `TestConfig` gathers every option a test sets in one place,
//! and `Fixture` builds the scanner, grouper, matcher, and name filter
//! from it the same way a caller composes the library components.

use std::path::{Path, PathBuf};

use crate::grouping::{GroupingOptions, PrefixGrouper};
use crate::matcher::{CustomMapping, DirectoryMatcher};
use crate::merge::{DirectoryMerger, find_prefix_merge_pairs};
use crate::name_filter::NameFilter;
use crate::roots::Roots;
use crate::scanner::{ScanOptions, Scanner};
use crate::types::{DirectoryInfo, FileInfo};
use crate::unpack::Unpacker;

/// Every option a library test can set, converted into the component options by `Fixture`.
#[derive(Debug, Clone, Default)]
pub struct TestConfig {
    /// Patterns that route matching files to a fixed directory.
    pub custom_mappings: Vec<CustomMapping>,
    /// Substrings a file name must contain.
    pub include: Vec<String>,
    /// Substrings that exclude a file or directory name.
    pub exclude: Vec<String>,
    /// Group names that are never offered.
    pub ignored_group_names: Vec<String>,
    /// Name parts that disqualify a group.
    pub ignored_group_parts: Vec<String>,
    /// Minimum number of files in a group.
    pub min_group_size: usize,
    /// Minimum number of characters in a group prefix.
    pub min_prefix_chars: usize,
    /// Prefixes stripped from names before matching.
    pub prefix_ignores: Vec<String>,
    /// Prefixes that replace any group prefix they overlap with.
    pub prefix_overrides: Vec<String>,
    /// Scan nested directories.
    pub recurse: bool,
}

/// Roots and component options built from a `TestConfig`.
pub struct Fixture {
    /// Input and output roots.
    roots: Roots,
    /// Options the fixture was built from.
    config: TestConfig,
    /// Ignored prefix and ignored group name rules.
    name_filter: NameFilter,
    /// Include and exclude patterns and the recursion setting.
    scan_options: ScanOptions,
    /// Group size, prefix length, ignored part, and prefix override rules.
    grouping_options: GroupingOptions,
}

impl TestConfig {
    /// Create a test config with specified `min_group_size` and default values.
    pub fn test_with_group_size(min_group_size: usize) -> Self {
        Self {
            min_group_size,
            min_prefix_chars: 1,
            ..Default::default()
        }
    }

    /// Create a test config with prefix ignores.
    pub fn test_with_prefix_ignores(prefix_ignores: Vec<&str>) -> Self {
        Self {
            prefix_ignores: strings(prefix_ignores),
            min_group_size: 3,
            min_prefix_chars: 1,
            ..Default::default()
        }
    }

    /// Create a test config with prefix overrides and ignores.
    pub fn test_with_overrides_and_ignores(prefix_overrides: Vec<&str>, prefix_ignores: Vec<&str>) -> Self {
        Self {
            prefix_overrides: strings(prefix_overrides),
            prefix_ignores: strings(prefix_ignores),
            min_group_size: 3,
            min_prefix_chars: 5,
            ..Default::default()
        }
    }

    /// Create a test config with ignored group names.
    pub fn test_with_ignored_group_names(ignored_group_names: Vec<&str>) -> Self {
        Self {
            ignored_group_names: strings(ignored_group_names),
            min_group_size: 3,
            min_prefix_chars: 1,
            ..Default::default()
        }
    }

    /// Create a test config with ignored group parts.
    pub fn test_with_ignored_group_parts(ignored_group_parts: Vec<&str>) -> Self {
        Self {
            ignored_group_parts: strings(ignored_group_parts),
            min_group_size: 3,
            min_prefix_chars: 1,
            ..Default::default()
        }
    }

    /// Create a test config with specified `min_group_size` and `min_prefix_chars`.
    pub fn test_with_group_size_and_min_chars(min_group_size: usize, min_prefix_chars: usize) -> Self {
        Self {
            min_group_size,
            min_prefix_chars,
            ..Default::default()
        }
    }

    /// Create a test config with prefix ignores and specified `min_group_size`.
    pub fn test_with_ignores_and_group_size(prefix_ignores: Vec<&str>, min_group_size: usize) -> Self {
        Self {
            prefix_ignores: strings(prefix_ignores),
            min_group_size,
            min_prefix_chars: 1,
            ..Default::default()
        }
    }

    /// Create a test config with both prefix ignores and ignored group names.
    pub fn test_with_prefix_ignores_and_ignored_group_names(
        prefix_ignores: Vec<&str>,
        ignored_group_names: Vec<&str>,
    ) -> Self {
        Self {
            prefix_ignores: strings(prefix_ignores),
            ignored_group_names: strings(ignored_group_names),
            min_group_size: 3,
            min_prefix_chars: 1,
            ..Default::default()
        }
    }
}

impl Fixture {
    /// Create a fixture using the same path as both input and output root.
    pub fn new(root: PathBuf, config: TestConfig) -> Self {
        let name_filter = NameFilter::new(&config.prefix_ignores, &config.ignored_group_names);
        let scan_options = ScanOptions::new(&config.include, &config.exclude, config.recurse);
        let grouping_options = GroupingOptions::new(
            config.min_group_size,
            config.min_prefix_chars,
            &config.ignored_group_parts,
            config.prefix_overrides.clone(),
        );
        Self {
            roots: Roots::new(vec![root.clone()], vec![root]),
            config,
            name_filter,
            scan_options,
            grouping_options,
        }
    }

    /// Return a scanner over the fixture roots.
    pub const fn scanner(&self) -> Scanner<'_> {
        Scanner::new(&self.roots, &self.scan_options, &self.name_filter)
    }

    /// Return a prefix grouper.
    pub const fn grouper(&self) -> PrefixGrouper<'_> {
        PrefixGrouper::new(&self.grouping_options, &self.name_filter)
    }

    /// Return a directory matcher for the fixture roots.
    pub fn matcher(&self) -> DirectoryMatcher<'_> {
        DirectoryMatcher::new(&self.roots, &self.name_filter, &self.config.custom_mappings)
    }

    /// Return the name filter.
    pub const fn name_filter(&self) -> &NameFilter {
        &self.name_filter
    }

    /// Return the config the fixture was built from.
    pub const fn config(&self) -> &TestConfig {
        &self.config
    }
}

impl Default for Fixture {
    /// Create a fixture rooted at the current directory placeholder with the default config.
    fn default() -> Self {
        Self::new(PathBuf::from("."), TestConfig::default())
    }
}

/// Convert string slices to owned strings.
fn strings(values: Vec<&str>) -> Vec<String> {
    values.into_iter().map(String::from).collect()
}

/// Create a fixture with prefix overrides and prefix ignores.
pub fn make_test_fixture_with_ignores(prefix_overrides: Vec<&str>, prefix_ignores: Vec<&str>) -> Fixture {
    Fixture::new(
        PathBuf::from("."),
        TestConfig::test_with_overrides_and_ignores(prefix_overrides, prefix_ignores),
    )
}

/// Create a fixture with prefix overrides.
pub fn make_test_fixture(prefix_overrides: Vec<&str>) -> Fixture {
    make_test_fixture_with_ignores(prefix_overrides, Vec::new())
}

/// Create test files with path, original name, and filtered name.
/// For simple tests, the original and filtered names are the same.
pub fn make_test_files(names: &[&str]) -> Vec<FileInfo<'static>> {
    names
        .iter()
        .map(|name| FileInfo::new(PathBuf::from(*name), (*name).to_string(), (*name).to_string()))
        .collect()
}

/// Create test directories whose name is the lowercase path.
pub fn make_test_dirs(names: &[&str]) -> Vec<DirectoryInfo> {
    names
        .iter()
        .map(|name| DirectoryInfo {
            path: PathBuf::from(*name),
            name: name.to_lowercase(),
        })
        .collect()
}

/// Create file paths from names.
pub fn make_file_paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

/// Unpack the given directory names under `root` without reporting anything.
pub fn unpack(root: &Path, names: &[&str], recurse: bool, dry_run: bool, overwrite: bool) {
    Unpacker::new(names, overwrite, dry_run).unpack(&[root.to_path_buf()], recurse, &mut ());
}

/// Merge every directory under `root` with an ignored prefix into its unprefixed sibling, reporting nothing.
pub fn merge_prefixed(
    root: &Path,
    prefix_ignores: &[&str],
    recurse: bool,
    dry_run: bool,
    overwrite: bool,
) -> anyhow::Result<()> {
    let name_filter = NameFilter::new(prefix_ignores, Vec::<String>::new());
    let merger = DirectoryMerger::new(overwrite, dry_run);
    for pair in find_prefix_merge_pairs(&[root.to_path_buf()], recurse, &name_filter)? {
        merger.merge(&pair.source, &pair.target, &mut ())?;
    }
    Ok(())
}

/// Write a file, creating its parent directories.
pub fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// Assert that a path exists.
pub fn assert_exists(path: &Path) {
    assert!(path.exists(), "Expected path to exist: {}", path.display());
}

/// Assert that a path does not exist.
pub fn assert_not_exists(path: &Path) {
    assert!(!path.exists(), "Expected path to NOT exist: {}", path.display());
}
