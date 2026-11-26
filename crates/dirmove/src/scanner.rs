//! Collecting the input files and directories and the candidate destination directories.
//!
//! `Scanner` walks the configured roots, applies the include, exclude, hidden, and unwanted name rules,
//! and returns the files to match or group and the directories they can move into.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use walkdir::WalkDir;

use cli_tools_common::unique_in_order;

use crate::candidates::FileInfo;
use crate::name_filter::{self, NameFilter};
use crate::roots::Roots;
use crate::types::DirectoryInfo;
use crate::utils;

/// Include and exclude patterns and the recursion setting used while scanning.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Lowercase substrings a file name must contain, when any are given.
    include: Vec<String>,
    /// Lowercase substrings that exclude a file or directory name.
    exclude: Vec<String>,
    /// Whether destination directories are collected from nested directories too.
    recurse: bool,
}

/// Collects files and directories from the configured roots.
#[derive(Debug, Clone, Copy)]
pub struct Scanner<'a> {
    /// Roots to scan.
    roots: &'a Roots,
    /// Include and exclude patterns and the recursion setting.
    options: &'a ScanOptions,
    /// Ignored prefix and ignored group name rules.
    name_filter: &'a NameFilter,
}

/// Candidate destination directories and the directories left out of them.
#[derive(Debug, Default)]
pub struct DirectoryScan {
    /// Directories files can move into.
    pub directories: Vec<DirectoryInfo>,
    /// Directories left out because their name contains an exclude pattern.
    pub excluded: Vec<PathBuf>,
    /// Directories left out by the ignored prefix and ignored group name rules.
    pub ignored: Vec<PathBuf>,
}

impl ScanOptions {
    /// Create scan options from include and exclude patterns, matched case-insensitively.
    ///
    /// Patterns are lowercased and duplicates removed.
    #[must_use]
    pub fn new<I, E>(include: I, exclude: E, recurse: bool) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        E: IntoIterator,
        E::Item: AsRef<str>,
    {
        Self {
            include: lowercase_unique(include),
            exclude: lowercase_unique(exclude),
            recurse,
        }
    }

    /// Return the lowercase include patterns.
    #[must_use]
    pub fn include(&self) -> &[String] {
        &self.include
    }

    /// Return the lowercase exclude patterns.
    #[must_use]
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// Return whether destination directories are collected from nested directories too.
    #[must_use]
    pub const fn recurse(&self) -> bool {
        self.recurse
    }

    /// Return whether a name matches any include glob or substring pattern, or there are no include patterns.
    #[must_use]
    fn is_included(&self, name: &str) -> bool {
        if self.include.is_empty() {
            return true;
        }
        if let Ok(filter) = cli_tools_common::NameFilter::new(&self.include, &[]) {
            return filter.matches_name(name);
        }
        let lower = name.to_lowercase();
        self.include.iter().any(|pattern| lower.contains(pattern))
    }

    /// Return whether a name matches any exclude glob or substring pattern.
    #[must_use]
    fn is_excluded(&self, name: &str) -> bool {
        if self.exclude.is_empty() {
            return false;
        }
        if let Ok(filter) = cli_tools_common::NameFilter::new(&[], &self.exclude) {
            return !filter.matches_name(name);
        }
        let lower = name.to_lowercase();
        self.exclude.iter().any(|pattern| lower.contains(pattern))
    }

    /// Return whether a filename matches the configured include and exclude glob/substring patterns.
    #[must_use]
    fn matches_name_filters(&self, filename: &str) -> bool {
        self.is_included(filename) && !self.is_excluded(filename)
    }
}

impl<'a> Scanner<'a> {
    /// Create a scanner over the given roots.
    #[must_use]
    pub const fn new(roots: &'a Roots, options: &'a ScanOptions, name_filter: &'a NameFilter) -> Self {
        Self {
            roots,
            options,
            name_filter,
        }
    }

