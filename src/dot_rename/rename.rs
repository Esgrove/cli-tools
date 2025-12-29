//! Dot rename implementation for formatting filenames with dot separators.

use std::path::{Path, PathBuf};
use std::{fmt, fs};

use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use indicatif::ProgressBar;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use rayon::prelude::*;
use regex::Regex;
use walkdir::WalkDir;

use crate::dot_rename::{DotFormatting, DotRenameConfig};

#[cfg(not(test))]
const PROGRESS_BAR_CHARS: &str = "=> ";
#[cfg(not(test))]
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.cyan/blue} {pos}/{len} {percent}%";

/// Dot rename handler for formatting filenames with dot separators.
#[derive(Debug, Default)]
pub struct DotRename {
    root: PathBuf,
    config: DotRenameConfig,
    path_given: bool,
}

impl DotRename {
    /// Create a new instance with CLI args.
    #[must_use]
    pub const fn new(root: PathBuf, config: DotRenameConfig, path_given: bool) -> Self {
        Self {
            root,
            config,
            path_given,
        }
    }

    /// Create a formatter that borrows the config.
    const fn formatter(&self) -> DotFormatting<'_> {
        DotFormatting::new(&self.config)
    }

    /// Create a new instance for name formatting only (no file operations).
    ///
    /// This loads the user config from the config file and creates a minimal
    /// instance suitable for calling `format_name`.
    #[must_use]
    pub fn for_name_formatting() -> Self {
        let config = DotRenameConfig::for_name_formatting();
        Self {
            root: PathBuf::new(),
            config,
            path_given: false,
        }
    }

    /// Format a file name using the configured formatting rules.
    #[must_use]
    pub fn format_name(&self, name: &str) -> String {
        self.formatter().format_name(name)
    }

    /// Format a directory name using the configured formatting rules.
    #[must_use]
    pub fn format_directory_name(&self, name: &str) -> String {
        self.formatter().format_directory_name(name)
    }

    /// Format a file with prefix/suffix based on its parent directory.
    #[must_use]
    pub fn format_file_with_parent_prefix_suffix(&self, path: &Path) -> Option<PathBuf> {
        self.formatter().format_file_with_parent_prefix_suffix(path)
    }

    /// Run renaming.
    ///
    /// # Errors
    /// Returns an error if directory renaming is requested but a file was given,
    /// or if file operations fail.
    pub fn run(&mut self) -> Result<()> {
        if self.config.rename_directories && self.root.is_file() {
            anyhow::bail!("Cannot rename directories when a file was given as input path");
        }
        let (paths_to_rename, name) = if self.config.rename_directories {
            (self.gather_directories_to_rename(self.path_given), "directories")
        } else {
            (self.gather_files_to_rename()?, "files")
        };

        if self.config.debug {
            println!("{self}");
        }

        if paths_to_rename.is_empty() {
            if self.config.verbose {
                println!("No {name} to rename");
            }
            return Ok(());
        }

        let num_renamed = self.rename_paths(paths_to_rename);
        let message = format!(
            "{num_renamed} {}",
            match (self.config.rename_directories, num_renamed > 1) {
                (true, true) => "directories",
                (true, false) => "directory",
                (false, true) => "files",
                (false, false) => "file",
            }
        );

        if self.config.dryrun {
            println!("Dryrun: would have renamed {message}");
        } else {
            println!("{}", format!("Renamed {message}").green());
        }
        Ok(())
    }

    /// Get all files that need to be renamed.
    fn gather_files_to_rename(&mut self) -> Result<Vec<(PathBuf, PathBuf)>> {
        // Handle recursive prefix/suffix separately - they compute per-file
        if self.config.prefix_dir_recursive || self.config.suffix_dir_recursive {
            return Ok(self.gather_files_with_recursive_prefix_suffix());
        }

        // Non-recursive prefix/suffix: use root directory name for all files
        if self.config.prefix_dir || self.config.suffix_dir {
            let formatted_dir = if self.root.is_dir() {
                crate::get_normalized_dir_name(&self.root)?
            } else {
                let parent_dir = self.root.parent().context("Failed to get parent dir")?;
                crate::get_normalized_dir_name(parent_dir)?
            };
            let name = self.format_name(&formatted_dir);
            if self.config.prefix_dir {
                if self.config.verbose {
                    println!("Using directory prefix: {name}");
                }
                // Only add reordering regexes if prefix_dir_start is not set
                if !self.config.prefix_dir_start {
                    let regexes = Self::build_prefix_dir_regexes(&name)?;
                    self.config.regex_replace_after.extend(regexes);
                }
                // Add prefix to deduplication patterns to remove consecutive duplicates
                let prefix_with_dot = if name.ends_with('.') {
                    name.clone()
                } else {
                    format!("{name}.")
                };
                let already_exists = self
                    .config
                    .deduplicate_patterns
                    .iter()
                    .any(|(_, existing)| existing == &prefix_with_dot);
                if !already_exists {
                    let escaped = regex::escape(&prefix_with_dot);
                    if let Ok(re) = Regex::new(&format!(r"({escaped}){{2,}}")) {
                        self.config.deduplicate_patterns.push((re, prefix_with_dot));
                    }
                }
                self.config.prefix = Some(name);
            } else if self.config.suffix_dir {
                if self.config.verbose {
                    println!("Using directory suffix: {name}");
                }
                self.config.suffix = Option::from(name);
            }
        }

        if self.root.is_file() {
            if self.config.verbose {
                println!("{}", format!("Formatting file {}", self.root.display()).bold());
            }
            return Ok(self
                .formatted_filepath(&self.root)
                .ok()
                .filter(|new_path| &self.root != new_path)
                .map(|new_path| vec![(self.root.clone(), new_path)])
                .unwrap_or_default());
        }

        if self.config.verbose {
            println!("{}", format!("Formatting files under {}", self.root.display()).bold());
        }

        let max_depth = if self.config.recurse { 100 } else { 1 };

        // Collect all file paths first
        let paths: Vec<PathBuf> = WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| !crate::should_skip_entry(e))
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .collect();

        let progress_bar = Self::create_progress_bar(paths.len() as u64);

        // Filter and format files in parallel
        let mut results: Vec<_> = paths
            .into_par_iter()
            .filter_map(|path| {
                let result = {
                    // Filter based on include and exclude lists
                    let path_str = crate::path_to_string(&path);
                    let include = self.config.include.iter().all(|name| path_str.contains(name));
                    let exclude = self.config.exclude.iter().all(|name| !path_str.contains(name));
                    if include && exclude {
                        self.formatted_filepath(&path)
                            .ok()
                            .filter(|new_path| &path != new_path)
                            .map(|new_path| (path, new_path))
                    } else {
                        None
                    }
                };
                progress_bar.inc(1);
                result
            })
            .collect();

        progress_bar.finish_and_clear();

        // Sort sequentially after parallel collection
        results.sort_by(|(a, _), (b, _)| {
            a.to_string_lossy()
                .to_lowercase()
                .cmp(&b.to_string_lossy().to_lowercase())
        });

        Ok(results)
    }

    /// Get all files to rename using recursive prefix/suffix mode.
    /// Each file uses its parent directory name as the prefix/suffix.
    fn gather_files_with_recursive_prefix_suffix(&self) -> Vec<(PathBuf, PathBuf)> {
        if self.config.verbose {
            println!(
                "{}",
                format!(
                    "Formatting files under {} with recursive parent prefix/suffix",
                    self.root.display()
                )
                .bold()
            );
        }

        let max_depth = if self.config.recurse { 100 } else { 1 };

        // Collect all file paths first
        let paths: Vec<PathBuf> = WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| !crate::should_skip_entry(e))
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|p| p.is_file())
            .collect();

        let progress_bar = Self::create_progress_bar(paths.len() as u64);

        // Process files - cannot use parallel iteration since we need to modify prefix/suffix per file
        let mut results: Vec<(PathBuf, PathBuf)> = Vec::new();

        for path in paths {
            let path_str = crate::path_to_string(&path);
            let include = self.config.include.iter().all(|name| path_str.contains(name));
            let exclude = self.config.exclude.iter().all(|name| !path_str.contains(name));

            if include
                && exclude
                && let Some(result) = self.format_file_with_parent_prefix_suffix(&path)
                && path != result
            {
                results.push((path, result));
            }
            progress_bar.inc(1);
        }

        progress_bar.finish_and_clear();

        // Sort results
        results.sort_by(|(a, _), (b, _)| {
            a.to_string_lossy()
                .to_lowercase()
                .cmp(&b.to_string_lossy().to_lowercase())
        });

        results
    }

    /// Get all directories that need to be renamed.
    fn gather_directories_to_rename(&self, path_specified: bool) -> Vec<(PathBuf, PathBuf)> {
        // Path specified without recurse - rename only that specific directory
        if path_specified && !self.config.recurse {
            return self
                .formatted_directory_path(&self.root)
                .ok()
                .filter(|new_path| &self.root != new_path)
                .map(|new_path| (self.root.clone(), new_path))
                .into_iter()
                .collect();
        }

        // No path specified, or recursing - rename directories inside root, not root itself
        let walker = if self.config.recurse {
            WalkDir::new(&self.root).min_depth(1)
        } else {
            WalkDir::new(&self.root).min_depth(1).max_depth(1)
        };

        // Collect all directory paths first
        let paths: Vec<PathBuf> = walker
            .into_iter()
            .filter_entry(|e| !crate::should_skip_entry(e))
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(walkdir::DirEntry::into_path)
            .collect();

        let progress_bar = Self::create_progress_bar(paths.len() as u64);

        let mut results: Vec<_> = paths
            .into_par_iter()
            .filter_map(|path| {
                let result = {
                    // Filter based on include list
                    let matches_filter = if self.config.include.is_empty() {
                        true
                    } else {
                        let path_str = crate::path_to_string(&path);
                        self.config.include.iter().all(|name| path_str.contains(name))
                    };
                    if matches_filter {
                        self.formatted_directory_path(&path)
                            .ok()
                            .filter(|new_path| &path != new_path)
                            .map(|new_path| (path, new_path))
                    } else {
                        None
                    }
                };
                progress_bar.inc(1);
                result
            })
            .collect();

        progress_bar.finish_and_clear();

        // Sort by depth to rename children before parents, avoiding renaming conflicts
        results.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));

        results
    }

    /// Move all files recursively from source directory to target directory
    fn move_directory_contents(&self, source_dir: &Path, target_dir: &Path) -> Result<bool> {
        let mut any_files_moved = false;

        for entry in WalkDir::new(source_dir)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let source_file = entry.path();
            let relative_path = source_file.strip_prefix(source_dir)?;
            let target_file = target_dir.join(relative_path);

            // Create parent directories if they don't exist
            if let Some(parent) = target_file.parent() {
                fs::create_dir_all(parent)?;
            }

            if target_file.exists() {
                if self.config.overwrite {
                    if self.config.verbose {
                        println!("Overwriting existing file: {}", target_file.display());
                    }
                    fs::rename(source_file, &target_file)?;
                    any_files_moved = true;
                } else {
                    println!(
                        "{}",
                        format!("Skipping existing file: {}", target_file.display()).yellow()
                    );
                }
            } else {
                if self.config.verbose {
                    println!("Moving file: {}", target_file.display());
                }
                fs::rename(source_file, &target_file)?;
                any_files_moved = true;
            }
        }

        Ok(any_files_moved)
    }

    /// Remove empty directories recursively from the bottom up
    fn remove_empty_directories(&self, dir: &Path) -> Result<()> {
        for entry in WalkDir::new(dir)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_dir())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        // Process from deepest to shallowest
        {
            let dir_path = entry.path();
            if dir_path != dir && crate::is_directory_empty(dir_path) {
                if self.config.verbose {
                    println!("Removing empty directory: {}", dir_path.display());
                }
                fs::remove_dir(dir_path)?;
            }
        }

        // Finally, try to remove the source directory itself
        if crate::is_directory_empty(dir) {
            if self.config.verbose {
                println!("Removing empty source directory: {}", dir.display());
            }
            fs::remove_dir(dir)?;
        }

        Ok(())
    }

    /// Rename all given path pairs or just print changes if dryrun is enabled.
    fn rename_paths(&self, paths: Vec<(PathBuf, PathBuf)>) -> usize {
        let mut num_renamed: usize = 0;
        let max_items = paths.len();
        let max_chars = paths.len().checked_ilog10().map_or(1, |d| d as usize + 1);
        for (index, (path, mut new_path)) in paths.into_iter().enumerate() {
            let old_str = crate::get_relative_path_or_filename(&path, &self.root);
            let mut new_str = crate::get_relative_path_or_filename(&new_path, &self.root);
            let number = format!("{:>max_chars$} / {max_items}", index + 1);

            let capitalization_change_only = if new_str.to_lowercase() == old_str.to_lowercase() {
                // File path contains only capitalization changes:
                // Need to use a temp file to work around case-insensitive file systems.
                true
            } else {
                false
            };

            // Handle directory renaming when target already exists
            // Copy files from source to target directory and remove source directory if empty
            let is_directory_merge = path.is_dir() && new_path.exists() && !capitalization_change_only;
            let mut skip_rename = false;

            if !capitalization_change_only && new_path.exists() && !self.config.overwrite && !is_directory_merge {
                if self.config.increment_name {
                    match Self::get_incremented_path(&new_path) {
                        Ok(incremented_path) => {
                            new_str = crate::get_relative_path_or_filename(&incremented_path, &self.root);
                            new_path = incremented_path;
                        }
                        Err(e) => {
                            println!("Error while incrementing name: {e}");
                            continue;
                        }
                    }
                } else {
                    skip_rename = true;
                }
            }

            if self.config.dryrun {
                println!("{}", format!("Dryrun {number}:").bold().cyan());
                crate::show_diff(&old_str, &new_str);

                if is_directory_merge {
                    println!("Would merge directory: {old_str} -> {new_str}");
                }
                num_renamed += 1;
                continue;
            }

            println!("{}", format!("Rename {number}:").bold().magenta());
            crate::show_diff(&old_str, &new_str);

            if is_directory_merge {
                if self.config.verbose {
                    println!("Merging directory: {old_str} -> {new_str}");
                }

                match self.move_directory_contents(&path, &new_path) {
                    Ok(_files_moved) => {
                        // Try to remove the source directory and any empty parent directories
                        if let Err(e) = self.remove_empty_directories(&path) {
                            eprintln!("{}", format!("Could not remove empty directories: {e}").red());
                        }
                        num_renamed += 1;
                    }
                    Err(e) => {
                        eprintln!("{}", format!("Error merging directories: {old_str}\n{e}").red());
                    }
                }
                continue;
            }

            if skip_rename {
                println!(
                    "{}",
                    format!("Skipping rename to already existing file: {new_str}").yellow()
                );
                continue;
            }

            let rename_result = if capitalization_change_only {
                Self::rename_with_temp_file(&path, &new_path)
            } else {
                fs::rename(&path, &new_path)
            };

            match rename_result {
                Ok(()) => {
                    num_renamed += 1;
                }
                Err(e) => {
                    eprintln!("{}", format!("Error renaming: {old_str}\n{e}").red());
                }
            }
        }
        num_renamed
    }

    /// Get the full path with formatted filename and extension.
    fn formatted_filepath(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_file() {
            anyhow::bail!("Path is not a file")
        }

        if let Ok((file_name, file_extension)) = crate::get_normalized_file_name_and_extension(path) {
            let new_file = format!("{}.{}", self.format_name(&file_name), file_extension.to_lowercase());
            let new_path = path.with_file_name(new_file);
            Ok(new_path)
        } else {
            Err(anyhow!("Failed to get filename"))
        }
    }

    /// Get the full path with formatted filename and extension.
    fn formatted_directory_path(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_dir() {
            anyhow::bail!("Path is not a directory")
        }

        let directory_name = crate::os_str_to_string(path.file_name().context("Failed to get directory name")?);
        let formatted_name = self.format_directory_name(&directory_name);

        Ok(path.with_file_name(formatted_name))
    }

    /// Build regex patterns for prefix directory date reordering.
    fn build_prefix_dir_regexes(name: &str) -> Result<[(Regex, String); 4]> {
        let escaped_name = regex::escape(name);

        let prefix_regex_start_full_date = Regex::new(&format!(
            "^({escaped_name}\\.)(.{{1,32}}?\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.(?:1[0-2]|0?[1-9])\\.(?:[12]\\d|3[01]|0?[1-9])\\.)",
        ))
        .context("Failed to compile prefix dir full date regex")?;

        let prefix_regex_start_year = Regex::new(&format!(
            "^({escaped_name}\\.)(.{{1,32}}?\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.)",
        ))
        .context("Failed to compile prefix dir year regex")?;

        let prefix_regex_middle_full_date = Regex::new(&format!(
            "^(.{{1,32}}?\\.)({escaped_name}\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.(?:1[0-2]|0?[1-9])\\.(?:[12]\\d|3[01]|0?[1-9])\\.)",
        ))
        .context("Failed to compile prefix dir middle full date regex")?;

        let prefix_regex_middle_year = Regex::new(&format!(
            "^(.{{1,32}}?\\.)({escaped_name}\\.)((20(?:0[0-9]|1[0-9]|2[0-5]))\\.)",
        ))
        .context("Failed to compile prefix dir middle year regex")?;

        Ok([
            (prefix_regex_start_full_date, "$2.$3.$1.".to_string()),
            (prefix_regex_start_year, "$2.$3.$1.".to_string()),
            (prefix_regex_middle_full_date, "$1.$3.$2.".to_string()),
            (prefix_regex_middle_year, "$1.$3.$2.".to_string()),
        ])
    }

    /// Rename a file with an intermediate temp file to work around case-insensitive file systems.
    fn rename_with_temp_file(path: &PathBuf, new_path: &PathBuf) -> std::io::Result<()> {
        let temp_file = crate::append_extension_to_path(new_path.clone(), ".tmp");
        fs::rename(path, &temp_file)?;
        fs::rename(&temp_file, new_path)
    }

    fn get_incremented_path(original: &Path) -> Result<PathBuf> {
        let mut index = 2;
        let parent = original.parent().unwrap_or_else(|| Path::new(""));
        let (name, extension) = crate::get_normalized_file_name_and_extension(original)?;
        loop {
            let file_name = format!("{name}.{index}.{extension}");
            let new_path = parent.join(file_name);
            if !new_path.exists() {
                return Ok(new_path);
            }
            index += 1;
        }
    }

    /// Create a progress bar that is hidden during tests.
    fn create_progress_bar(len: u64) -> ProgressBar {
        #[cfg(test)]
        {
            let _ = len;
            ProgressBar::hidden()
        }
        #[cfg(not(test))]
        {
            let progress_bar = ProgressBar::new(len);
            progress_bar.set_style(
                ProgressStyle::default_bar()
                    .template(PROGRESS_BAR_TEMPLATE)
                    .expect("Failed to set progress bar template")
                    .progress_chars(PROGRESS_BAR_CHARS),
            );
            progress_bar
        }
    }
}

