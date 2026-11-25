//! Collecting the input files and directories and the candidate destination directories.
//!
//! `Scanner` walks the configured roots, applies the include, exclude, hidden, and unwanted name rules,
//! and returns the files to match or group and the directories they can move into.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use crate::config::Config;
use crate::roots::Roots;
use crate::types::{DirectoryInfo, FileInfo};
use crate::utils;

/// Collects files and directories from the configured roots.
#[derive(Debug, Clone, Copy)]
pub struct Scanner<'a> {
    /// Roots to scan.
    roots: &'a Roots,
    /// Options for filtering names.
    config: &'a Config,
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

impl<'a> Scanner<'a> {
    /// Create a scanner over the given roots.
    #[must_use]
    pub const fn new(roots: &'a Roots, config: &'a Config) -> Self {
        Self { roots, config }
    }

    /// Collect direct input files from every input root.
    ///
    /// Explicit file inputs are included directly. Directory inputs contribute only their direct child files.
    /// Files are kept when they pass the include and exclude patterns.
    ///
    /// # Errors
    /// Returns an error if an input directory cannot be read.
    pub fn collect_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let mut seen_paths = HashSet::new();
        for file_path in self.input_file_paths()? {
            let file_name = cli_tools_common::path_to_filename_string(&file_path).to_lowercase();
            if self.config.is_included(&file_name)
                && !self.config.is_excluded(&file_name)
                && seen_paths.insert(file_path.clone())
            {
                files.push(file_path);
            }
        }
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
    pub fn collect_files_with_names(&self) -> Result<Vec<FileInfo<'static>>> {
        let name_filter = self.config.name_filter();
        let mut files_with_names = Vec::new();
        let mut seen_paths = HashSet::new();

        for file_path in self.input_file_paths()? {
            if !seen_paths.insert(file_path.clone()) {
                continue;
            }

            let file_name = cli_tools_common::path_to_filename_string(&file_path);
            if file_name.starts_with('.') {
                continue;
            }

            let file_name_lowercase = file_name.to_lowercase();
            if !self.config.is_included(&file_name_lowercase) || self.config.is_excluded(&file_name_lowercase) {
                continue;
            }

            let original_name = name_filter.strip_ignored_dot_prefixes(&file_name);
            let filtered_name = utils::filter_numeric_resolution_and_glue_parts(&original_name);
            files_with_names.push(FileInfo::new(file_path, original_name, filtered_name));
        }

        Ok(files_with_names)
    }

    /// Collect candidate destination directories from every output root.
    ///
    /// Without `recurse`, only direct child directories of each output root are considered.
    /// With `recurse`, nested directories are included and later prioritized by depth during matching.
    /// Duplicate paths are ignored when roots overlap.
    ///
    /// # Errors
    /// Returns an error if an output directory cannot be walked.
    pub fn collect_directories(&self) -> Result<DirectoryScan> {
        let mut scan = DirectoryScan::default();
        let mut candidates = Vec::new();
        let mut seen_paths = HashSet::new();
        let max_depth = if self.config.recurse { usize::MAX } else { 1 };

        for output_root in self.roots.output() {
            for entry in WalkDir::new(output_root)
                .min_depth(1)
                .max_depth(max_depth)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || (!cli_tools_common::should_skip_entry(entry)
                            && !utils::is_unwanted_directory(&cli_tools_common::os_str_to_string(entry.file_name())))
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
                if self.config.is_excluded(&directory_name) {
                    scan.excluded.push(directory_path);
                    continue;
                }

                candidates.push(DirectoryInfo::new(directory_path));
            }
        }

        (scan.directories, scan.ignored) = self.config.name_filter().partition_destination_directories(candidates);
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
                    || utils::is_unwanted_directory(&directory_name)
                    || self.config.is_excluded(&directory_name.to_lowercase())
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

    /// List explicit file inputs and the direct child files of input directories, in root order.
    fn input_file_paths(&self) -> Result<Vec<PathBuf>> {
        let mut file_paths = Vec::new();
        for input_root in self.roots.input() {
            if input_root.is_file() {
                file_paths.push(input_root.clone());
                continue;
            }

            for entry in std::fs::read_dir(input_root)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    file_paths.push(entry.path());
                }
            }
        }
        Ok(file_paths)
    }
}

/// Return whether a file can still be offered by another matching group.
#[must_use]
pub fn file_is_available<S: BuildHasher>(file_path: &Path, consumed_files: &HashSet<PathBuf, S>) -> bool {
    !consumed_files.contains(file_path) && file_path.exists()
}

#[cfg(test)]
mod test_collect_directories {
    use super::*;
    use crate::test_helpers::*;
    use cli_tools_common::path_to_filename_string;

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

        let config = Config::test_with_ignored_group_names(vec!["EPISODE", "season one"]);
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
            Config::test_with_prefix_ignores_and_ignored_group_names(vec!["web", "dl"], vec!["studio", "episode"]);
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
        let config = Config::default();
        let directories = Scanner::new(&roots, &config).collect_input_directories()?;

        assert_eq!(directories, vec![normal]);
        Ok(())
    }
}
