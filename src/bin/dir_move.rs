use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use colored::Colorize;
use itertools::Itertools;
use serde::Deserialize;

use cli_tools::{
    get_relative_path_or_filename, path_to_filename_string, path_to_string_relative, print_bold, print_error,
    print_warning,
};

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Move files to directories based on name")]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Auto-confirm all prompts without asking
    #[arg(short, long)]
    auto: bool,

    /// Create directories for files with matching prefixes
    #[arg(short, long)]
    create: bool,

    /// Print debug information
    #[arg(short = 'D', long)]
    debug: bool,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Override prefix to use for directory names
    #[arg(short = 'o', long = "override", num_args = 1, action = clap::ArgAction::Append, name = "OVERRIDE")]
    prefix_override: Vec<String>,

    /// Minimum number of matching files needed to create a group
    #[arg(short, long, name = "COUNT", default_value_t = 3)]
    group: usize,

    /// Only print changes without moving files
    #[arg(short, long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct MoveConfig {
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    debug: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    min_group_size: Option<usize>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    prefix_overrides: Vec<String>,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the user config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dirmove: MoveConfig,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
struct Config {
    auto: bool,
    create: bool,
    debug: bool,
    dryrun: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    min_group_size: usize,
    overwrite: bool,
    prefix_overrides: Vec<String>,
    recurse: bool,
    verbose: bool,
}

/// Information about a directory used for matching files to move.
#[derive(Debug)]
struct DirectoryInfo {
    /// Absolute path to the directory.
    path: PathBuf,
    /// Normalized directory name (lowercase, dots replaced with spaces).
    name: String,
}

#[derive(Debug)]
struct DirMove {
    root: PathBuf,
    config: Config,
}

impl MoveConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|e| {
                        print_error!("Error reading config file {}: {e}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|e| {
                        print_error!("Error reading config file: {e}");
                    })
                    .ok()
            })
            .map(|config| config.dirmove)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: Args) -> Self {
        let user_config = MoveConfig::get_user_config();
        let include: Vec<String> = user_config.include.into_iter().chain(args.include).unique().collect();
        let exclude: Vec<String> = user_config.exclude.into_iter().chain(args.exclude).unique().collect();
        let prefix_overrides: Vec<String> = user_config
            .prefix_overrides
            .into_iter()
            .chain(args.prefix_override)
            .unique()
            .collect();
        Self {
            auto: args.auto || user_config.auto,
            create: args.create || user_config.create,
            debug: args.debug || user_config.debug,
            dryrun: args.print || user_config.dryrun,
            include,
            exclude,
            min_group_size: user_config.min_group_size.unwrap_or(args.group),
            overwrite: args.force || user_config.overwrite,
            prefix_overrides,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
        }
    }
}

impl DirectoryInfo {
    fn new(path: PathBuf) -> Self {
        let name = path_to_filename_string(&path).to_lowercase().replace('.', " ");
        Self { path, name }
    }
}