    /// Collect direct input files from every input root.
    ///
    /// Explicit file inputs are included directly. Directory inputs contribute only their direct child files.
    /// Files are kept when they pass the include and exclude patterns.
    /// Hidden files are kept too, unlike in `collect_files_with_names`,
    /// so a hidden file whose name contains a directory name is still offered for that directory.
    ///
    /// # Errors
    /// Returns an error if an input directory cannot be read.
    pub fn collect_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let mut seen_paths = HashSet::new();
        self.for_each_input_file(|mut file_path| {
            let file_name = cli_tools_common::path_to_filename_string(&file_path);
            if self.options.matches_name_filters(&file_name) && seen_paths.insert(file_path.clone()) {
                file_path.shrink_to_fit();
                files.push(file_path);
            }
        })?;
        Ok(files)
    }

    /// Collect files with their processed names for prefix grouping.
    ///
    /// All configured input roots are scanned for direct child files,
    /// and explicit file inputs are included directly.
    /// Hidden files and files that fail the include and exclude patterns are skipped.
    /// The original name has configured dot-prefix ignores stripped
    /// before numeric, resolution, and glue filtering.
    ///
    /// # Errors
    /// Returns an error if an input directory cannot be read.
    pub fn collect_files_with_names(&self) -> Result<Vec<FileInfo>> {
        let mut files_with_names = Vec::new();
        let mut seen_paths = HashSet::new();

        self.for_each_input_file(|mut file_path| {
            if !seen_paths.insert(file_path.clone()) {
                return;
            }

            let file_name = cli_tools_common::path_to_filename_string(&file_path);
            if file_name.starts_with('.') {
                return;
            }

            if !self.options.matches_name_filters(&file_name) {
                return;
            }

            let original_name = self.name_filter.strip_ignored_dot_prefixes(&file_name);
            let filtered_name = utils::filter_numeric_resolution_and_glue_parts(&original_name);
            file_path.shrink_to_fit();
            files_with_names.push(FileInfo::new(file_path, original_name, filtered_name));
        })?;

        Ok(files_with_names)
    }

    /// Collect candidate destination directories from every output root.
    ///
    /// Without recursion, only direct child directories of each output root are considered.
    /// With recursion, nested directories are included and later prioritized by depth during matching.
    /// Duplicate paths are ignored when roots overlap.
    ///
    /// # Errors
    /// Returns an error if an output directory cannot be walked.
    pub fn collect_directories(&self) -> Result<DirectoryScan> {
        let mut scan = DirectoryScan::default();
        let mut candidates = Vec::new();
        let mut seen_paths = HashSet::new();
        let max_depth = if self.options.recurse { usize::MAX } else { 1 };

        for output_root in self.roots.output() {
            for entry in WalkDir::new(output_root)
                .min_depth(1)
                .max_depth(max_depth)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || (!cli_tools_common::should_skip_entry(entry)
                            && !name_filter::is_unwanted_directory(&cli_tools_common::os_str_to_string(
                                entry.file_name(),
                            )))
                })
            {
                let entry = entry?;
                if !entry.file_type().is_dir() {
                    continue;
                }

                let directory_path = entry.path().to_path_buf();
                if !seen_paths.insert(directory_path.clone()) {
                    continue;
                }

                let directory_name = cli_tools_common::os_str_to_string(entry.file_name()).to_lowercase();
                if self.options.is_excluded(&directory_name) {
                    scan.excluded.push(directory_path);
                    continue;
                }

                candidates.push(DirectoryInfo::new(directory_path));
            }
        }

        (scan.directories, scan.ignored) = self.name_filter.partition_destination_directories(candidates);
        Ok(scan)
    }

    /// Collect direct child directories from every input root for whole directory merge matching.
    ///
    /// Only immediate children are considered,
    /// because this pass offers moving a whole top-level input directory before considering individual files.
    /// Hidden, system, unwanted, and excluded directory names are skipped.
    /// Explicit file inputs do not contribute directories.
    ///
    /// # Errors
    /// Returns an error if an input directory cannot be read.
    pub fn collect_input_directories(&self) -> Result<Vec<PathBuf>> {
        let mut directories = Vec::new();
        let mut seen_paths = HashSet::new();

        for input_root in self.roots.input() {
            if !input_root.is_dir() {
                continue;
            }

            for entry in std::fs::read_dir(input_root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }

                let path = entry.path();
                if cli_tools_common::is_system_directory_path(&path) {
                    continue;
                }

                let directory_name = cli_tools_common::path_to_filename_string(&path);
                if directory_name.starts_with('.')
                    || name_filter::is_unwanted_directory(&directory_name)
                    || self.options.is_excluded(&directory_name.to_lowercase())
                {
                    continue;
                }

                if seen_paths.insert(path.clone()) {
                    directories.push(path);
                }
            }
        }

        Ok(directories)
    }

    /// Visit explicit file inputs and the direct child files of input directories, in root order.
    ///
    /// A path built from a directory entry has spare capacity from joining the directory and file name,
    /// so callers that keep a path shrink it to fit first.
    fn for_each_input_file(&self, mut visit: impl FnMut(PathBuf)) -> Result<()> {
        for input_root in self.roots.input() {
            if input_root.is_file() {
                visit(input_root.clone());
                continue;
            }

            for entry in std::fs::read_dir(input_root)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    visit(entry.path());
                }
            }
        }
        Ok(())
    }
}

