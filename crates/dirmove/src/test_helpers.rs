//! Shared builders for the library unit tests.
//!
//! `Fixture` holds roots and a config
//! and hands out the scanner, grouper, matcher, and name filter built from them,
//! so a test can exercise each component the way the `dirmove` binary composes them.

use std::path::PathBuf;

use crate::config::Config;
use crate::grouping::PrefixGrouper;
use crate::matcher::DirectoryMatcher;
use crate::name_filter::NameFilter;
use crate::roots::Roots;
use crate::scanner::Scanner;
use crate::types::{DirectoryInfo, FileInfo};

/// Roots and a config that the library components under test are built from.
pub struct Fixture {
    /// Input and output roots.
    roots: Roots,
    /// Options for every component.
    config: Config,
}

impl Fixture {
    /// Create a fixture using the same path as both input and output root.
    pub fn new(root: PathBuf, config: Config) -> Self {
        Self {
            roots: Roots::new(vec![root.clone()], vec![root]),
            config,
        }
    }

    /// Return a scanner over the fixture roots.
    pub const fn scanner(&self) -> Scanner<'_> {
        Scanner::new(&self.roots, &self.config)
    }

    /// Return a prefix grouper with hidden progress bars.
    pub const fn grouper(&self) -> PrefixGrouper<'_> {
        PrefixGrouper::new(&self.config, true)
    }

    /// Return a directory matcher for the fixture roots.
    pub const fn matcher(&self) -> DirectoryMatcher<'_> {
        DirectoryMatcher::new(&self.roots, &self.config)
    }

    /// Return the fixture config.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Return the name filter of the fixture config.
    pub fn name_filter(&self) -> NameFilter<'_> {
        self.config.name_filter()
    }
}

impl Default for Fixture {
    /// Create a fixture rooted at the current directory placeholder with the default config.
    fn default() -> Self {
        Self::new(PathBuf::from("."), Config::default())
    }
}

/// Create a fixture with prefix overrides and prefix ignores.
pub fn make_test_fixture_with_ignores(prefix_overrides: Vec<&str>, prefix_ignores: Vec<&str>) -> Fixture {
    Fixture::new(
        PathBuf::from("."),
        Config::test_with_overrides_and_ignores(prefix_overrides, prefix_ignores),
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