impl DirMove {
    pub fn new(args: Args) -> anyhow::Result<Self> {
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args);
        if config.debug {
            eprintln!("Config: {config:#?}");
            eprintln!("Root: {}", root.display());
        }
        Ok(Self { root, config })
    }

    pub fn run(&self) -> anyhow::Result<()> {
        if self.config.create {
            self.create_dirs_and_move_files()
        } else {
            self.move_files_to_dir()
        }
    }

    fn move_files_to_dir(&self) -> anyhow::Result<()> {
        // TODO: implement recurse option for dirs
        let _ = self.config.recurse;

        let directories = self.collect_directories_in_root()?;
        if directories.is_empty() {
            if self.config.verbose {
                println!("No directories found in current path.");
            }
            return Ok(());
        }

        let files_in_root = self.collect_files_in_root()?;
        if files_in_root.is_empty() {
            if self.config.verbose {
                println!("No files found in current directory.");
            }
            return Ok(());
        }

        let matches = Self::match_files_to_directories(&files_in_root, &directories);
        if matches.is_empty() {
            if self.config.verbose {
                println!("No files found matching any directory names.");
            }
            return Ok(());
        }

        // Sort by directory name and process
        let groups_to_process: Vec<_> = matches
            .into_iter()
            .map(|(idx, files)| (&directories[idx], files))
            .sorted_by(|a, b| a.0.name.cmp(&b.0.name))
            .collect();

        print_bold!(
            "Found {} directory match(es) with files to move:\n",
            groups_to_process.len()
        );

        for (dir, files) in groups_to_process {
            self.process_directory_match(dir, &files)?;
        }

        Ok(())
    }

    fn collect_directories_in_root(&self) -> anyhow::Result<Vec<DirectoryInfo>> {
        let mut dirs = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let dir_name = entry.file_name().to_string_lossy().to_lowercase();
                if !self.config.exclude.is_empty()
                    && self
                        .config
                        .exclude
                        .iter()
                        .any(|pattern| dir_name.contains(&pattern.to_lowercase()))
                {
                    if self.config.verbose {
                        println!("Excluding directory: {}", path_to_string_relative(&entry.path()));
                    }
                    continue;
                }
                dirs.push(DirectoryInfo::new(entry.path()));
            }
        }
        Ok(dirs)
    }

    fn collect_files_in_root(&self) -> anyhow::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let file_name = entry.file_name().to_string_lossy().to_lowercase();

                // Skip files that don't match include patterns (if any specified)
                if !self.config.include.is_empty()
                    && !self
                        .config
                        .include
                        .iter()
                        .any(|pattern| file_name.contains(&pattern.to_lowercase()))
                {
                    continue;
                }
                // Skip files that match exclude patterns
                if self
                    .config
                    .exclude
                    .iter()
                    .any(|pattern| file_name.contains(&pattern.to_lowercase()))
                {
                    continue;
                }

                files.push(entry.path());
            }
        }
        Ok(files)
    }

    fn match_files_to_directories(files: &[PathBuf], dirs: &[DirectoryInfo]) -> HashMap<usize, Vec<PathBuf>> {
        let mut matches: HashMap<usize, Vec<PathBuf>> = HashMap::new();

        // Sort directory indices by name length (longest first) to match more specific names first
        let mut dir_indices: Vec<usize> = (0..dirs.len()).collect();
        dir_indices.sort_by(|&a, &b| dirs[b].name.len().cmp(&dirs[a].name.len()));

        for file_path in files {
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Normalize: replace dots with spaces for matching
            let file_name_normalized = file_name.replace('.', " ").to_lowercase();

            for &idx in &dir_indices {
                // dir.name is already lowercase
                // Check if the normalized filename contains the directory name
                if file_name_normalized.contains(&dirs[idx].name) {
                    matches.entry(idx).or_default().push(file_path.clone());
                    // Only match to first directory found
                    break;
                }
            }
        }

        matches
    }

    fn process_directory_match(&self, dir: &DirectoryInfo, files: &[PathBuf]) -> anyhow::Result<()> {
        let dir_display = get_relative_path_or_filename(&dir.path, &self.root);
        println!("{}: {} file(s)", dir_display.cyan().bold(), files.len());

        for file_path in files {
            println!("  {}", path_to_filename_string(file_path));
        }

        println!("  {} Move to: {dir_display}", "→".green());

        if !self.config.dryrun {
            let confirmed = if self.config.auto {
                true
            } else {
                print!("{}", "Move files to this directory? (y/n): ".magenta());
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                input.trim().eq_ignore_ascii_case("y")
            };

            if confirmed {
                if let Err(e) = self.move_files_to_target_dir(&dir.path, files) {
                    print_error!("Failed to move files to {}: {e}", dir.path.display());
                }
            } else {
                println!("  Skipped");
            }
        }
        println!();

        Ok(())
    }

    /// Move files to the target directory, creating it if needed.
    fn move_files_to_target_dir(&self, dir_path: &Path, files: &[PathBuf]) -> anyhow::Result<()> {
        if !dir_path.exists() {
            fs::create_dir(dir_path)?;
            println!("  Created directory: {}", path_to_filename_string(dir_path));
        }

        let mut moved_count = 0;
        for file_path in files {
            let file_name = path_to_filename_string(file_path);
            if file_name.is_empty() {
                print_error!("Could not get file name for path: {}", file_path.display());
                continue;
            }
            let new_path = dir_path.join(&file_name);

            if new_path.exists() && !self.config.overwrite {
                print_warning!("Skipping existing file: {}", new_path.display());
                continue;
            }

            match fs::rename(file_path, &new_path) {
                Ok(()) => {
                    if self.config.verbose {
                        println!("  Moved: {file_name}");
                    }
                    moved_count += 1;
                }
                Err(e) => print_error!("Failed to move {}: {e}", file_path.display()),
            }
        }
        println!("  Moved {moved_count} files");

        Ok(())
    }

    /// Collect files from base path and group them by prefix.
    fn collect_files_by_prefix(&self) -> anyhow::Result<HashMap<String, Vec<PathBuf>>> {
        // First pass: collect all files with their filename
        let mut files_with_names: Vec<(PathBuf, String)> = Vec::new();

        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            let file_path = entry.path();
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };

            // Skip hidden files
            if file_name.starts_with('.') {
                continue;
            }

            if !self.config.include.is_empty() && !self.config.include.iter().any(|pattern| file_name.contains(pattern))
            {
                continue;
            }
            if !self.config.exclude.is_empty() && self.config.exclude.iter().any(|pattern| file_name.contains(pattern))
            {
                continue;
            }

            files_with_names.push((file_path, file_name));
        }

        // Second pass: determine best prefix for each file
        let mut prefix_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for (file_path, file_name) in &files_with_names {
            if let Some(prefix) = Self::find_best_prefix(file_name, &files_with_names) {
                prefix_groups
                    .entry(prefix.into_owned())
                    .or_default()
                    .push(file_path.clone());
            }
        }

        // Apply prefix overrides: if a group's prefix starts with an override, use the override
        let prefix_groups = self.apply_prefix_overrides(prefix_groups);

        Ok(prefix_groups)
    }

    /// Find the best prefix for a file by checking if other files share the same prefix.
    /// For short simple prefixes (≤4 chars), tries longer prefixes first.
    /// Returns None if only a short prefix exists with no shared longer prefix.
    fn find_best_prefix<'a>(file_name: &'a str, all_files: &[(PathBuf, String)]) -> Option<Cow<'a, str>> {
        let simple_prefix = file_name.split('.').next().filter(|p| !p.is_empty())?;

        // If simple prefix is longer than 4 chars, use it directly
        if simple_prefix.len() > 4 {
            return Some(Cow::Borrowed(simple_prefix));
        }

        // For short prefixes, try to find shared longer prefixes
        // First try 3-part prefix
        if let Some(three_part) = Self::get_n_part_prefix(file_name, 3) {
            let has_matches = all_files
                .iter()
                .any(|(_, name)| name != file_name && Self::get_n_part_prefix(name, 3) == Some(three_part));
            if has_matches {
                return Some(Cow::Borrowed(three_part));
            }
        }

        // Then try 2-part prefix
        if let Some(two_part) = Self::get_n_part_prefix(file_name, 2) {
            let has_matches = all_files
                .iter()
                .any(|(_, name)| name != file_name && Self::get_n_part_prefix(name, 2) == Some(two_part));
            if has_matches {
                return Some(Cow::Borrowed(two_part));
            }
        }

        // No shared longer prefix found for short simple prefix, skip this file
        None
    }

    /// Create directories for files with matching prefixes and move files into them.
    /// Only considers files directly in the base path (not recursive).
    fn create_dirs_and_move_files(&self) -> anyhow::Result<()> {
        let prefix_groups = self.collect_files_by_prefix()?;

        // Filter to only groups with 3+ files
        let groups_to_process: Vec<_> = prefix_groups
            .into_iter()
            .filter(|(_, files)| files.len() >= self.config.min_group_size)
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect();

        if groups_to_process.is_empty() {
            if self.config.verbose {
                println!("No file groups with 3 or more matching prefixes found.");
            }
            return Ok(());
        }

        print_bold!(
            "Found {} group(s) with {}+ files sharing the same prefix:\n",
            groups_to_process.len(),
            self.config.min_group_size
        );

        for (prefix, files) in groups_to_process {
            let dir_name = prefix.replace('.', " ");
            let dir_path = self.root.join(&dir_name);
            let dir_exists = dir_path.exists();

            println!("{}: {} files", dir_name.cyan().bold(), files.len());
            for file_path in &files {
                println!("  {}", path_to_filename_string(file_path));
            }

            if dir_exists {
                println!("  {} Directory already exists", "→".green());
            } else {
                println!("  {} Will create directory: {dir_name}", "→".yellow());
            }

            if !self.config.dryrun {
                let confirmed = if self.config.auto {
                    true
                } else {
                    print!("{}", "Create directory and move files? (y/n): ".magenta());
                    io::stdout().flush()?;

                    let mut input = String::new();
                    io::stdin().read_line(&mut input)?;
                    input.trim().eq_ignore_ascii_case("y")
                };

                if confirmed {
                    if let Err(e) = self.move_files_to_target_dir(&dir_path, &files) {
                        print_error!("Failed to process {}: {e}", dir_name);
                    }
                } else {
                    println!("  Skipped");
                }
            }
            println!();
        }

        Ok(())
    }

    /// Apply prefix overrides to groups.
    /// If files in a group start with an override prefix, merge them under the override name.
    fn apply_prefix_overrides(&self, groups: HashMap<String, Vec<PathBuf>>) -> HashMap<String, Vec<PathBuf>> {
        if self.config.prefix_overrides.is_empty() {
            return groups;
        }

        let mut result: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for (prefix, files) in groups {
            // Check if any override matches: either the prefix starts with override,
            // or the override starts with the prefix (override is more specific),
            // or any file in the group starts with the override
            let target_prefix = self
                .config
                .prefix_overrides
                .iter()
                .find(|&override_prefix| {
                    prefix.starts_with(override_prefix)
                        || override_prefix.starts_with(&prefix)
                        || files.iter().any(|f| {
                            f.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|name| name.starts_with(override_prefix))
                        })
                })
                .map_or(prefix, std::clone::Clone::clone);

            result.entry(target_prefix).or_default().extend(files);
        }

        result
    }

    /// Extract a prefix consisting of the first n dot-separated parts.
    /// Returns None if there aren't enough parts.
    fn get_n_part_prefix(file_name: &str, n: usize) -> Option<&str> {
        let mut dots_found = 0;
        let mut nth_dot_pos = 0;

        for (i, c) in file_name.bytes().enumerate() {
            if c == b'.' {
                dots_found += 1;
                if dots_found == n {
                    nth_dot_pos = i;
                } else if dots_found > n {
                    // Found more than n dots, return prefix up to nth dot
                    return Some(&file_name[..nth_dot_pos]);
                }
            }
        }

        // If we found exactly n dots, that's n+1 parts which is enough
        if dots_found >= n && nth_dot_pos > 0 {
            return Some(&file_name[..nth_dot_pos]);
        }

        // Not enough parts
        None
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DirMove::new(args)?.run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_files(names: &[&str]) -> Vec<(PathBuf, String)> {
        names.iter().map(|n| (PathBuf::from(*n), (*n).to_string())).collect()
    }

    #[test]
    fn test_get_n_part_prefix_three_parts() {
        assert_eq!(
            DirMove::get_n_part_prefix("Some.Name.Thing.v1.mp4", 3),
            Some("Some.Name.Thing")
        );
    }

    #[test]
    fn test_get_n_part_prefix_two_parts() {
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.Thing.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn test_get_n_part_prefix_not_enough_parts() {
        // Need n+1 parts minimum (n for prefix, 1 for extension)
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.mp4", 3), None);
        assert_eq!(DirMove::get_n_part_prefix("Some.mp4", 2), None);
    }

    #[test]
    fn test_get_n_part_prefix_exact_parts() {
        // 3 parts total, asking for 2-part prefix should work
        assert_eq!(DirMove::get_n_part_prefix("Some.Name.mp4", 2), Some("Some.Name"));
    }

    #[test]
    fn test_find_best_prefix_long_simple_prefix() {
        // Simple prefix > 4 chars should be used directly
        let files = make_test_files(&["LongName.v1.mp4", "Other.v2.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("LongName.v1.mp4", &files),
            Some(Cow::Borrowed("LongName"))
        );
    }

    #[test]
    fn test_find_best_prefix_short_prefix_no_matches() {
        // Short prefix with no shared longer prefix should return None
        let files = make_test_files(&["ABC.random.mp4", "XYZ.other.mp4"]);
        assert_eq!(DirMove::find_best_prefix("ABC.random.mp4", &files), None);
    }

    #[test]
    fn test_find_best_prefix_short_prefix_with_three_part_match() {
        // Files sharing 3-part prefix should be grouped by that
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
        ]);
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.v1.mp4", &files),
            Some(Cow::Borrowed("Some.Name.Thing"))
        );
    }

    #[test]
    fn test_find_best_prefix_short_prefix_fallback_to_two_part() {
        // No 3-part matches, but 2-part matches exist
        let files = make_test_files(&["Some.Name.Thing.mp4", "Some.Name.Other.mp4", "Some.Name.More.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("Some.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_prefers_three_part_over_two_part() {
        // When both 3-part and 2-part matches exist, prefer 3-part
        let files = make_test_files(&[
            "Some.Name.Thing.v1.mp4",
            "Some.Name.Thing.v2.mp4",
            "Some.Name.Other.v1.mp4",
            "Some.Name.Other.v2.mp4",
        ]);
        // Should match on 3-part, not fall back to 2-part
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.v1.mp4", &files),
            Some(Cow::Borrowed("Some.Name.Thing"))
        );
    }

    #[test]
    fn test_find_best_prefix_exactly_four_char_prefix() {
        // 4-char prefix is still "short", needs longer match
        let files = make_test_files(&["ABCD.Name.Thing.mp4", "ABCD.Name.Other.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCD.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("ABCD.Name"))
        );
    }

    #[test]
    fn test_find_best_prefix_five_char_prefix_uses_simple() {
        // 5-char prefix is "long", uses simple prefix directly
        let files = make_test_files(&["ABCDE.Name.Thing.mp4", "ABCDE.Name.Other.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCDE.Name.Thing.mp4", &files),
            Some(Cow::Borrowed("ABCDE"))
        );
    }

    fn make_test_config(prefix_overrides: Vec<String>) -> Config {
        Config {
            auto: false,
            create: false,
            debug: false,
            dryrun: true,
            include: Vec::new(),
            exclude: Vec::new(),
            min_group_size: 3,
            overwrite: false,
            prefix_overrides,
            recurse: false,
            verbose: false,
        }
    }

    fn make_test_dirmove(prefix_overrides: Vec<String>) -> DirMove {
        DirMove {
            root: PathBuf::from("."),
            config: make_test_config(prefix_overrides),
        }
    }

    #[test]
    fn test_apply_prefix_overrides_no_overrides() {
        let dirmove = make_test_dirmove(Vec::new());
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_matching_override() {
        let dirmove = make_test_dirmove(vec!["longer.prefix".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert(
            "longer.prefix.name".to_string(),
            vec![PathBuf::from("longer.prefix.name.file.mp4")],
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("longer.prefix"));
        assert!(!result.contains_key("longer.prefix.name"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_merges_groups() {
        let dirmove = make_test_dirmove(vec!["Some.Name".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);
        groups.insert("Some.Name.Other".to_string(), vec![PathBuf::from("file2.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Some.Name").map(Vec::len), Some(2));
    }

    #[test]
    fn test_apply_prefix_overrides_non_matching() {
        let dirmove = make_test_dirmove(vec!["Other.Prefix".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Some.Name.Thing".to_string(), vec![PathBuf::from("file1.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Some.Name.Thing"));
        assert!(!result.contains_key("Other.Prefix"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_partial_match_only() {
        let dirmove = make_test_dirmove(vec!["Some".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert("Something.Else".to_string(), vec![PathBuf::from("file1.mp4")]);
        groups.insert("Some.Name".to_string(), vec![PathBuf::from("file2.mp4")]);

        let result = dirmove.apply_prefix_overrides(groups);
        // "Something.Else" starts with "Some" so it gets merged
        // "Some.Name" also starts with "Some" so it gets merged
        assert!(result.contains_key("Some"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_apply_prefix_overrides_override_more_specific_than_prefix() {
        // Override "Example.Name" is more specific than computed prefix "Example"
        // Files start with "Example.Name" so override should apply
        let dirmove = make_test_dirmove(vec!["Example.Name".to_string()]);
        let mut groups: HashMap<String, Vec<PathBuf>> = HashMap::new();
        groups.insert(
            "Example".to_string(),
            vec![
                PathBuf::from("Example.Name.Video1.mp4"),
                PathBuf::from("Example.Name.Video2.mp4"),
                PathBuf::from("Example.Name.Video3.mp4"),
            ],
        );

        let result = dirmove.apply_prefix_overrides(groups);
        assert!(result.contains_key("Example.Name"));
        assert!(!result.contains_key("Example"));
        assert_eq!(result.len(), 1);
        assert_eq!(result.get("Example.Name").map(Vec::len), Some(3));
    }

    fn make_test_dirs(names: &[&str]) -> Vec<DirectoryInfo> {
        names
            .iter()
            .map(|n| DirectoryInfo {
                path: PathBuf::from(*n),
                name: n.to_lowercase(),
            })
            .collect()
    }

    fn make_file_paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from(*n)).collect()
    }

    #[test]
    fn test_match_files_to_directories_basic_match() {
        // Directory: "Certain Name", files with "Certain.Name" should match
        let dirs = make_test_dirs(&["Certain Name"]);
        let files = make_file_paths(&[
            "Something.else.Certain.Name.video.1.mp4",
            "Certain.Name.Example.video.2.mp4",
            "Another.Certain.Name.Example.video.3.mp4",
            "Another.Name.Example.video.3.mp4",
            "Cert.Name.Example.video.3.mp4",
            "Certain.Not.Example.video.mp4",
        ]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&0));
        assert_eq!(result.get(&0).map(Vec::len), Some(3));
    }

    #[test]
    fn test_match_files_to_directories_no_matches() {
        let dirs = make_test_dirs(&["Some Directory"]);
        let files = make_file_paths(&["unrelated.file.mp4", "another.file.txt"]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_to_directories_multiple_dirs() {
        let dirs = make_test_dirs(&["First Dir", "Second Dir"]);
        let files = make_file_paths(&["First.Dir.file1.mp4", "Second.Dir.file2.mp4", "First.Dir.file3.mp4"]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 2);
        assert_eq!(result.get(&0).map(Vec::len), Some(2));
        assert_eq!(result.get(&1).map(Vec::len), Some(1));
    }

    #[test]
    fn test_match_files_to_directories_case_insensitive() {
        let dirs = make_test_dirs(&["My Directory"]);
        let files = make_file_paths(&[
            "MY.DIRECTORY.file1.mp4",
            "my.directory.file2.mp4",
            "My.Directory.file3.mp4",
        ]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).map(Vec::len), Some(3));
    }

    #[test]
    fn test_match_files_to_directories_partial_match() {
        // Directory "Test Name" should NOT match "Testing.Name.file.mp4"
        // because "testing name file mp4" does not contain "test name" as substring
        // ("testing" != "test ")
        let dirs = make_test_dirs(&["Test Name"]);
        let files = make_file_paths(&["Testing.Name.file.mp4", "Test.Name.file.mp4"]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        // Only "Test.Name.file.mp4" matches because normalized is "test name file mp4"
        // which contains "test name"
        // "Testing.Name.file.mp4" normalized is "testing name file mp4" which does NOT
        // contain "test name" (it has "testing name" not "test name")
        assert_eq!(result.get(&0).map(Vec::len), Some(1));
    }

    #[test]
    fn test_match_files_to_directories_longer_match_wins() {
        // If a file could match multiple directories, longer/more specific name wins
        // e.g., "ProjectNew" should match before "Project"
        let dirs = make_test_dirs(&["Project", "ProjectNew"]);
        let files = make_file_paths(&["ProjectNew.2025.10.12.file.mp4", "Project.2025.10.05.file.mp4"]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        // Should have matches for both directories
        assert_eq!(result.len(), 2);
        // "ProjectNew" file should match "ProjectNew" directory (index 1), not "Project"
        assert!(result.contains_key(&1));
        assert_eq!(result.get(&1).map(Vec::len), Some(1));
        // "Project" file should match "Project" directory (index 0)
        assert!(result.contains_key(&0));
        assert_eq!(result.get(&0).map(Vec::len), Some(1));
    }

    #[test]
    fn test_match_files_to_directories_empty_files() {
        let dirs = make_test_dirs(&["Some Dir"]);
        let files: Vec<PathBuf> = Vec::new();

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_to_directories_empty_dirs() {
        let dirs: Vec<DirectoryInfo> = Vec::new();
        let files = make_file_paths(&["some.file.mp4"]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_to_directories_dots_replaced_with_spaces() {
        // Verify that dots in filenames are replaced with spaces for matching
        // Directory "My Show" should match "My.Show.S01E01.mp4"
        let dirs = make_test_dirs(&["My Show"]);
        let files = make_file_paths(&["My.Show.S01E01.mp4", "My.Show.S01E02.mp4"]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).map(Vec::len), Some(2));
    }

    #[test]
    fn test_match_files_to_directories_mixed_separators() {
        // Files with various separators in names
        let dirs = make_test_dirs(&["Show Name"]);
        let files = make_file_paths(&[
            "Show.Name.episode.mp4",
            "Other.Show.Name.here.mp4",
            "prefix.Show.Name.suffix.mp4",
        ]);

        let result = DirMove::match_files_to_directories(&files, &dirs);

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&0).map(Vec::len), Some(3));
    }
}
