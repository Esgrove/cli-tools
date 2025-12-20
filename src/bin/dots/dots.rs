use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::{fmt, fs};

use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;
use walkdir::WalkDir;

use cli_tools::date::{CURRENT_YEAR, RE_CORRECT_DATE_FORMAT, RE_YEAR};

use crate::Args;
use crate::config::Config;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[({\]})]+").expect("Failed to create regex pattern for brackets"));

static RE_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("Failed to compile whitespace regex"));

static RE_DOTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{2,}").expect("Failed to compile dots regex"));

static RE_EXCLAMATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!+").expect("Failed to compile exclamation regex"));

static RE_DOTCOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\.com|\.net)\b").expect("Failed to compile .com regex"));

static RE_IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9]{9,20}").expect("Failed to compile id regex"));

static RE_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{3,4}x\d{3,4}\b").expect("Failed to compile resolution regex"));

static RE_WRITTEN_DATE_MDY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?P<month>Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:tember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\.(?P<day>\d{1,2})\.(?P<year>\d{4})\b",
    )
        .expect("Failed to compile MDY written date regex")
});

static RE_WRITTEN_DATE_DMY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?P<day>\d{1,2})\.(?P<month>Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:tember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\.(?P<year>\d{4})\b",
    )
        .expect("Failed to compile DMY written date regex")
});

static WRITTEN_MONTHS_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    [
        ("jan", "01"),
        ("january", "01"),
        ("feb", "02"),
        ("february", "02"),
        ("mar", "03"),
        ("march", "03"),
        ("apr", "04"),
        ("april", "04"),
        ("may", "05"),
        ("jun", "06"),
        ("june", "06"),
        ("jul", "07"),
        ("july", "07"),
        ("aug", "08"),
        ("august", "08"),
        ("sep", "09"),
        ("september", "09"),
        ("oct", "10"),
        ("october", "10"),
        ("nov", "11"),
        ("november", "11"),
        ("dec", "12"),
        ("december", "12"),
    ]
    .into_iter()
    .collect()
});

static REPLACE: [(&str, &str); 27] = [
    (" ", "."),
    (" - ", " "),
    (", ", " "),
    ("_", "."),
    ("-", "."),
    ("–", "."),
    (".&.", ".and."),
    ("*", "."),
    ("~", "."),
    ("¡", "."),
    ("#", "."),
    ("$", "."),
    (";", "."),
    ("@", "."),
    ("+", "."),
    ("=", "."),
    (",.", "."),
    (",", "."),
    ("-=-", "."),
    (".-.", "."),
    (".rq", ""),
    ("www.", ""),
    ("^", ""),
    ("｜", ""),
    ("`", "'"),
    ("’", "'"),
    ("\"", "'"),
];

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
const RESOLUTIONS: [&str; 6] = ["540", "720", "1080", "1920", "2160", "3840"];

#[derive(Debug, Default)]
pub struct Dots {
    root: PathBuf,
    config: Config,
    path_given: bool,
}