/// Lowercase patterns and remove duplicates, keeping the first occurrence of each.
fn lowercase_unique<P>(patterns: P) -> Vec<String>
where
    P: IntoIterator,
    P::Item: AsRef<str>,
{
    unique_in_order(patterns.into_iter().map(|pattern| pattern.as_ref().to_lowercase()))
}

#[cfg(test)]
mod test_collect_directories {

    use crate::test_helpers::*;
    use cli_tools_common::path_to_filename_string;

    #[test]
    fn unwanted_directories_are_skipped() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let unwanted = root.join(".unwanted");
        std::fs::create_dir(&unwanted)?;
        let serato = root.join("_Serato_");
        std::fs::create_dir(&serato)?;
        let normal = root.join("normal");
        std::fs::create_dir(&normal)?;

        let directories = Fixture::new(root, TestConfig::default())
            .scanner()
            .collect_directories()?
            .directories;

        assert_eq!(directories.len(), 1);
        assert_eq!(directories[0].path, normal);
        Ok(())
    }

    #[test]
    fn collect_directories_filters_ignored_group_names_and_keeps_other_names() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let root = tmp.path().to_path_buf();
        for directory_name in [
            "Episode",
            "Season One",
            "Season.One",
            "SeasonOne",
            "Studio",
            "Studio Archive",
        ] {
            std::fs::create_dir(root.join(directory_name))?;
        }

        let config = TestConfig::test_with_ignored_group_names(vec!["EPISODE", "season one"]);
        let fixture = Fixture::new(root, config);

        let mut directory_names: Vec<_> = fixture
            .scanner()
            .collect_directories()?
            .directories
            .iter()
            .map(|directory| path_to_filename_string(&directory.path))
            .collect();
        directory_names.sort();

        assert_eq!(
            directory_names,
            vec!["Studio".to_string(), "Studio Archive".to_string()]
        );
        Ok(())
    }

    #[test]
    fn collect_directories_filters_prefix_stripped_ignored_group_names() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let root = tmp.path().to_path_buf();
        for directory_name in [
            "DL Studio",
            "WEB DL Episode",
            "WEB.DL.Episode",
            "DL Studio Archive",
            "Other",
        ] {
            std::fs::create_dir(root.join(directory_name))?;
        }

        let config =
            TestConfig::test_with_prefix_ignores_and_ignored_group_names(vec!["web", "dl"], vec!["studio", "episode"]);
        let fixture = Fixture::new(root, config);

        let mut directory_names: Vec<_> = fixture
            .scanner()
            .collect_directories()?
            .directories
            .iter()
            .map(|directory| path_to_filename_string(&directory.path))
            .collect();
        directory_names.sort();

        assert_eq!(
            directory_names,
            vec!["DL Studio Archive".to_string(), "Other".to_string()]
        );
        Ok(())
    }
}

#[cfg(test)]
mod test_collect_input_directories {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn serato_skipped_as_input_directory_for_merge() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let root = tmp.path().join("Example");
        std::fs::create_dir_all(&root)?;
        let serato = root.join("_Serato_");
        std::fs::create_dir(&serato)?;
        let normal = root.join("normal");
        std::fs::create_dir(&normal)?;