impl fmt::Display for DotRename {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Root: {}", self.root.display())?;
        write!(f, "{}", self.config)
    }
}

#[cfg(test)]
mod test_prefix_suffix_options {
    use super::*;
    use std::fs::{self, File};
    use tempfile::TempDir;

    /// Helper to create a test file with given name.
    fn create_test_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        File::create(&path).expect("Failed to create test file");
        path
    }

    /// Helper to create a subdirectory.
    fn create_subdir(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        fs::create_dir_all(&path).expect("Failed to create subdirectory");
        path
    }

    #[test]
    fn test_prefix_with_explicit_name() {
        let dots = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("My.Prefix".to_string()),
                ..Default::default()
            },
            path_given: false,
        };

        assert_eq!(dots.format_name("some file name"), "My.Prefix.Some.File.Name");
        assert_eq!(dots.format_name("another_file"), "My.Prefix.Another.File");
    }

    #[test]
    fn test_suffix_with_explicit_name() {
        let dots = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                suffix: Some("My.Suffix".to_string()),
                ..Default::default()
            },
            path_given: false,
        };

        assert_eq!(dots.format_name("some file name"), "Some.File.Name.My.Suffix");
        assert_eq!(dots.format_name("another_file"), "Another.File.My.Suffix");
    }

    #[test]
    fn test_prefix_dir_uses_root_directory_name() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Test Directory");
        create_test_file(&root, "some_file.txt");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        assert!(
            new_name.starts_with("Test.Directory."),
            "Expected prefix 'Test.Directory.', got: {new_name}"
        );
    }

    #[test]
    fn test_suffix_dir_uses_root_directory_name() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Test Directory");
        create_test_file(&root, "some_file.txt");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                suffix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        assert!(
            new_name.contains(".Test.Directory."),
            "Expected suffix 'Test.Directory', got: {new_name}"
        );
    }

    #[test]
    fn test_prefix_dir_recursive_uses_parent_directory_names() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root Dir");
        let sub1 = create_subdir(&root, "Sub One");
        let sub2 = create_subdir(&root, "Sub Two");

        create_test_file(&sub1, "file_in_sub1.txt");
        create_test_file(&sub2, "file_in_sub2.txt");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 2);

        // Check that each file uses its parent directory name as prefix
        for (old_path, new_path) in &files {
            let parent_name = old_path
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace(' ', ".");

            let new_name = new_path.file_name().unwrap().to_string_lossy();
            assert!(
                new_name.starts_with(&parent_name),
                "File in '{}' should have prefix '{}', got: {}",
                old_path.parent().unwrap().display(),
                parent_name,
                new_name
            );
        }
    }

    #[test]
    fn test_suffix_dir_recursive_uses_parent_directory_names() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root Dir");
        let sub1 = create_subdir(&root, "Sub One");
        let sub2 = create_subdir(&root, "Sub Two");

        create_test_file(&sub1, "file_in_sub1.txt");
        create_test_file(&sub2, "file_in_sub2.txt");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                suffix_dir: true,
                suffix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 2);

        // Check that each file uses its parent directory name as suffix
        for (old_path, new_path) in &files {
            let parent_name = old_path
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace(' ', ".");

            let new_name = new_path.file_stem().unwrap().to_string_lossy();
            assert!(
                new_name.ends_with(&parent_name),
                "File in '{}' should have suffix '{}', got: {}",
                old_path.parent().unwrap().display(),
                parent_name,
                new_name
            );
        }
    }

    #[test]
    fn test_prefix_dir_start_prevents_date_reordering() {
        let dots_without_start = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("Artist".to_string()),
                regex_replace_after: DotRename::build_prefix_dir_regexes("Artist").unwrap().to_vec(),
                ..Default::default()
            },
            path_given: false,
        };

        let dots_with_start = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("Artist".to_string()),
                prefix_dir_start: true,
                // No regex_replace_after when prefix_dir_start is true
                ..Default::default()
            },
            path_given: false,
        };

        // Without --start, date causes reordering (prefix moves after date)
        // The regex requires: prefix.content.year. (with trailing content after year)
        assert_eq!(
            dots_without_start.format_name("song title 2024 extra"),
            "Song.Title.2024.Artist.Extra"
        );

        // With --start, prefix stays at start
        assert_eq!(
            dots_with_start.format_name("song title 2024 extra"),
            "Artist.Song.Title.2024.Extra"
        );
    }

    #[test]
    fn test_prefix_recursive_different_subdirs_get_different_prefixes() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Music");
        let artist1 = create_subdir(&root, "Artist One");
        let artist2 = create_subdir(&root, "Artist Two");

        create_test_file(&artist1, "song1.mp3");
        create_test_file(&artist1, "song2.mp3");
        create_test_file(&artist2, "track1.mp3");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 3);

        let artist1_files: Vec<_> = files
            .iter()
            .filter(|(old, _)| old.parent().unwrap().ends_with("Artist One"))
            .collect();
        let artist2_files: Vec<_> = files
            .iter()
            .filter(|(old, _)| old.parent().unwrap().ends_with("Artist Two"))
            .collect();

        assert_eq!(artist1_files.len(), 2);
        assert_eq!(artist2_files.len(), 1);

        // Artist One files should have "Artist.One" prefix
        for (_, new_path) in artist1_files {
            let name = new_path.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with("Artist.One."),
                "Expected 'Artist.One.' prefix, got: {name}"
            );
        }

        // Artist Two files should have "Artist.Two" prefix
        for (_, new_path) in artist2_files {
            let name = new_path.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with("Artist.Two."),
                "Expected 'Artist.Two.' prefix, got: {name}"
            );
        }
    }

    #[test]
    fn test_prefix_and_suffix_cannot_both_be_set() {
        // This is enforced by clap conflicts_with, but test the behavior if both were set
        let dots = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("Prefix".to_string()),
                suffix: Some("Suffix".to_string()),
                ..Default::default()
            },
            path_given: false,
        };

        // Both should be applied (prefix first, then suffix)
        let result = dots.format_name("file name");
        assert_eq!(result, "Prefix.File.Name.Suffix");
    }

    #[test]
    fn test_recursive_mode_includes_root_and_subdir_files() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root");
        let sub = create_subdir(&root, "Subdir");

        // File in root should use "Root" as parent
        create_test_file(&root, "root_file.txt");
        // File in subdir should use "Subdir" as parent
        create_test_file(&sub, "sub_file.txt");

        let dots = DotRename {
            root: root.clone(),
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 2);

        // Find the root file and subdir file
        let root_file = files.iter().find(|(old, _)| old.parent().unwrap() == root);
        let sub_file = files.iter().find(|(old, _)| old.parent().unwrap() == sub);

        assert!(root_file.is_some(), "Root file should be included");
        assert!(sub_file.is_some(), "Subdir file should be included");

        // Root file should have "Root" prefix
        let (_, new_root) = root_file.unwrap();
        assert!(
            new_root.file_name().unwrap().to_string_lossy().starts_with("Root."),
            "Root file should have 'Root.' prefix"
        );

        // Subdir file should have "Subdir" prefix
        let (_, new_sub) = sub_file.unwrap();
        assert!(
            new_sub.file_name().unwrap().to_string_lossy().starts_with("Subdir."),
            "Subdir file should have 'Subdir.' prefix"
        );
    }

    #[test]
    fn test_prefix_dir_with_deeply_nested_directories() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root");
        let level1 = create_subdir(&root, "Level One");
        let level2 = create_subdir(&level1, "Level Two");
        let level3 = create_subdir(&level2, "Level Three");

        create_test_file(&level3, "deep_file.txt");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        // Should use immediate parent "Level Three", not root
        assert!(
            new_name.starts_with("Level.Three."),
            "Expected 'Level.Three.' prefix for deeply nested file, got: {new_name}"
        );
    }

    #[test]
    fn test_prefix_dir_non_recursive_uses_root_for_all() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root Dir");
        let sub = create_subdir(&root, "Subdir");

        create_test_file(&root, "file1.txt");
        create_test_file(&sub, "file2.txt");

        // Non-recursive prefix_dir should use root name for direct children only
        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                recurse: false,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        // Only root level file should be included (non-recursive)
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        assert!(
            new_name.starts_with("Root.Dir."),
            "Expected 'Root.Dir.' prefix, got: {new_name}"
        );
    }

    #[test]
    fn test_prefix_with_special_characters_in_directory_name() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Test (2024) [Special]");
        create_test_file(&root, "file.txt");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        // Special characters should be normalized
        assert!(
            new_name.starts_with("Test.2024.Special."),
            "Expected normalized prefix, got: {new_name}"
        );
    }

    #[test]
    fn test_suffix_with_special_characters_in_directory_name() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Artist & Band");
        create_test_file(&root, "song.mp3");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                suffix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_stem().unwrap().to_string_lossy();
        assert!(
            new_name.ends_with("Artist.&.Band")
                || new_name.ends_with("Artist.Band")
                || new_name.ends_with("Artist.and.Band"),
            "Expected normalized suffix, got: {new_name}"
        );
    }

    #[test]
    fn test_prefix_already_present_in_filename() {
        let dots = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("Artist.Name".to_string()),
                ..Default::default()
            },
            path_given: false,
        };

        // If filename already contains the prefix, it should not duplicate
        assert_eq!(dots.format_name("Artist Name - Song Title"), "Artist.Name.Song.Title");
        assert_eq!(dots.format_name("artist.name.song.title"), "Artist.Name.Song.Title");
    }

    #[test]
    fn test_suffix_already_present_in_filename() {
        let dots = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                suffix: Some("2024".to_string()),
                ..Default::default()
            },
            path_given: false,
        };

        // If filename already ends with suffix, should not duplicate
        assert_eq!(dots.format_name("song title 2024"), "Song.Title.2024");
    }

    #[test]
    fn test_prefix_dir_with_numeric_directory_name() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "2024");
        create_test_file(&root, "file.txt");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 1);

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        assert!(
            new_name.starts_with("2024."),
            "Expected '2024.' prefix, got: {new_name}"
        );
    }

    #[test]
    fn test_prefix_dir_start_with_5_digit_filename() {
        let dots = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("Prefix".to_string()),
                regex_replace_after: DotRename::build_prefix_dir_regexes("Prefix").unwrap().to_vec(),
                ..Default::default()
            },
            path_given: false,
        };

        // 5+ digit ID at start should keep prefix at start (skip reordering)
        assert_eq!(
            dots.format_name("12345 content 2024 extra"),
            "Prefix.12345.Content.2024.Extra"
        );

        // 4 digit number should allow reordering
        assert_eq!(
            dots.format_name("1234 content 2024 extra"),
            "1234.Content.2024.Prefix.Extra"
        );
    }

    #[test]
    fn test_prefix_recursive_with_include_filter() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root");
        let sub = create_subdir(&root, "Subdir");

        create_test_file(&root, "include_this.txt");
        create_test_file(&root, "exclude_this.txt");
        create_test_file(&sub, "include_sub.txt");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                include: vec!["include".to_string()],
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 2, "Should only include files matching filter");

        for (old_path, _) in &files {
            let name = old_path.file_name().unwrap().to_string_lossy();
            assert!(
                name.contains("include"),
                "All files should match include filter, got: {name}"
            );
        }
    }

    #[test]
    fn test_prefix_recursive_with_exclude_filter() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root");

        create_test_file(&root, "keep_this.txt");
        create_test_file(&root, "exclude_this.txt");
        create_test_file(&root, "also_keep.txt");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                exclude: vec!["exclude".to_string()],
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 2, "Should exclude files matching filter");

        for (old_path, _) in &files {
            let name = old_path.file_name().unwrap().to_string_lossy();
            assert!(
                !name.contains("exclude"),
                "No files should match exclude filter, got: {name}"
            );
        }
    }

    #[test]
    fn test_suffix_recursive_different_subdirs() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Videos");
        let cat1 = create_subdir(&root, "Category One");
        let cat2 = create_subdir(&root, "Category Two");

        create_test_file(&cat1, "video1.mp4");
        create_test_file(&cat2, "video2.mp4");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                suffix_dir: true,
                suffix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 2);

        let cat1_file = files
            .iter()
            .find(|(old, _)| old.parent().unwrap().ends_with("Category One"));
        let cat2_file = files
            .iter()
            .find(|(old, _)| old.parent().unwrap().ends_with("Category Two"));

        assert!(cat1_file.is_some());
        assert!(cat2_file.is_some());

        let (_, new_cat1) = cat1_file.unwrap();
        let (_, new_cat2) = cat2_file.unwrap();

        let name1 = new_cat1.file_stem().unwrap().to_string_lossy();
        let name2 = new_cat2.file_stem().unwrap().to_string_lossy();

        assert!(
            name1.ends_with("Category.One"),
            "Expected 'Category.One' suffix, got: {name1}"
        );
        assert!(
            name2.ends_with("Category.Two"),
            "Expected 'Category.Two' suffix, got: {name2}"
        );
    }

    #[test]
    fn test_prefix_with_date_reordering_full_date() {
        let dots_without_start = DotRename {
            root: PathBuf::default(),
            config: DotRenameConfig {
                prefix: Some("Show".to_string()),
                regex_replace_after: DotRename::build_prefix_dir_regexes("Show").unwrap().to_vec(),
                ..Default::default()
            },
            path_given: false,
        };

        // Full date pattern: year.month.day
        assert_eq!(
            dots_without_start.format_name("episode title 2024.06.15 extra"),
            "Episode.Title.2024.06.15.Show.Extra"
        );
    }

    #[test]
    fn test_prefix_dir_empty_directory() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Empty Dir");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert!(files.is_empty(), "Empty directory should have no files to rename");
    }

    #[test]
    fn test_prefix_recursive_empty_subdirectories() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Root");
        let _empty_sub = create_subdir(&root, "Empty Sub");
        let has_files = create_subdir(&root, "Has Files");

        create_test_file(&has_files, "file.txt");

        let dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                prefix_dir_recursive: true,
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_with_recursive_prefix_suffix();
        assert_eq!(files.len(), 1, "Only files from non-empty dirs should be included");

        let (_, new_path) = &files[0];
        let new_name = new_path.file_name().unwrap().to_string_lossy();
        assert!(
            new_name.starts_with("Has.Files."),
            "Expected 'Has.Files.' prefix, got: {new_name}"
        );
    }

    #[test]
    fn test_multiple_files_same_directory_get_same_prefix() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Album Name");

        create_test_file(&root, "track01.mp3");
        create_test_file(&root, "track02.mp3");
        create_test_file(&root, "track03.mp3");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 3);

        // All files should have the same prefix
        for (_, new_path) in &files {
            let new_name = new_path.file_name().unwrap().to_string_lossy();
            assert!(
                new_name.starts_with("Album.Name."),
                "All files should have 'Album.Name.' prefix, got: {new_name}"
            );
        }
    }

    #[test]
    fn test_prefix_preserves_file_extension() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Test");

        create_test_file(&root, "file.TXT");
        create_test_file(&root, "video.MP4");
        create_test_file(&root, "image.JPEG");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 3);

        for (_, new_path) in &files {
            let ext = new_path.extension().unwrap().to_string_lossy();
            // Extensions should be lowercased
            assert!(
                ext.chars().all(|c| c.is_lowercase() || c.is_numeric()),
                "Extension should be lowercase, got: {ext}"
            );
        }
    }
}