impl Dots {
    /// Init new instance with CLI args.
    pub fn new(args: Args) -> Result<Self> {
        let path_given = args.path.is_some();
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args)?;
        Ok(Self {
            root,
            config,
            path_given,
        })
    }

    /// Run renaming with given args.
    #[inline]
    pub fn run_with_args(args: Args) -> Result<()> {
        Self::new(args)?.run()
    }

    /// Run renaming.
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
        if self.config.prefix_dir || self.config.suffix_dir {
            let formatted_dir = if self.root.is_dir() {
                cli_tools::get_normalized_dir_name(&self.root)?
            } else {
                let parent_dir = self.root.parent().context("Failed to get parent dir")?;
                cli_tools::get_normalized_dir_name(parent_dir)?
            };
            let name = self.format_name(&formatted_dir);
            if self.config.prefix_dir {
                if self.config.verbose {
                    println!("Using directory prefix: {name}");
                }
                let regexes = Self::build_prefix_dir_regexes(&name)?;
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
                self.config.regex_replace_after.extend(regexes);
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
            .filter_entry(|e| !cli_tools::is_hidden(e))
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .collect();

        let progress_bar = ProgressBar::new(paths.len() as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );

        // Filter and format files in parallel
        let mut results: Vec<_> = paths
            .into_par_iter()
            .filter_map(|path| {
                let result = {
                    // Filter based on include and exclude lists
                    let path_str = cli_tools::path_to_string(&path);
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

    /// Get all directories that need to be renamed.
    fn gather_directories_to_rename(&self, path_specified: bool) -> Vec<(PathBuf, PathBuf)> {
        // If a directory was given as input, use that unless recurse mode is enabled
        if path_specified && !self.config.recurse {
            return self
                .formatted_directory_path(&self.root)
                .ok()
                .filter(|new_path| &self.root != new_path)
                .map(|new_path| (self.root.clone(), new_path))
                .into_iter()
                .collect();
        }

        let max_depth = if self.config.recurse { 100 } else { 1 };

        // Collect all directory paths first
        let paths: Vec<PathBuf> = WalkDir::new(&self.root)
            .max_depth(max_depth)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(walkdir::DirEntry::into_path)
            .collect();

        let progress_bar = ProgressBar::new(paths.len() as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );

        // Filter and format directories in parallel
        let mut results: Vec<_> = paths
            .into_par_iter()
            .filter_map(|path| {
                let result = {
                    // Filter based on include list
                    let matches_filter = if self.config.include.is_empty() {
                        true
                    } else {
                        let path_str = cli_tools::path_to_string(&path);
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
            if dir_path != dir && cli_tools::is_directory_empty(dir_path) {
                if self.config.verbose {
                    println!("Removing empty directory: {}", dir_path.display());
                }
                fs::remove_dir(dir_path)?;
            }
        }

        // Finally, try to remove the source directory itself
        if cli_tools::is_directory_empty(dir) {
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
            let old_str = cli_tools::get_relative_path_or_filename(&path, &self.root);
            let mut new_str = cli_tools::get_relative_path_or_filename(&new_path, &self.root);
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
                            new_str = cli_tools::get_relative_path_or_filename(&incremented_path, &self.root);
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
                cli_tools::show_diff(&old_str, &new_str);

                if is_directory_merge {
                    println!("Would merge directory: {old_str} -> {new_str}");
                }
                num_renamed += 1;
                continue;
            }

            println!("{}", format!("Rename {number}:").bold().magenta());
            cli_tools::show_diff(&old_str, &new_str);

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

        if let Ok((file_name, file_extension)) = cli_tools::get_normalized_file_name_and_extension(path) {
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

        let directory_name = cli_tools::os_str_to_string(path.file_name().context("Failed to get directory name")?);

        let formatted_name = self.format_name(&directory_name).replace('.', " ");

        Ok(path.with_file_name(formatted_name))
    }

    /// Format the file name without the file extension
    fn format_name(&self, file_name: &str) -> String {
        let mut new_name = String::from(file_name);

        self.apply_replacements(&mut new_name);
        Self::remove_special_characters(&mut new_name);

        if self.config.convert_case {
            new_name = new_name.to_lowercase();
        }

        self.apply_config_replacements(&mut new_name);

        if self.config.remove_random {
            Self::remove_random_identifiers(&mut new_name);
        }

        new_name = new_name.trim_start_matches('.').trim_end_matches('.').to_string();

        Self::apply_titlecase(&mut new_name);
        Self::convert_written_date_format(&mut new_name);

        if let Some(date_flipped_name) = if self.config.rename_directories {
            cli_tools::date::reorder_directory_date(&new_name)
        } else {
            cli_tools::date::reorder_filename_date(&new_name, self.config.date_starts_with_year, false, false)
        } {
            new_name = date_flipped_name;
        }

        if let Some(ref prefix) = self.config.prefix {
            new_name = Self::apply_prefix(&new_name, prefix);
        }

        if let Some(ref suffix) = self.config.suffix {
            new_name = self.apply_suffix(&new_name, suffix);
        }

        if !self.config.move_to_start.is_empty() {
            self.move_to_start(&mut new_name);
        }
        if !self.config.move_to_end.is_empty() {
            self.move_to_end(&mut new_name);
        }
        if !self.config.move_date_after_prefix.is_empty() {
            self.move_date_after_prefix(&mut new_name);
        }
        if !self.config.remove_from_start.is_empty() {
            self.remove_from_start(&mut new_name);
        }

        // Apply regex replacements (workaround for prefix regex)
        if !self.config.regex_replace_after.is_empty() {
            for (regex, replacement) in &self.config.regex_replace_after {
                new_name = regex.replace_all(&new_name, replacement).to_string();
            }
        }

        // Remove consecutive duplicate patterns from substitutions and prefix_dir
        if !self.config.deduplicate_patterns.is_empty() {
            Self::remove_consecutive_duplicates(&mut new_name, &self.config.deduplicate_patterns);
        }

        RE_DOTS
            .replace_all(&new_name, ".")
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_string()
    }

    fn move_to_start(&self, name: &mut String) {
        for pattern in &self.config.move_to_start {
            let re = Regex::new(&format!(r"\b{}\b", regex::escape(pattern))).expect("Failed to create regex pattern");

            if re.is_match(name) {
                *name = format!("{}.{}", pattern, re.replace(name, ""));
            }
        }
    }

    fn move_to_end(&self, name: &mut String) {
        for sub in &self.config.move_to_end {
            if name.contains(sub) {
                *name = format!("{}.{}", name.replace(sub, ""), sub);
            }
        }
    }

    fn move_date_after_prefix(&self, name: &mut String) {
        for prefix in &self.config.move_date_after_prefix {
            if name.starts_with(prefix) {
                if let Some(date_match) = RE_CORRECT_DATE_FORMAT.find(name) {
                    let date = date_match.as_str();
                    let mut new_name = name.clone();

                    // Remove the date from its current location
                    new_name.replace_range(date_match.range(), "");

                    let insert_pos = prefix.len();
                    new_name.insert_str(insert_pos, &format!(".{date}."));

                    *name = new_name;
                }
                if let Some(date_match) = RE_YEAR.find(name) {
                    let date = date_match.as_str().parse::<i32>().expect("Failed to parse year");
                    if date <= *CURRENT_YEAR {
                        let mut new_name = name.clone();

                        new_name.replace_range(date_match.range(), "");

                        let insert_pos = prefix.len();
                        new_name.insert_str(insert_pos, &format!(".{date}."));

                        *name = new_name;
                    }
                }
            }
        }
    }

    fn remove_from_start(&self, name: &mut String) {
        for pattern in &self.config.remove_from_start {
            let re = Regex::new(&format!(r"\b{}\b", regex::escape(pattern))).expect("Failed to create regex pattern");
            if let Some(last_match) = re.find_iter(name).last() {
                // Split the text into parts before the last regex match
                let before_last = &name[..last_match.start()];
                let after_last = &name[last_match.start()..];

                // Remove all occurrences from the first part using regex
                *name = format!("{}.{after_last}", re.replace_all(before_last, ""));
            }
        }
    }

    /// Only retain alphanumeric characters and a few common filename characters
    fn remove_special_characters(name: &mut String) {
        let cleaned: String = name
            // Split the string into graphemes (for handling emojis and complex characters)
            .graphemes(true)
            .filter(|g| {
                g.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '\'' || c == '&')
            })
            .collect();

        *name = cleaned;
    }

    /// Apply suffix to the filename, handling various matching scenarios.
    fn apply_suffix(&self, name: &str, suffix: &str) -> String {
        let mut new_name = name.to_string();

        if new_name.starts_with(suffix) {
            new_name = new_name.replacen(suffix, "", 1);
        }

        if new_name.contains(suffix) {
            self.remove_from_start(&mut new_name);
            return new_name;
        }

        let lower_name = new_name.to_lowercase();
        let lower_suffix = suffix.to_lowercase();

        if lower_name.ends_with(&lower_suffix) {
            format!("{}{}", &new_name[..new_name.len() - lower_suffix.len()], suffix)
        } else {
            format!("{new_name}.{suffix}")
        }
    }

    /// Apply static and pre-configured replacements to a filename.
    fn apply_replacements(&self, name: &mut String) {
        for (pattern, replacement) in &self.config.pre_replace {
            *name = name.replace(pattern, replacement);
        }

        // Apply static replacements
        for (pattern, replacement) in REPLACE {
            *name = name.replace(pattern, replacement);
        }

        *name = RE_BRACKETS.replace_all(name, ".").into_owned();
        *name = RE_DOTCOM.replace_all(name, ".").into_owned();
        *name = RE_EXCLAMATION.replace_all(name, ".").into_owned();
        *name = RE_WHITESPACE.replace_all(name, ".").into_owned();
        *name = RE_DOTS.replace_all(name, ".").into_owned();
    }

    /// Apply titlecase formatting.
    fn apply_titlecase(name: &mut String) {
        // Temporarily convert dots back to whitespace so titlecase works
        *name = name.replace('.', " ");
        *name = titlecase::titlecase(name);
        *name = name.replace(' ', ".");
        // Fix encoding capitalization
        *name = name.replace("X265", "x265").replace("X264", "x264");
    }

    /// Apply user-configured replacements from args and config file.
    fn apply_config_replacements(&self, name: &mut String) {
        for (pattern, replacement) in &self.config.replace {
            *name = name.replace(pattern, replacement);
        }

        for (regex, replacement) in &self.config.regex_replace {
            *name = regex.replace_all(name, replacement).into_owned();
        }
    }

    /// Apply prefix to the filename, handling various matching scenarios.
    fn apply_prefix(name: &str, prefix: &str) -> String {
        let mut new_name = name.to_string();

        if !new_name.starts_with(prefix) && new_name.contains(prefix) {
            new_name = new_name.replacen(prefix, "", 1);
        }

        let lower_name = new_name.to_lowercase();
        let lower_prefix = prefix.to_lowercase();

        if lower_name.starts_with(&lower_prefix) {
            // Full prefix match - update capitalization
            return format!("{}{}", prefix, &new_name[prefix.len()..]);
        }

        // Check if new_name starts with any suffix of the prefix
        let prefix_parts: Vec<&str> = prefix.split('.').collect();
        for i in 1..prefix_parts.len() {
            let suffix = prefix_parts[i..].join(".");
            let lower_suffix = suffix.to_lowercase();

            if lower_name.starts_with(&lower_suffix) {
                // Found a matching suffix, replace with full prefix
                return format!("{}{}", prefix, &new_name[suffix.len()..]);
            }
        }

        format!("{prefix}.{new_name}")
    }

    fn remove_random_identifiers(name: &mut String) {
        *name = RE_IDENTIFIER
            .replace_all(name, |caps: &regex::Captures| {
                let matched_str = &caps[0];
                if Self::has_at_least_six_digits(matched_str)
                    && !RE_RESOLUTION.is_match(matched_str)
                    && !RESOLUTIONS.iter().any(|&number| matched_str.contains(number))
                    && !name.contains("hash2")
                {
                    String::new()
                } else {
                    matched_str.trim().to_string()
                }
            })
            .to_string();
    }

    /// Remove consecutive duplicate occurrences of patterns from the name.
    /// For example, "Some.Name.Some.Name.File" with pattern "Some.Name." becomes "Some.Name.File"
    fn remove_consecutive_duplicates(name: &mut String, patterns: &[(Regex, String)]) {
        for (regex, replacement) in patterns {
            *name = regex.replace_all(name, replacement.as_str()).into_owned();
        }
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

    fn has_at_least_six_digits(s: &str) -> bool {
        s.chars().filter(char::is_ascii_digit).count() >= 6
    }

    /// Rename a file with an intermediate temp file to work around case-insensitive file systems.
    fn rename_with_temp_file(path: &PathBuf, new_path: &PathBuf) -> std::io::Result<()> {
        let temp_file = cli_tools::append_extension_to_path(new_path.clone(), ".tmp");
        fs::rename(path, &temp_file)?;
        fs::rename(&temp_file, new_path)
    }

    fn get_incremented_path(original: &Path) -> Result<PathBuf> {
        let mut index = 2;
        let parent = original.parent().unwrap_or_else(|| Path::new(""));
        let (name, extension) = cli_tools::get_normalized_file_name_and_extension(original)?;
        loop {
            let file_name = format!("{name}.{index}.{extension}");
            let new_path = parent.join(file_name);
            if !new_path.exists() {
                return Ok(new_path);
            }
            index += 1;
        }
    }

    /// Convert date with written month name to numeral date.
    ///
    /// For example:
    /// ```not_rust
    /// "Jan.3.2020" -> "2020.01.03"
    /// "December.6.2023" -> "2023.12.06"
    /// "23.May.2016" -> "2016.05.23"
    /// ```
    fn convert_written_date_format(name: &mut String) {
        // Replace Month.Day.Year
        *name = RE_WRITTEN_DATE_MDY
            .replace_all(name, |caps: &regex::Captures| {
                let year = &caps["year"];
                let month_raw = &caps["month"].to_lowercase();
                let month = WRITTEN_MONTHS_MAP.get(month_raw.as_str()).expect("Failed to map month");
                let day = format!("{:02}", caps["day"].parse::<u8>().expect("Failed to parse day"));
                format!("{year}.{month}.{day}")
            })
            .to_string();

        // Replace Day.Month.Year
        *name = RE_WRITTEN_DATE_DMY
            .replace_all(name, |caps: &regex::Captures| {
                let year = &caps["year"];
                let month_raw = &caps["month"].to_lowercase();
                let month = WRITTEN_MONTHS_MAP.get(month_raw.as_str()).expect("Failed to map month");
                let day = format!("{:02}", caps["day"].parse::<u8>().expect("Failed to parse day"));
                format!("{year}.{month}.{day}")
            })
            .to_string();
    }

    /// Convert to lowercase.
    ///
    /// Splits from dot and only converts parts longer than 3 characters.
    #[allow(unused)]
    fn convert_to_lowercase(name: &mut String) {
        let parts: Vec<_> = name
            .split('.')
            .map(|s| {
                if s.chars().count() > 3 {
                    Cow::Owned(s.to_lowercase())
                } else {
                    Cow::Borrowed(s)
                }
            })
            .collect();

        *name = parts.join(".");
    }
}

impl fmt::Display for Dots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Root: {}", self.root.display())?;
        write!(f, "{}", self.config)
    }
}

#[cfg(test)]
mod dots_tests {
    use super::*;

    static DOTS: LazyLock<Dots> = LazyLock::new(Dots::default);
    static DOTS_INCREMENT: LazyLock<Dots> = LazyLock::new(|| Dots {
        root: PathBuf::default(),
        config: Config {
            remove_random: true,
            ..Default::default()
        },
        path_given: false,
    });

    #[test]
    fn test_format_basic() {
        assert_eq!(DOTS.format_name("Some file"), "Some.File");
        assert_eq!(DOTS.format_name("some file"), "Some.File");
        assert_eq!(DOTS.format_name("word"), "Word");
        assert_eq!(DOTS.format_name("__word__"), "Word");
        assert_eq!(DOTS.format_name("testCAP CAP WORD GL"), "testCAP.CAP.WORD.GL");
        assert_eq!(DOTS.format_name("test CAP CAP WORD GL"), "Test.CAP.CAP.WORD.GL");
        assert_eq!(DOTS.format_name("CAP WORD GL"), "Cap.Word.Gl");
    }

    #[test]
    fn test_format_convert_case() {
        let dots_case = Dots {
            root: PathBuf::default(),
            config: Config {
                convert_case: true,
                ..Default::default()
            },
            path_given: false,
        };
        assert_eq!(dots_case.format_name("CAP WORD GL"), "Cap.Word.Gl");
        assert_eq!(dots_case.format_name("testCAP CAP WORD GL"), "Testcap.Cap.Word.Gl");
        assert_eq!(dots_case.format_name("test CAP CAP WORD GL"), "Test.Cap.Cap.Word.Gl");
    }

    #[test]
    fn test_format_name_with_newlines() {
        assert_eq!(
            DOTS.format_name("Meeting \tNotes \n(2023) - Draft\r\n"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_no_brackets() {
        assert_eq!(DOTS.format_name("John Doe - Document"), "John.Doe.Document");
    }

    #[test]
    fn test_format_name_with_brackets() {
        assert_eq!(
            DOTS.format_name("Project Report - [Final Version]"),
            "Project.Report.Final.Version"
        );
        assert_eq!(DOTS.format_name("Code {Snippet} (example)"), "Code.Snippet.Example");
    }

    #[test]
    fn test_format_name_with_parentheses() {
        assert_eq!(
            DOTS.format_name("Meeting Notes (2023) - Draft"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_with_extra_dots() {
        assert_eq!(DOTS.format_name("file..with...dots"), "File.With.Dots");
        assert_eq!(
            DOTS.format_name("...leading.and.trailing.dots..."),
            "Leading.and.Trailing.Dots"
        );
    }

    #[test]
    fn test_format_name_with_exclamations() {
        assert_eq!(DOTS.format_name("Exciting!Document!!"), "Exciting.Document");
        assert_eq!(DOTS.format_name("Hello!!!World!!"), "Hello.World");
    }

    #[test]
    fn test_format_name_with_dotcom() {
        assert_eq!(
            DOTS.format_name("visit.website.com.for.details"),
            "Visit.Website.for.Details"
        );
        assert_eq!(
            DOTS.format_name("Contact us at email@domain.net"),
            "Contact.Us.at.Email.Domain"
        );
        assert_eq!(DOTS.format_name("Contact.company.test"), "Contact.Company.Test");
    }

    #[test]
    fn test_format_name_with_combined_cases() {
        assert_eq!(
            DOTS.format_name("Amazing [Stuff]!! Visit my.site.com..now"),
            "Amazing.Stuff.Visit.My.Site.Now"
        );
    }

    #[test]
    fn test_format_name_with_weird_characters() {
        assert_eq!(
            DOTS.format_name("Weird-Text-~File-Name-@Example#"),
            "Weird.Text.File.Name.Example"
        );
    }

    #[test]
    fn test_format_name_empty_string() {
        assert_eq!(DOTS.format_name(""), "");
    }

    #[test]
    fn test_format_name_no_changes() {
        assert_eq!(DOTS.format_name("SingleWord"), "SingleWord");
    }

    #[test]
    fn test_format_name_full_resolution() {
        assert_eq!(
            DOTS_INCREMENT.format_name("test.string.with resolution. 1234x900"),
            "Test.String.With.Resolution.1234x900"
        );
        assert_eq!(DOTS_INCREMENT.format_name("resolution 719x719"), "Resolution.719x719");
        assert_eq!(DOTS_INCREMENT.format_name("resolution 122225x719"), "Resolution");
    }

    #[test]
    fn test_move_to_start() {
        let mut dots = Dots::default();
        dots.config.move_to_start = vec!["Test".to_string()];
        assert_eq!(
            dots.format_name("This is a test string test"),
            "Test.This.Is.a.String.Test"
        );
        assert_eq!(
            dots.format_name("Test.This.Is.a.test.string.test"),
            "Test.This.Is.a.Test.String.Test"
        );
        assert_eq!(dots.format_name("test"), "Test");
        assert_eq!(dots.format_name("Test"), "Test");
        assert_eq!(
            dots.format_name("TestOther should not be broken"),
            "TestOther.Should.Not.Be.Broken"
        );
        assert_eq!(dots.format_name("Test-Something-else"), "Test.Something.Else");
    }

    #[test]
    fn test_move_to_end() {
        let mut dots = Dots::default();
        dots.config.move_to_end = vec!["Test".to_string()];
        assert_eq!(dots.format_name("This is a test string test"), "This.Is.a.String.Test");
        assert_eq!(
            dots.format_name("Test.This.Is.a.test.string.test"),
            "This.Is.a.String.Test"
        );
        assert_eq!(dots.format_name("test"), "Test");
        assert_eq!(dots.format_name("Test"), "Test");
    }

    #[test]
    fn test_remove_identifier() {
        assert_eq!(
            DOTS_INCREMENT.format_name("This is a string test ^[640e54a564228]"),
            "This.Is.a.String.Test"
        );
        assert_eq!(
            DOTS_INCREMENT.format_name("This.Is.a.test.string.65f09e4248e03..."),
            "This.Is.a.Test.String"
        );
        assert_eq!(DOTS_INCREMENT.format_name("test Ph5d9473a841fe9"), "Test");
        assert_eq!(DOTS_INCREMENT.format_name("Test-355989849"), "Test");
    }

    #[test]
    fn test_format_date() {
        assert_eq!(
            DOTS.format_name("This is a test string test 1.1.2014"),
            "This.Is.a.Test.String.Test.2014.01.01"
        );
        assert_eq!(
            DOTS.format_name("Test.This.Is.a.test.string.test.30.05.2020"),
            "Test.This.Is.a.Test.String.Test.2020.05.30"
        );
        assert_eq!(
            DOTS.format_name("Testing date 30.05.2020 in the middle"),
            "Testing.Date.2020.05.30.in.the.Middle"
        );
    }

    #[test]
    fn test_format_date_year_first() {
        let dots = Dots {
            root: PathBuf::default(),
            config: Config {
                date_starts_with_year: true,
                ..Default::default()
            },
            path_given: false,
        };

        assert_eq!(
            dots.format_name("This is a test string test 1.1.2014"),
            "This.Is.a.Test.String.Test.2014.01.01"
        );
        assert_eq!(
            dots.format_name("Test.This.Is.a.test.string.test.30.05.2020"),
            "Test.This.Is.a.Test.String.Test.2020.05.30"
        );
        assert_eq!(
            dots.format_name("Test.This.Is.a.test.string.test.24.02.20"),
            "Test.This.Is.a.Test.String.Test.2024.02.20"
        );
        assert_eq!(
            dots.format_name("Testing date 16.10.20 in the middle"),
            "Testing.Date.2016.10.20.in.the.Middle"
        );
    }

    #[test]
    fn test_prefix_dir() {
        let dots = Dots {
            root: PathBuf::default(),
            config: Config {
                prefix: Some("Test.One.Two".to_string()),
                ..Default::default()
            },
            path_given: false,
        };

        assert_eq!(dots.format_name("example"), "Test.One.Two.Example");
        assert_eq!(dots.format_name("two example"), "Test.One.Two.Example");
        assert_eq!(dots.format_name("1"), "Test.One.Two.1");
        assert_eq!(dots.format_name("Test one  two three"), "Test.One.Two.Three");
        assert_eq!(dots.format_name("three"), "Test.One.Two.Three");
        assert_eq!(dots.format_name("test.one.two"), "Test.One.Two");
        assert_eq!(dots.format_name(" test one two "), "Test.One.Two");
        assert_eq!(dots.format_name("Test.One.Two"), "Test.One.Two");
    }
}

#[cfg(test)]
mod written_date_tests {
    use super::*;

    #[test]
    fn test_single_date() {
        let mut input = "Mar.23.2016".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2016.03.23");

        let mut input = "23.mar.2016".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2016.03.23");

        let mut input = "March.1.2011".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2011.03.01");

        let mut input = "1.March.2011".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2011.03.01");

        let mut input = "December.20.2023".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2023.12.20");

        let mut input = "20.December.2023".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2023.12.20");
    }

    #[test]
    fn test_multiple_dates() {
        let mut input = "Mar.23.2016 Jun.17.2015".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2016.03.23 2015.06.17");
    }

    #[test]
    fn test_mixed_text() {
        let mut input = "Event on Apr.5.2021 at noon".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "Event on 2021.04.05 at noon");
    }

    #[test]
    fn test_edge_case_single_digit_day() {
        let mut input = "Jan.03.2020".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "2020.01.03");
    }

    #[test]
    fn test_no_date_in_text() {
        let mut input = "This text has no date".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "This text has no date");
    }

    #[test]
    fn test_leading_and_trailing_spaces() {
        let mut input = "Something.Feb.Jun.09.2022".to_string();
        Dots::convert_written_date_format(&mut input);
        assert_eq!(input, "Something.Feb.2022.06.09");
    }
}

#[cfg(test)]
mod move_date_tests {
    use super::*;

    static DOTS: LazyLock<Dots> = LazyLock::new(|| Dots {
        root: PathBuf::default(),
        config: Config {
            move_date_after_prefix: vec!["Test".to_string(), "Prefix".to_string()],
            date_starts_with_year: true,
            ..Default::default()
        },
        path_given: false,
    });

    #[test]
    fn test_valid_date() {
        assert_eq!(
            DOTS.format_name("Test something 2010.11.16"),
            "Test.2010.11.16.Something"
        );
        assert_eq!(
            DOTS.format_name("Test something 1080p 2010.11.16"),
            "Test.2010.11.16.Something.1080p"
        );
    }

    #[test]
    fn test_short_date() {
        assert_eq!(
            DOTS.format_name("Test something else 25.05.30"),
            "Test.2025.05.30.Something.Else"
        );
    }

    #[test]
    fn test_no_match_with_valid_date() {
        assert_eq!(
            DOTS.format_name("something else 2024.01.01"),
            "Something.Else.2024.01.01"
        );
        assert_eq!(
            DOTS.format_name("something else 2160p 24.05.28"),
            "Something.Else.2160p.2024.05.28"
        );
    }
}

#[cfg(test)]
mod test_remove_from_start {
    use super::*;

    static DOTS: LazyLock<Dots> = LazyLock::new(|| Dots {
        root: PathBuf::default(),
        config: Config {
            remove_from_start: vec!["Test".to_string(), "test".to_string()],
            ..Default::default()
        },
        path_given: false,
    });

    #[test]
    fn test_no_patterns() {
        assert_eq!(DOTS.format_name("test.string.test"), "String.Test");
    }

    #[test]
    fn test_single_occurrence() {
        assert_eq!(DOTS.format_name("test.string"), "Test.String");
    }

    #[test]
    fn test_multiple_occurrences() {
        assert_eq!(DOTS.format_name("test.string.test.test"), "String.Test");
        assert_eq!(
            DOTS.format_name("test.string.test.something.test"),
            "String.Something.Test"
        );
    }

    #[test]
    fn test_partial_word_match() {
        assert_eq!(DOTS.format_name("testing.test.contest"), "Testing.Test.Contest");
    }

    #[test]
    fn test_consecutive_patterns() {
        assert_eq!(DOTS.format_name("test.test.test.test"), "Test");
    }
}

#[cfg(test)]
mod test_deduplicate_patterns {
    use super::*;

    static DOTS: LazyLock<Dots> = LazyLock::new(|| Dots {
        root: PathBuf::default(),
        config: Config {
            replace: vec![("SomeName.".to_string(), "Some.Name.".to_string())],
            deduplicate_patterns: vec![(
                Regex::new(r"(Some\.Name\.){2,}").expect("valid regex"),
                "Some.Name.".to_string(),
            )],
            ..Default::default()
        },
        path_given: false,
    });

    #[test]
    fn test_no_duplicates() {
        assert_eq!(DOTS.format_name("Some.Name.File"), "Some.Name.File");
    }

    #[test]
    fn test_double_duplicate() {
        assert_eq!(DOTS.format_name("SomeName.SomeName.File"), "Some.Name.File");
        assert_eq!(DOTS.format_name("SomeName.Some.Name.File"), "Some.Name.File");
        assert_eq!(DOTS.format_name("Some.SomeName.Some.Name.File"), "Some.Some.Name.File");
        assert_eq!(
            DOTS.format_name("Something.SomeName.Some.Name.File"),
            "Something.Some.Name.File"
        );
    }

    #[test]
    fn test_triple_duplicate() {
        assert_eq!(DOTS.format_name("SomeName.SomeName.SomeName.File"), "Some.Name.File");
        assert_eq!(DOTS.format_name("SomeName.SomeName.Some.Name.File"), "Some.Name.File");
        assert_eq!(DOTS.format_name("SomeName.File"), "Some.Name.File");
        assert_eq!(DOTS.format_name("Some.Name.File"), "Some.Name.File");
    }

    #[test]
    fn test_substitute_creates_duplicate() {
        assert_eq!(DOTS.format_name("Some.Name.SomeName.File"), "Some.Name.File");
    }

    #[test]
    fn test_mixed_case_duplicates() {
        let dots = Dots {
            root: PathBuf::default(),
            config: Config {
                deduplicate_patterns: vec![(Regex::new(r"(Test\.){2,}").expect("valid regex"), "Test.".to_string())],
                ..Default::default()
            },
            path_given: false,
        };
        assert_eq!(dots.format_name("Test.Test.File"), "Test.File");
    }
}