        let roots = Roots::new(vec![root.clone()], vec![root]);
        let directories =
            Scanner::new(&roots, &ScanOptions::default(), &NameFilter::default()).collect_input_directories()?;

        assert_eq!(directories, vec![normal]);
        Ok(())
    }
}

#[cfg(test)]
mod test_scan_options {
    use super::*;

    #[test]
    fn everything_is_included_without_include_patterns() {
        assert!(ScanOptions::default().is_included("anything.mp4"));
    }

    #[test]
    fn include_patterns_require_a_match_ignoring_case() {
        let options = ScanOptions::new(["Show"], Vec::<String>::new(), false);
        assert!(options.is_included("my.show.mp4"));
        assert!(!options.is_included("other.mp4"));
    }

    #[test]
    fn exclude_patterns_exclude_matching_names_ignoring_case() {
        let options = ScanOptions::new(Vec::<String>::new(), ["SAMPLE"], false);
        assert!(options.is_excluded("show.sample.mp4"));
        assert!(!options.is_excluded("show.mp4"));
    }
}

#[cfg(test)]
mod test_collect_files {
    use std::path::Path;

    use cli_tools_common::test_support::TempTree;

    use super::*;

    /// Build a scanner over the given input roots with the given include and exclude patterns.
    fn scan_files(inputs: Vec<PathBuf>, include: &[&str], exclude: &[&str]) -> Vec<String> {
        let roots = Roots::new(inputs, vec![PathBuf::from("output")]);
        let options = ScanOptions::new(include, exclude, false);
        let name_filter = NameFilter::default();
        let mut names: Vec<String> = Scanner::new(&roots, &options, &name_filter)
            .collect_files()
            .unwrap()
            .iter()
            .map(|path| cli_tools_common::path_to_filename_string(path))
            .collect();
        names.sort();
        names
    }

    /// Return the file names that `collect_files_with_names` finds under one root.
    fn scan_named_files(root: &Path) -> Vec<String> {
        let roots = Roots::new(vec![root.to_path_buf()], vec![root.to_path_buf()]);
        let options = ScanOptions::default();
        let name_filter = NameFilter::default();
        let mut names: Vec<String> = Scanner::new(&roots, &options, &name_filter)
            .collect_files_with_names()
            .unwrap()
            .iter()
            .map(|file| cli_tools_common::path_to_filename_string(&file.path))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn collects_only_direct_child_files() {
        let tree = TempTree::with_entries(&["root/a.mp4", "root/b.mkv", "root/nested/c.mp4", "root/empty/"]);

        assert_eq!(scan_files(vec![tree.join("root")], &[], &[]), ["a.mp4", "b.mkv"]);
    }

    #[test]
    fn applies_include_and_exclude_patterns_ignoring_case() {
        let tree = TempTree::with_entries(&["root/Show.One.mp4", "root/Show.Sample.mp4", "root/Other.mp4"]);

        assert_eq!(
            scan_files(vec![tree.join("root")], &["SHOW"], &["sample"]),
            ["Show.One.mp4"]
        );
    }

    #[test]
    fn explicit_file_inputs_are_included_and_not_repeated() {
        let tree = TempTree::with_entries(&["root/a.mp4", "root/b.mp4", "elsewhere/c.mp4"]);

        let names = scan_files(
            vec![tree.join("root"), tree.join("root/a.mp4"), tree.join("elsewhere/c.mp4")],
            &[],
            &[],
        );

        assert_eq!(names, ["a.mp4", "b.mp4", "c.mp4"]);
    }

    #[test]
    fn a_root_given_twice_is_scanned_once() {
        let tree = TempTree::with_entries(&["root/a.mp4"]);

        assert_eq!(
            scan_files(vec![tree.join("root"), tree.join("root")], &[], &[]),
            ["a.mp4"]
        );
    }

    #[test]
    fn hidden_files_are_offered_for_moves_but_never_grouped() {
        let tree = TempTree::with_entries(&["root/.Show.Name.mp4", "root/Show.Name.mp4"]);

        assert_eq!(
            scan_files(vec![tree.join("root")], &[], &[]),
            [".Show.Name.mp4", "Show.Name.mp4"]
        );
        assert_eq!(scan_named_files(&tree.join("root")), ["Show.Name.mp4"]);
    }
}
