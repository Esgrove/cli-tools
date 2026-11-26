//! Dot rename implementation for formatting filenames with dot separators.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{fmt, fs};

use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use indicatif::ProgressBar;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use rayon::prelude::*;
use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::normalize_extension;
use crate::{DotFormat, DotRenameConfig};
use cli_tools_common::get_normalized_file_name_and_extension;
use cli_tools_common::os_str_to_string;
use cli_tools_common::{OverwritePolicy, RenameOutcome, TargetReservations};

#[cfg(not(test))]
const PROGRESS_BAR_CHARS: &str = "=> ";
#[cfg(not(test))]
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.cyan/blue} {pos}/{len} {percent}%";

/// How to handle the initial destination for a rename.
#[derive(Clone, Copy)]
enum RenameTarget {
    Available,
    MergeDirectory,
    Conflict,
}

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

        let num_renamed = self.rename_paths(&paths_to_rename);
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

    /// Create a formatter that borrows the config.
    const fn formatter(&self) -> DotFormat<'_> {
        DotFormat::new(&self.config)
    }

    /// Check the configured literal filters against the entire input path.
    fn matches_filters(&self, path: &Path) -> bool {
        let path = cli_tools_common::path_to_string(path);
        self.config.include.iter().all(|pattern| path.contains(pattern))
            && (self.config.include_any.is_empty()
                || self.config.include_any.iter().any(|pattern| path.contains(pattern)))
            && !self.config.exclude.iter().any(|pattern| path.contains(pattern))
    }

    /// Get all files that need to be renamed.
    fn gather_files_to_rename(&mut self) -> Result<Vec<(PathBuf, PathBuf)>> {
        // Handle recursive prefix/suffix separately - they compute per-file
        if self.config.prefix_dir_recursive || self.config.suffix_dir_recursive {
            return Ok(self.gather_files_with_recursive_prefix_suffix());
        }

        // Non-recursive prefix/suffix: use root directory name for all files
        if self.config.prefix_dir || self.config.suffix_dir {
            self.configure_prefix_suffix_from_directory()?;
        }

        if self.root.is_file() {
            if !self.matches_filters(&self.root) {
                return Ok(Vec::new());
            }
            if self.config.verbose {
                println!("{}", format!("Formatting file {}", self.root.display()).bold());
            }
            return Ok(self
                .formatted_filepath(&self.root)
                .ok()
                .filter(|new_path| Self::path_changed(&self.root, new_path))
                .map_or_default(|new_path| vec![(self.root.clone(), new_path)]));
        }

        if self.config.verbose {
            println!("{}", format!("Formatting files under {}", self.root.display()).bold());
        }

        let max_depth = if self.config.recurse { 100 } else { 1 };

        // Collect all file paths first
        let paths: Vec<PathBuf> = WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|e| !cli_tools_common::should_skip_entry(e))
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .collect();

        let progress_bar = Self::create_progress_bar(paths.len() as u64);

        // Filter and format files in parallel
        let mut results: Vec<_> = paths
            .into_par_iter()
            .filter_map(|path| {
                let result = if self.matches_filters(&path) {
                    self.formatted_filepath(&path)
                        .ok()
                        .filter(|new_path| Self::path_changed(&path, new_path))
                        .map(|new_path| (path, new_path))
                } else {
                    None
                };
                progress_bar.inc(1);
                result
            })
            .collect();

        progress_bar.finish_and_clear();

        // Sort sequentially after parallel collection
        results.sort_by_cached_key(|(path, _)| cli_tools_common::path_to_string(path).to_lowercase());

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
            .filter_entry(|e| !cli_tools_common::should_skip_entry(e))
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|p| p.is_file())
            .collect();

        let progress_bar = Self::create_progress_bar(paths.len() as u64);

        // Process files - cannot use parallel iteration since we need to modify prefix/suffix per file
        let mut results: Vec<(PathBuf, PathBuf)> = Vec::new();

        for path in paths {
            if self.matches_filters(&path)
                && let Some(result) = self.format_file_with_parent_prefix_suffix(&path)
                && Self::path_changed(&path, &result)
            {
                results.push((path, result));
            }
            progress_bar.inc(1);
        }

        progress_bar.finish_and_clear();

        // Sort results
        results.sort_by_cached_key(|(path, _)| cli_tools_common::path_to_string(path).to_lowercase());

        results
    }

    /// Get all directories that need to be renamed.
    fn gather_directories_to_rename(&self, path_specified: bool) -> Vec<(PathBuf, PathBuf)> {
        // Path specified without recurse - rename only that specific directory
        if path_specified && !self.config.recurse {
            return self
                .formatted_directory_path(&self.root)
                .ok()
                .filter(|new_path| Self::path_changed(&self.root, new_path))
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
            .filter_entry(|e| !cli_tools_common::should_skip_entry(e))
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(walkdir::DirEntry::into_path)
            .collect();

        let progress_bar = Self::create_progress_bar(paths.len() as u64);

        let mut results: Vec<_> = paths
            .into_par_iter()
            .filter_map(|path| {
                let result = if self.matches_filters(&path) {
                    self.formatted_directory_path(&path)
                        .ok()
                        .filter(|new_path| Self::path_changed(&path, new_path))
                        .map(|new_path| (path, new_path))
                } else {
                    None
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
            cli_tools_common::check_destination_parents(target_dir, &target_file)?;

            // Create parent directories if they don't exist
            if let Some(parent) = target_file.parent() {
                fs::create_dir_all(parent)?;
            }

            if fs::symlink_metadata(&target_file).is_ok() {
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
            if dir_path != dir && cli_tools_common::is_directory_empty(dir_path) {
                if self.config.verbose {
                    println!("Removing empty directory: {}", dir_path.display());
                }
                fs::remove_dir(dir_path)?;
            }
        }

        // Finally, try to remove the source directory itself
        if cli_tools_common::is_directory_empty(dir) {
            if self.config.verbose {
                println!("Removing empty source directory: {}", dir.display());
            }
            fs::remove_dir(dir)?;
        }

        Ok(())
    }

    /// Rename all given path pairs or just print changes if dryrun is enabled.
    fn rename_paths(&self, paths: &[(PathBuf, PathBuf)]) -> usize {
        let mut num_renamed: usize = 0;
        let max_items = paths.len();
        let max_chars = paths.len().checked_ilog10().map_or(1, |d| d as usize + 1);
        // Track destinations claimed earlier in this batch so --force never overwrites
        // another renamed source and dry-run reports the same collision outcomes as a real run.
        let mut reservations = TargetReservations::new();
        let pending_sources = Self::index_sources(paths);

        for (index, (path, target)) in paths.iter().enumerate() {
            let mut new_path = target.as_path();
            let old_str = cli_tools_common::get_relative_path_or_filename(path, &self.root);
            let mut new_str = cli_tools_common::get_relative_path_or_filename(new_path, &self.root);
            let number = format!("{:>max_chars$} / {max_items}", index + 1);

            let disposition = self.classify_target(path, new_path, index, &pending_sources, &reservations);
            let is_directory_merge = matches!(disposition, RenameTarget::MergeDirectory);
            let mut skip_rename = false;
            let incremented_path;

            if matches!(disposition, RenameTarget::Conflict) {
                if self.config.increment_name {
                    match Self::get_incremented_path_avoiding(new_path, &reservations) {
                        Ok(incremented) => {
                            incremented_path = incremented;
                            new_path = &incremented_path;
                            new_str = cli_tools_common::get_relative_path_or_filename(new_path, &self.root);
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

            if !skip_rename && !is_directory_merge {
                reservations.reserve(new_path);
            }

            if self.config.dryrun {
                println!("{}", format!("Dryrun {number}:").bold().cyan());
                cli_tools_common::show_diff(&old_str, &new_str);

                // A dry run reports and counts the same skips a real run makes.
                if skip_rename {
                    println!(
                        "{}",
                        format!("Would skip rename to already existing file: {new_str}").yellow()
                    );
                    continue;
                }

                if is_directory_merge {
                    println!("Would merge directory: {old_str} -> {new_str}");
                }
                num_renamed += 1;
                continue;
            }

            println!("{}", format!("Rename {number}:").bold().magenta());
            cli_tools_common::show_diff(&old_str, &new_str);

            if is_directory_merge {
                if self.config.verbose {
                    println!("Merging directory: {old_str} -> {new_str}");
                }

                match self.move_directory_contents(path, new_path) {
                    Ok(_files_moved) => {
                        // Try to remove the source directory and any empty parent directories
                        if let Err(e) = self.remove_empty_directories(path) {
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

            let policy = if self.config.overwrite {
                OverwritePolicy::Overwrite
            } else {
                OverwritePolicy::Skip
            };
            match cli_tools_common::safe_rename(path, new_path, policy) {
                Ok(RenameOutcome::SkippedExisting) => {
                    println!(
                        "{}",
                        format!("Skipping rename to already existing file: {new_str}").yellow()
                    );
                }
                Ok(_) => {
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

        if let Ok((file_name, file_extension)) = get_normalized_file_name_and_extension(path) {
            let formatted_stem = self.format_name(&file_name);
            if formatted_stem.is_empty() {
                anyhow::bail!("Formatted filename stem is empty");
            }
            let normalized_extension = normalize_extension(&file_extension);
            let new_file = if normalized_extension.is_empty() {
                formatted_stem
            } else {
                format!("{formatted_stem}.{normalized_extension}")
            };
            let new_path = path.with_file_name(new_file);
            Ok(new_path)
        } else {
            Err(anyhow!("Failed to get filename"))
        }
    }

    /// Get the full path with formatted directory name.
    fn formatted_directory_path(&self, path: &Path) -> Result<PathBuf> {
        if !path.is_dir() {
            anyhow::bail!("Path is not a directory")
        }

        let directory_name = os_str_to_string(path.file_name().context("Failed to get directory name")?);
        let formatted_name = self.format_directory_name(&directory_name);
        if formatted_name.is_empty() {
            anyhow::bail!("Formatted directory name is empty");
        }

        Ok(path.with_file_name(formatted_name))
    }

    /// Configure prefix or suffix from the root directory name.
    ///
    /// Sets up the prefix/suffix value, deduplication patterns,
    /// and regex reordering rules based on the directory name.
    fn configure_prefix_suffix_from_directory(&mut self) -> Result<()> {
        let formatted_dir = if self.root.is_dir() {
            cli_tools_common::get_normalized_dir_name(&self.root)?
        } else {
            let parent_dir = self.root.parent().context("Failed to get parent dir")?;
            cli_tools_common::get_normalized_dir_name(parent_dir)?
        };
        let name = self.format_name(&formatted_dir);
        if self.config.prefix_dir && self.config.prefix.is_none() {
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
        } else if self.config.suffix_dir && self.config.suffix.is_none() {
            if self.config.verbose {
                println!("Using directory suffix: {name}");
            }
            self.config.suffix = Option::from(name);
        }
        Ok(())
    }

    /// Build regex patterns for prefix directory date reordering.
    fn build_prefix_dir_regexes(name: &str) -> Result<[(Regex, String); 4]> {
        let escaped_name = regex::escape(name);

        let prefix_regex_start_full_date = Regex::new(&format!(
            "^({escaped_name}\\.)(.{{1,32}}?\\.)((20[0-9]{{2}})\\.(?:1[0-2]|0?[1-9])\\.(?:[12]\\d|3[01]|0?[1-9])\\.)",
        ))
        .context("Failed to compile prefix dir full date regex")?;

        let prefix_regex_start_year = Regex::new(&format!("^({escaped_name}\\.)(.{{1,32}}?\\.)((20[0-9]{{2}})\\.)"))
            .context("Failed to compile prefix dir year regex")?;

        let prefix_regex_middle_full_date = Regex::new(&format!(
            "^(.{{1,32}}?\\.)({escaped_name}\\.)((20[0-9]{{2}})\\.(?:1[0-2]|0?[1-9])\\.(?:[12]\\d|3[01]|0?[1-9])\\.)"
        ))
        .context("Failed to compile prefix dir middle full date regex")?;

        let prefix_regex_middle_year = Regex::new(&format!("^(.{{1,32}}?\\.)({escaped_name}\\.)((20[0-9]{{2}})\\.)"))
            .context("Failed to compile prefix dir middle year regex")?;

        Ok([
            (prefix_regex_start_full_date, "$2.$3.$1.".to_string()),
            (prefix_regex_start_year, "$2.$3.$1.".to_string()),
            (prefix_regex_middle_full_date, "$1.$3.$2.".to_string()),
            (prefix_regex_middle_year, "$1.$3.$2.".to_string()),
        ])
    }

    /// Compare paths as strings to detect changes on case-insensitive filesystems.
    ///
    /// `PathBuf` comparison is case-insensitive on some systems,
    /// which misses case-only and extension-only changes like `Photo.JPG` → `Photo.jpg`.
    fn path_changed(old: &Path, new: &Path) -> bool {
        cli_tools_common::path_to_string(old) != cli_tools_common::path_to_string(new)
    }

    /// Index source paths by possible aliases and retain their positions in the batch.
    fn index_sources(paths: &[(PathBuf, PathBuf)]) -> HashMap<String, Vec<(usize, &Path)>> {
        let mut sources = HashMap::<_, Vec<_>>::new();
        for (index, (path, _)) in paths.iter().enumerate() {
            sources
                .entry(Self::source_lookup_key(path))
                .or_default()
                .push((index, path.as_path()));
        }
        sources
    }

    /// Decide whether the destination is usable, mergeable, or needs a different path.
    fn classify_target(
        &self,
        source: &Path,
        target: &Path,
        index: usize,
        sources: &HashMap<String, Vec<(usize, &Path)>>,
        reservations: &TargetReservations,
    ) -> RenameTarget {
        if let Ok(metadata) = fs::symlink_metadata(target)
            && !cli_tools_common::paths_refer_to_same_entry(source, target)
        {
            let is_pending_source = sources.get(&Self::source_lookup_key(target)).is_some_and(|candidates| {
                candidates.iter().any(|&(source_index, candidate)| {
                    source_index > index && cli_tools_common::paths_refer_to_same_entry(candidate, target)
                })
            });
            if is_pending_source {
                return RenameTarget::Conflict;
            }

            // Only merge real directories. A symlink could point outside the run.
            if metadata.is_dir() && fs::symlink_metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
                return RenameTarget::MergeDirectory;
            }
            if !self.config.overwrite {
                return RenameTarget::Conflict;
            }
        }

        if reservations.contains(target) {
            RenameTarget::Conflict
        } else {
            RenameTarget::Available
        }
    }

    /// Group possible aliases of a source by case and Unicode spelling.
    /// The caller must still confirm entry identity on disk.
    fn source_lookup_key(path: &Path) -> String {
        path.to_string_lossy().to_lowercase().nfc().collect()
    }

    #[cfg(test)]
    fn get_incremented_path(original: &Path) -> Result<PathBuf> {
        Self::get_incremented_path_avoiding(original, &TargetReservations::new())
    }

    fn get_incremented_path_avoiding(original: &Path, reservations: &TargetReservations) -> Result<PathBuf> {
        let mut index = 2;
        let parent = original.parent().unwrap_or_else(|| Path::new(""));
        let (name, extension) = cli_tools_common::get_normalized_file_name_and_extension(original)?;
        loop {
            let file_name = if extension.is_empty() {
                format!("{name}.{index}")
            } else {
                format!("{name}.{index}.{extension}")
            };
            let new_path = parent.join(file_name);
            if fs::symlink_metadata(&new_path).is_err() && !reservations.contains(&new_path) {
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
    fn gather_files_skips_serato_directory() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_subdir(temp_dir.path(), "Music Library");
        let serato = create_subdir(&root, "_Serato_");
        let skipped_file = create_test_file(&serato, "skip me.txt");
        let normal_file = create_test_file(&root, "rename me.txt");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig {
                prefix: Some("Test".to_string()),
                recurse: true,
                ..Default::default()
            },
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");

        assert!(files.iter().any(|(path, _)| path == &normal_file));
        assert!(!files.iter().any(|(path, _)| path == &skipped_file));
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

        // JPEG should be normalized to jpg
        let jpeg_result = files.iter().find(|(old, _)| {
            old.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("JPEG"))
        });
        assert!(jpeg_result.is_some(), "Should find JPEG file in results");
        let (_, jpeg_new_path) = jpeg_result.expect("JPEG file not found");
        assert_eq!(
            jpeg_new_path.extension().unwrap().to_string_lossy(),
            "jpg",
            "JPEG should be normalized to jpg"
        );
    }
}

#[cfg(test)]
mod test_extension_only_rename {
    use super::*;
    use std::fs::{self, File};
    use tempfile::TempDir;

    fn create_test_dir(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        fs::create_dir_all(&path).expect("Failed to create test directory");
        path
    }

    fn create_test_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        File::create(&path).expect("Failed to create test file");
        path
    }

    #[test]
    fn jpeg_renamed_to_jpg_when_stem_unchanged() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_test_dir(temp_dir.path(), "TestDir");

        create_test_file(&root, "Semireal1.jpeg");
        create_test_file(&root, "Semireal2.jpeg");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig::default(),
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 2, "Both jpeg files should be detected for rename");

        for (old_path, new_path) in &files {
            assert_eq!(
                old_path.extension().unwrap().to_string_lossy(),
                "jpeg",
                "Original should have jpeg extension"
            );
            assert_eq!(
                new_path.extension().unwrap().to_string_lossy(),
                "jpg",
                "New path should have jpg extension"
            );
        }
    }

    #[test]
    fn uppercase_extension_renamed_when_stem_unchanged() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_test_dir(temp_dir.path(), "TestDir");

        create_test_file(&root, "Photo.JPG");
        create_test_file(&root, "Video.MP4");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig::default(),
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 2, "Both files with uppercase extensions should be renamed");

        for (_, new_path) in &files {
            let ext = new_path.extension().unwrap().to_string_lossy();
            assert!(
                ext.chars().all(|c| c.is_lowercase() || c.is_numeric()),
                "Extension should be lowercase, got: {ext}"
            );
        }
    }

    #[test]
    fn already_normalized_extension_skipped() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = create_test_dir(temp_dir.path(), "TestDir");

        create_test_file(&root, "Semireal1.jpg");
        create_test_file(&root, "Semireal2.png");

        let mut dots = DotRename {
            root,
            config: DotRenameConfig::default(),
            path_given: true,
        };

        let files = dots.gather_files_to_rename().expect("Failed to gather files");
        assert_eq!(files.len(), 0, "Already normalized files should not be renamed");
    }
}

#[cfg(test)]
mod rename_test_helpers {
    use super::*;
    use std::fs::{self, File};
    use tempfile::TempDir;

    /// Temporary directory with a visible name, so the walker does not skip it as hidden.
    pub fn temporary_directory() -> TempDir {
        tempfile::Builder::new()
            .prefix("dots_test")
            .tempdir()
            .expect("temporary directory should be created")
    }

    /// Create a file inside the directory, creating parent directories as needed.
    pub fn write(directory: &Path, relative: &str) -> PathBuf {
        let path = directory.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directories should be created");
        }
        File::create(&path).expect("file should be created");
        path
    }

    /// Create a directory inside the directory.
    pub fn directory(parent: &Path, relative: &str) -> PathBuf {
        let path = parent.join(relative);
        fs::create_dir_all(&path).expect("directory should be created");
        path
    }

    /// Names of the entries directly inside the directory, sorted.
    pub fn entries(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Instance rooted at the directory with the given config.
    pub fn dots(root: &Path, config: DotRenameConfig, path_given: bool) -> DotRename {
        DotRename::new(root.to_path_buf(), config, path_given)
    }
}

#[cfg(test)]
mod test_run_files {
    use super::rename_test_helpers::*;
    use super::*;

    #[test]
    fn renames_the_files_of_the_root_directory() {
        let temporary = temporary_directory();
        write(temporary.path(), "some file name.txt");
        write(temporary.path(), "another_file.txt");

        dots(temporary.path(), DotRenameConfig::default(), false)
            .run()
            .expect("the run should succeed");

        assert_eq!(
            entries(temporary.path()),
            vec!["Another.File.txt", "Some.File.Name.txt"]
        );
    }

    #[test]
    fn dryrun_leaves_every_file_alone() {
        let temporary = temporary_directory();
        write(temporary.path(), "some file name.txt");

        dots(
            temporary.path(),
            DotRenameConfig {
                dryrun: true,
                verbose: true,
                debug: true,
                ..Default::default()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["some file name.txt"]);
    }

    #[test]
    fn a_directory_of_already_formatted_files_is_left_alone() {
        let temporary = temporary_directory();
        write(temporary.path(), "Already.Formatted.txt");

        dots(
            temporary.path(),
            DotRenameConfig {
                verbose: true,
                ..Default::default()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Already.Formatted.txt"]);
    }

    #[test]
    fn a_single_file_path_is_renamed_on_its_own() {
        let temporary = temporary_directory();
        let file = write(temporary.path(), "some file.txt");
        write(temporary.path(), "other file.txt");

        dots(
            &file,
            DotRenameConfig {
                verbose: true,
                ..Default::default()
            },
            true,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Some.File.txt", "other file.txt"]);
    }

    #[test]
    fn renaming_directories_with_a_file_as_input_is_an_error() {
        let temporary = temporary_directory();
        let file = write(temporary.path(), "some file.txt");

        let error = dots(
            &file,
            DotRenameConfig {
                rename_directories: true,
                ..Default::default()
            },
            true,
        )
        .run()
        .expect_err("a file input should be rejected");

        assert!(error.to_string().contains("Cannot rename directories"));
    }

    #[test]
    fn recursing_renames_files_in_subdirectories() {
        let temporary = temporary_directory();
        let nested = directory(temporary.path(), "nested dir");
        write(&nested, "deep file.txt");

        dots(
            temporary.path(),
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(&nested), vec!["Deep.File.txt"]);
    }
}

#[cfg(test)]
mod test_run_directories {
    use super::rename_test_helpers::*;
    use super::*;

    fn config() -> DotRenameConfig {
        DotRenameConfig {
            rename_directories: true,
            ..Default::default()
        }
    }

    #[test]
    fn renames_the_directories_inside_the_root() {
        let temporary = temporary_directory();
        directory(temporary.path(), "some dir name");
        directory(temporary.path(), "another_dir");

        dots(temporary.path(), config(), false)
            .run()
            .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Another Dir", "Some Dir Name"]);
    }

    #[test]
    fn a_given_directory_renames_itself_rather_than_its_children() {
        let temporary = temporary_directory();
        let target = directory(temporary.path(), "some dir name");
        directory(&target, "inner dir");

        dots(&target, config(), true).run().expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Some Dir Name"]);
        assert_eq!(
            entries(&temporary.path().join("Some Dir Name")),
            vec!["inner dir"],
            "only the given directory should be renamed"
        );
    }

    #[test]
    fn recursing_renames_the_deepest_directories_first() {
        let temporary = temporary_directory();
        let outer = directory(temporary.path(), "outer dir");
        directory(&outer, "inner dir");

        dots(
            temporary.path(),
            DotRenameConfig {
                recurse: true,
                ..config()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Outer Dir"]);
        assert_eq!(entries(&temporary.path().join("Outer Dir")), vec!["Inner Dir"]);
    }

    #[test]
    fn the_include_filter_limits_the_renamed_directories() {
        let temporary = temporary_directory();
        directory(temporary.path(), "keep this");
        directory(temporary.path(), "skip that");

        dots(
            temporary.path(),
            DotRenameConfig {
                include: vec!["keep".to_string()],
                ..config()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Keep This", "skip that"]);
    }

    #[test]
    fn directory_mode_applies_exclude_and_include_any_without_include() {
        let temporary = temporary_directory();
        directory(temporary.path(), "match alpha");
        directory(temporary.path(), "match skip");
        directory(temporary.path(), "other dir");

        dots(
            temporary.path(),
            DotRenameConfig {
                include_any: vec!["match".to_string()],
                exclude: vec!["skip".to_string()],
                ..config()
            },
            false,
        )
        .run()
        .expect("run");

        assert_eq!(
            entries(temporary.path()),
            vec!["Match Alpha", "match skip", "other dir"]
        );
    }

    #[test]
    fn prefix_dir_reorders_dates_in_2026_and_later() {
        let temporary = temporary_directory();
        let show_dir = directory(temporary.path(), "Show");
        std::fs::write(show_dir.join("episode title 2026.06.15 extra.mp4"), b"video").expect("write");

        DotRename::new(
            show_dir.clone(),
            DotRenameConfig {
                prefix_dir: true,
                ..Default::default()
            },
            true,
        )
        .run()
        .expect("run");

        assert_eq!(entries(&show_dir), vec!["Episode.Title.2026.06.15.Show.Extra.mp4"]);
    }

    #[test]
    fn nothing_to_rename_reports_and_succeeds() {
        let temporary = temporary_directory();
        directory(temporary.path(), "Already Formatted");

        dots(
            temporary.path(),
            DotRenameConfig {
                verbose: true,
                ..config()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Already Formatted"]);
    }
}

#[cfg(test)]
mod test_directory_merge {
    use super::rename_test_helpers::*;
    use super::*;

    fn config() -> DotRenameConfig {
        DotRenameConfig {
            rename_directories: true,
            ..Default::default()
        }
    }

    #[test]
    fn merging_into_an_existing_directory_moves_the_files_and_drops_the_source() {
        let temporary = temporary_directory();
        let source = directory(temporary.path(), "some_dir");
        write(&source, "file one.txt");
        write(&source, "nested/file two.txt");
        directory(temporary.path(), "Some Dir");

        dots(
            temporary.path(),
            DotRenameConfig {
                verbose: true,
                ..config()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Some Dir"]);
        let target = temporary.path().join("Some Dir");
        assert!(target.join("file one.txt").is_file());
        assert!(target.join("nested").join("file two.txt").is_file());
        assert!(!source.exists(), "the emptied source directory should be removed");
    }

    #[test]
    fn a_conflicting_file_is_kept_unless_overwrite_is_set() {
        let temporary = temporary_directory();
        let source = directory(temporary.path(), "some_dir");
        std::fs::write(source.join("same.txt"), b"from source").expect("file should be written");
        let target = directory(temporary.path(), "Some Dir");
        std::fs::write(target.join("same.txt"), b"from target").expect("file should be written");

        dots(temporary.path(), config(), false)
            .run()
            .expect("the run should succeed");

        let content = std::fs::read_to_string(target.join("same.txt")).expect("the file should exist");
        assert_eq!(content, "from target", "the existing file should survive");
        assert!(
            source.join("same.txt").is_file(),
            "the source file should be left behind"
        );
    }

    #[test]
    fn overwrite_replaces_the_conflicting_file() {
        let temporary = temporary_directory();
        let source = directory(temporary.path(), "some_dir");
        std::fs::write(source.join("same.txt"), b"from source").expect("file should be written");
        let target = directory(temporary.path(), "Some Dir");
        std::fs::write(target.join("same.txt"), b"from target").expect("file should be written");

        dots(
            temporary.path(),
            DotRenameConfig {
                overwrite: true,
                verbose: true,
                ..config()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        let content = std::fs::read_to_string(target.join("same.txt")).expect("the file should exist");
        assert_eq!(content, "from source", "the moved file should win");
        assert!(!source.exists(), "the emptied source directory should be removed");
    }

    #[test]
    fn dryrun_reports_the_merge_without_moving_anything() {
        let temporary = temporary_directory();
        let source = directory(temporary.path(), "some_dir");
        write(&source, "file one.txt");
        directory(temporary.path(), "Some Dir");

        dots(
            temporary.path(),
            DotRenameConfig {
                dryrun: true,
                ..config()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert!(source.join("file one.txt").is_file(), "nothing should move in a dryrun");
        assert_eq!(entries(temporary.path()), vec!["Some Dir", "some_dir"]);
    }
}

#[cfg(test)]
mod test_rename_conflicts {
    use super::rename_test_helpers::*;
    use super::*;

    #[test]
    fn a_dryrun_counts_the_same_renames_as_a_real_run() {
        let temporary = temporary_directory();
        let source = temporary.path().join("some file.txt");
        let target = temporary.path().join("Some.File.txt");
        std::fs::write(&source, b"source").expect("file should be written");
        std::fs::write(&target, b"target").expect("file should be written");
        let paths = vec![(source, target)];

        let dryrun = dots(
            temporary.path(),
            DotRenameConfig {
                dryrun: true,
                ..Default::default()
            },
            false,
        )
        .rename_paths(&paths);
        let real = dots(temporary.path(), DotRenameConfig::default(), false).rename_paths(&paths);

        assert_eq!(real, 0, "the existing target should be skipped");
        assert_eq!(dryrun, real);
    }

    #[test]
    fn an_existing_target_is_skipped_by_default() {
        let temporary = temporary_directory();
        std::fs::write(temporary.path().join("some file.txt"), b"source").expect("file should be written");
        std::fs::write(temporary.path().join("Some.File.txt"), b"target").expect("file should be written");

        dots(temporary.path(), DotRenameConfig::default(), false)
            .run()
            .expect("the run should succeed");

        let content = std::fs::read_to_string(temporary.path().join("Some.File.txt")).expect("the file should exist");
        assert_eq!(content, "target");
        assert!(temporary.path().join("some file.txt").is_file());
    }

    #[test]
    fn increment_name_renames_beside_the_existing_file() {
        let temporary = temporary_directory();
        write(temporary.path(), "some file.txt");
        write(temporary.path(), "Some.File.txt");

        dots(
            temporary.path(),
            DotRenameConfig {
                increment_name: true,
                ..Default::default()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        assert_eq!(
            entries(temporary.path()),
            vec!["Some.File.2.txt", "Some.File.txt"],
            "the incremented name should be used"
        );
    }

    #[test]
    fn overwrite_replaces_the_existing_file() {
        let temporary = temporary_directory();
        std::fs::write(temporary.path().join("some file.txt"), b"source").expect("file should be written");
        std::fs::write(temporary.path().join("Some.File.txt"), b"target").expect("file should be written");

        dots(
            temporary.path(),
            DotRenameConfig {
                overwrite: true,
                ..Default::default()
            },
            false,
        )
        .run()
        .expect("the run should succeed");

        let content = std::fs::read_to_string(temporary.path().join("Some.File.txt")).expect("the file should exist");
        assert_eq!(content, "source");
        assert_eq!(entries(temporary.path()), vec!["Some.File.txt"]);
    }

    #[test]
    fn a_different_file_differing_only_by_case_is_never_replaced() {
        let temporary = temporary_directory();
        if !cli_tools_common::directory_is_case_sensitive(temporary.path()).expect("case probe should work") {
            return;
        }
        std::fs::write(temporary.path().join("Photo.JPG"), b"UPPER").expect("file should be written");
        std::fs::write(temporary.path().join("Photo.jpg"), b"ORIGINAL").expect("file should be written");

        dots(temporary.path(), DotRenameConfig::default(), false)
            .run()
            .expect("the run should succeed");

        let original = std::fs::read_to_string(temporary.path().join("Photo.jpg")).expect("the file should exist");
        assert_eq!(original, "ORIGINAL");
        let upper = std::fs::read_to_string(temporary.path().join("Photo.JPG")).expect("the file should exist");
        assert_eq!(upper, "UPPER");
    }

    #[test]
    fn a_capitalization_only_change_is_applied() {
        let temporary = temporary_directory();
        std::fs::write(temporary.path().join("photo.JPG"), b"image").expect("file should be written");

        dots(temporary.path(), DotRenameConfig::default(), false)
            .run()
            .expect("the run should succeed");

        assert_eq!(entries(temporary.path()), vec!["Photo.jpg"]);
        let content = std::fs::read_to_string(temporary.path().join("Photo.jpg")).expect("the file should exist");
        assert_eq!(content, "image");
    }
}

#[cfg(test)]
mod test_get_incremented_path {
    use super::rename_test_helpers::*;
    use super::*;

    #[test]
    fn the_first_free_index_starts_at_two() {
        let temporary = temporary_directory();
        let taken = write(temporary.path(), "Name.txt");

        let incremented = DotRename::get_incremented_path(&taken).expect("a free path should be found");

        assert_eq!(incremented, temporary.path().join("Name.2.txt"));
    }

    #[test]
    fn taken_indexes_are_skipped() {
        let temporary = temporary_directory();
        let taken = write(temporary.path(), "Name.txt");
        write(temporary.path(), "Name.2.txt");
        write(temporary.path(), "Name.3.txt");

        let incremented = DotRename::get_incremented_path(&taken).expect("a free path should be found");

        assert_eq!(incremented, temporary.path().join("Name.4.txt"));
    }
}

#[cfg(test)]
mod test_nested_trees {
    use super::*;
    use cli_tools_common::test_support::TempTree;

    /// Tree with unformatted file names at four depths and a few already formatted ones.
    fn deep_tree() -> TempTree {
        let tree = TempTree::new();
        tree.file_with("top file.txt", "top");
        tree.file_with("level one/second file.txt", "second");
        tree.file_with("level one/level two/third file.txt", "third");
        tree.file_with("level one/level two/level three/fourth file.txt", "fourth");
        tree.file_with("level one/Already.Formatted.txt", "formatted");
        tree.dir("level one/empty dir");
        tree
    }

    /// Run dots on the root of the tree with the given config.
    fn run(tree: &TempTree, config: DotRenameConfig) {
        DotRename::new(tree.path().to_path_buf(), config, false)
            .run()
            .expect("the run should succeed");
    }

    #[test]
    fn recursing_renames_files_at_every_depth_and_keeps_directories() {
        let tree = deep_tree();

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        assert_eq!(
            tree.snapshot(),
            vec![
                "Top.File.txt: top",
                "level one/",
                "level one/Already.Formatted.txt: formatted",
                "level one/Second.File.txt: second",
                "level one/empty dir/",
                "level one/level two/",
                "level one/level two/Third.File.txt: third",
                "level one/level two/level three/",
                "level one/level two/level three/Fourth.File.txt: fourth",
            ]
        );
    }

    #[test]
    fn without_recursing_only_the_root_files_are_renamed() {
        let tree = deep_tree();

        run(&tree, DotRenameConfig::default());

        assert_eq!(
            tree.files(),
            vec![
                "Top.File.txt",
                "level one/Already.Formatted.txt",
                "level one/level two/level three/fourth file.txt",
                "level one/level two/third file.txt",
                "level one/second file.txt",
            ]
        );
    }

    #[test]
    fn dryrun_on_a_deep_tree_changes_nothing() {
        let tree = deep_tree();
        let before = tree.snapshot();

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                dryrun: true,
                ..Default::default()
            },
        );
        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                dryrun: true,
                rename_directories: true,
                ..Default::default()
            },
        );

        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn a_second_recursive_run_changes_nothing() {
        let tree = deep_tree();
        let config = || DotRenameConfig {
            recurse: true,
            ..Default::default()
        };
        run(&tree, config());
        let after_first_run = tree.snapshot();

        run(&tree, config());

        assert_eq!(tree.snapshot(), after_first_run);
    }

    #[test]
    fn hidden_files_and_directories_are_skipped_at_every_depth() {
        let tree = TempTree::new();
        tree.file(".hidden file.txt");
        tree.file(".hidden dir/inner file.txt");
        tree.file("level one/.secret/deep file.txt");
        tree.file("level one/level two/.another hidden.txt");
        tree.file("level one/level two/visible file.txt");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        assert_eq!(
            tree.files(),
            vec![
                ".hidden dir/inner file.txt",
                ".hidden file.txt",
                "level one/.secret/deep file.txt",
                "level one/level two/.another hidden.txt",
                "level one/level two/Visible.File.txt",
            ]
        );
    }

    #[test]
    fn hidden_directories_are_not_renamed_when_renaming_directories() {
        let tree = TempTree::with_entries(&[".hidden dir/", "level one/.hidden inner/", "level one/visible dir/"]);

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                rename_directories: true,
                ..Default::default()
            },
        );

        assert_eq!(
            tree.dirs(),
            vec![
                ".hidden dir",
                "Level One",
                "Level One/.hidden inner",
                "Level One/Visible Dir"
            ]
        );
    }

    #[test]
    fn names_that_format_to_the_same_target_in_one_directory_keep_both_files() {
        let tree = TempTree::new();
        tree.file_with("nested/dir/some file.txt", "first");
        tree.file_with("nested/dir/some_file.txt", "second");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        let files = tree.files();
        assert_eq!(files.len(), 2, "no file should be lost: {files:?}");
        assert!(tree.exists("nested/dir/Some.File.txt"));
        let mut contents: Vec<String> = files.iter().map(|file| tree.read(file)).collect();
        contents.sort();
        assert_eq!(contents, vec!["first", "second"]);
    }

    #[test]
    fn names_that_format_to_the_same_target_with_overwrite_keep_both_files() {
        let tree = TempTree::new();
        tree.file_with("nested/dir/some file.txt", "first");
        tree.file_with("nested/dir/some_file.txt", "second");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                overwrite: true,
                ..Default::default()
            },
        );

        let files = tree.files();
        assert_eq!(
            files.len(),
            2,
            "overwrite must never destroy another source renamed earlier in the same batch: {files:?}"
        );
        let mut contents: Vec<String> = files.iter().map(|file| tree.read(file)).collect();
        contents.sort();
        assert_eq!(contents, vec!["first", "second"]);
    }

    #[test]
    fn overwrite_skips_a_pending_source_destination() {
        let tree = TempTree::new();
        tree.file_with("nested/Alpha.txt", "first");
        tree.file_with("nested/Beta.txt", "second");
        let config = DotRenameConfig {
            overwrite: true,
            ..Default::default()
        };
        let paths = vec![
            (tree.join("nested/Alpha.txt"), tree.join("nested/Beta.txt")),
            (tree.join("nested/Beta.txt"), tree.join("nested/Gamma.txt")),
        ];
        let before = tree.snapshot();

        let dryrun = DotRename::new(
            tree.path().to_path_buf(),
            DotRenameConfig {
                dryrun: true,
                overwrite: true,
                ..Default::default()
            },
            false,
        );
        let planned = dryrun.rename_paths(&paths);
        assert_eq!(tree.snapshot(), before);

        let dots = DotRename::new(tree.path().to_path_buf(), config, false);
        assert_eq!(dots.rename_paths(&paths), 1);
        assert_eq!(
            tree.snapshot(),
            ["nested/", "nested/Alpha.txt: first", "nested/Gamma.txt: second"]
        );
        assert_eq!(planned, 1);
    }

    #[test]
    fn overwrite_protects_an_alias_of_a_pending_source_but_allows_its_case_change() {
        let tree = TempTree::new();
        tree.file_with("nested/Alpha.txt", "first");
        tree.file_with("nested/beta.txt", "second");
        let alias = tree.join("nested/Beta.txt");
        if !alias.exists() {
            return;
        }

        let dots = DotRename::new(
            tree.path().to_path_buf(),
            DotRenameConfig {
                overwrite: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(
            dots.rename_paths(&[
                (tree.join("nested/Alpha.txt"), alias.clone()),
                (tree.join("nested/beta.txt"), alias),
            ]),
            1
        );
        assert_eq!(
            tree.snapshot(),
            ["nested/", "nested/Alpha.txt: first", "nested/Beta.txt: second"]
        );
    }

    #[test]
    fn overwrite_increments_a_pending_source_destination_when_requested() {
        let tree = TempTree::new();
        tree.file_with("nested/Alpha.txt", "first");
        tree.file_with("nested/Beta.txt", "second");
        tree.file_with("nested/Beta.2.txt", "untouched");
        let dots = DotRename::new(
            tree.path().to_path_buf(),
            DotRenameConfig {
                overwrite: true,
                increment_name: true,
                ..Default::default()
            },
            false,
        );

        assert_eq!(
            dots.rename_paths(&[
                (tree.join("nested/Alpha.txt"), tree.join("nested/Beta.txt")),
                (tree.join("nested/Beta.txt"), tree.join("nested/Gamma.txt")),
            ]),
            2
        );
        assert_eq!(
            tree.snapshot(),
            [
                "nested/",
                "nested/Beta.2.txt: untouched",
                "nested/Beta.3.txt: first",
                "nested/Gamma.txt: second"
            ]
        );
    }

    #[test]
    fn directories_do_not_merge_into_a_pending_source() {
        let tree = TempTree::new();
        tree.file_with("nested/Alpha/item.txt", "first");
        tree.file_with("nested/Beta/item.txt", "second");
        let dots = DotRename::new(
            tree.path().to_path_buf(),
            DotRenameConfig {
                rename_directories: true,
                overwrite: true,
                ..Default::default()
            },
            false,
        );

        assert_eq!(
            dots.rename_paths(&[
                (tree.join("nested/Alpha"), tree.join("nested/Beta")),
                (tree.join("nested/Beta"), tree.join("nested/Gamma")),
            ]),
            1
        );
        assert_eq!(
            tree.snapshot(),
            [
                "nested/",
                "nested/Alpha/",
                "nested/Alpha/item.txt: first",
                "nested/Gamma/",
                "nested/Gamma/item.txt: second"
            ]
        );
    }

    #[test]
    fn punctuation_only_names_are_skipped_and_extensionless_files_omit_trailing_dot() {
        let tree = TempTree::new();
        tree.file_with("---/secret.txt", "secret");
        tree.file_with("!!!.txt", "punct");
        tree.file_with("my readme", "docs");

        run(
            &tree,
            DotRenameConfig {
                rename_directories: true,
                recurse: true,
                ..Default::default()
            },
        );
        assert!(
            tree.exists("---/secret.txt"),
            "punctuation-only directory must not flatten into parent: {:?}",
            tree.snapshot()
        );

        run(
            &tree,
            DotRenameConfig {
                recurse: false,
                ..Default::default()
            },
        );
        assert!(
            tree.exists("!!!.txt"),
            "punctuation-only file must not become a hidden dotfile"
        );
        assert!(
            tree.exists("My.Readme"),
            "extensionless file must not gain a trailing dot: {:?}",
            tree.snapshot()
        );
        assert!(!tree.exists("My.Readme."));
    }

    #[test]
    fn names_that_format_to_the_same_target_with_incrementing_get_numbered() {
        let tree = TempTree::new();
        tree.file_with("nested/dir/some file.txt", "first");
        tree.file_with("nested/dir/some_file.txt", "second");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                increment_name: true,
                ..Default::default()
            },
        );

        assert_eq!(
            tree.files(),
            vec!["nested/dir/Some.File.2.txt", "nested/dir/Some.File.txt"]
        );
    }

    #[test]
    fn the_same_name_in_different_directories_is_renamed_in_each() {
        let tree = TempTree::new();
        tree.file_with("first dir/some file.txt", "first");
        tree.file_with("second dir/deeper/some file.txt", "second");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        assert_eq!(
            tree.snapshot(),
            vec![
                "first dir/",
                "first dir/Some.File.txt: first",
                "second dir/",
                "second dir/deeper/",
                "second dir/deeper/Some.File.txt: second",
            ]
        );
    }

    #[test]
    fn an_existing_target_is_kept_without_overwrite() {
        let tree = TempTree::new();
        tree.file_with("dir/Some.File.txt", "existing");
        tree.file_with("dir/some file.txt", "incoming");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        assert_eq!(
            tree.snapshot(),
            vec!["dir/", "dir/Some.File.txt: existing", "dir/some file.txt: incoming"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_renamed_without_losing_files() {
        let tree = TempTree::new();
        let Some(_) = tree.non_utf8_file("nested dir", "some file", "txt") else {
            return;
        };
        tree.file("nested dir/other file.txt");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        let files = tree.files();
        assert_eq!(files.len(), 2, "no file should be lost: {files:?}");
        assert!(tree.exists("nested dir/Other.File.txt"));
    }
}

#[cfg(all(test, unix))]
mod test_symlinks {
    use super::*;
    use cli_tools_common::test_support::TempTree;

    /// Tree with a walk root and a directory outside it that the root links to.
    fn linked_tree() -> TempTree {
        let tree = TempTree::new();
        tree.file_with("outside/real dir/inner file.txt", "inner");
        tree.file_with("outside/real file.txt", "real");
        tree.file("root/sub dir/plain file.txt");
        tree.symlink("outside/real dir", "root/sub dir/linked dir");
        tree.symlink("outside/real file.txt", "root/sub dir/linked file.txt");
        tree
    }

    /// Run dots on the "root" directory of the tree.
    fn run(tree: &TempTree, config: DotRenameConfig) {
        DotRename::new(tree.join("root"), config, false)
            .run()
            .expect("the run should succeed");
    }

    #[test]
    fn link_targets_outside_the_root_are_never_renamed() {
        let tree = linked_tree();

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                ..Default::default()
            },
        );

        assert_eq!(tree.read("outside/real file.txt"), "real");
        assert_eq!(tree.read("outside/real dir/inner file.txt"), "inner");
        assert!(tree.exists("root/sub dir/Plain.File.txt"));
        assert!(
            tree.join("root/sub dir/linked dir")
                .symlink_metadata()
                .expect("the directory link should remain")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn directories_behind_a_link_are_not_walked() {
        let tree = linked_tree();

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                rename_directories: true,
                ..Default::default()
            },
        );

        assert!(tree.exists("outside/real dir/inner file.txt"));
        assert_eq!(tree.dirs(), vec!["outside", "outside/real dir", "root", "root/Sub Dir"]);
    }

    #[test]
    fn a_linked_directory_is_not_merged_into_an_existing_directory() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("root/Some Dir/existing.txt", "existing");
        tree.symlink("outside/real", "root/some_dir");

        run(
            &tree,
            DotRenameConfig {
                rename_directories: true,
                ..Default::default()
            },
        );

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        assert!(!tree.exists("root/Some Dir/data.txt"));
        assert!(tree.exists("root/some_dir"));
    }

    #[test]
    fn overwrite_does_not_replace_an_existing_directory_with_a_linked_one() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        tree.file_with("root/Some Dir/existing.txt", "existing");
        tree.symlink("outside/real", "root/some_dir");

        run(
            &tree,
            DotRenameConfig {
                rename_directories: true,
                overwrite: true,
                ..Default::default()
            },
        );

        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        assert_eq!(tree.read("root/Some Dir/existing.txt"), "existing");
    }

    #[test]
    fn a_real_directory_is_not_merged_into_a_destination_symlink() {
        let tree = TempTree::new();
        tree.dir("outside/real");
        tree.file_with("root/some_dir/a.txt", "local");
        tree.symlink("outside/real", "root/Some Dir");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                rename_directories: true,
                ..Default::default()
            },
        );

        assert!(
            !tree.exists("outside/real/a.txt"),
            "must not move files through a destination symlink outside root"
        );
        assert!(tree.exists("root/some_dir/a.txt"));
    }

    #[test]
    fn merge_refuses_intermediate_destination_symlink() {
        let tree = TempTree::new();
        tree.file_with("outside/real/deep/track.txt", "original");
        tree.file_with("root/some_dir/languages/deep/track.txt", "local");
        tree.symlink("outside/real", "root/Some Dir/languages");
        let before = tree.snapshot();

        run(
            &tree,
            DotRenameConfig {
                rename_directories: true,
                overwrite: true,
                ..Default::default()
            },
        );

        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn broken_symlink_inside_target_directory_is_kept_without_overwrite() {
        let tree = TempTree::new();
        tree.file_with("root/some_dir/item.txt", "new");
        tree.dir("root/Some Dir");
        tree.symlink("outside/missing.txt", "root/Some Dir/item.txt");

        run(
            &tree,
            DotRenameConfig {
                recurse: true,
                rename_directories: true,
                ..Default::default()
            },
        );

        assert!(
            tree.join("root/Some Dir/item.txt")
                .symlink_metadata()
                .expect("broken link")
                .file_type()
                .is_symlink()
        );
        assert!(tree.exists("root/some_dir/item.txt"));
    }
}

#[cfg(test)]
mod test_remove_empty_directories {
    use super::*;
    use cli_tools_common::test_support::TempTree;

    #[test]
    fn removes_nested_empty_directories_and_keeps_those_with_files() {
        let tree = TempTree::with_entries(&[
            "source/empty a/",
            "source/nested/empty b/",
            "source/nested/deeper/empty c/",
            "source/kept/file.txt",
            "source/nested/hidden keeper/.hidden",
        ]);

        DotRename::default()
            .remove_empty_directories(&tree.join("source"))
            .expect("removal should succeed");

        assert_eq!(
            tree.dirs(),
            vec!["source", "source/kept", "source/nested", "source/nested/hidden keeper"]
        );
        assert_eq!(
            tree.files(),
            vec!["source/kept/file.txt", "source/nested/hidden keeper/.hidden"]
        );
    }

    #[test]
    fn a_tree_of_only_empty_directories_is_removed_entirely() {
        let tree = TempTree::with_entries(&["source/a/b/c/", "source/d/", "other/"]);

        DotRename::default()
            .remove_empty_directories(&tree.join("source"))
            .expect("removal should succeed");

        assert_eq!(tree.dirs(), vec!["other"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_holding_only_a_symlink_is_kept() {
        let tree = TempTree::new();
        tree.file("target.txt");
        tree.dir("source/empty/");
        tree.symlink("target.txt", "source/link holder/link");

        DotRename::default()
            .remove_empty_directories(&tree.join("source"))
            .expect("removal should succeed");

        assert_eq!(tree.dirs(), vec!["source", "source/link holder"]);
        assert!(tree.exists("target.txt"));
    }
}
