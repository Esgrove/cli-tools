use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use colored::Colorize;
use itertools::Itertools;
use serde::Deserialize;
use walkdir::WalkDir;

use cli_tools::{print_error, print_warning};

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Move files to directories based on name")]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Create directories for files with matching prefixes
    #[arg(short, long)]
    create: bool,

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
    create: bool,
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
    create: bool,
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
    /// Path relative to the root directory.
    relative: PathBuf,
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
            create: args.create || user_config.create,
            dryrun: args.print || user_config.dryrun,
            exclude,
            include,
            min_group_size: user_config.min_group_size.unwrap_or(args.group),
            overwrite: args.force || user_config.overwrite,
            prefix_overrides,
            recurse: args.recurse || user_config.recurse,
            verbose: args.verbose || user_config.verbose,
        }
    }
}

impl DirectoryInfo {
    fn new(path: PathBuf, root: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase()
            .replace('.', " ");

        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();

        Self { path, relative, name }
    }
}

impl DirMove {
    pub fn new(args: Args) -> anyhow::Result<Self> {
        let root = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args);
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
        // Collect directories in the base path
        let mut dirs: Vec<DirectoryInfo> = Vec::new();
        // TODO: implement recurse option for dirs
        let _ = self.config.recurse;
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
                        println!("Ignoring directory: {}", entry.path().display());
                    }
                    continue;
                }
                dirs.push(DirectoryInfo::new(entry.path(), &self.root));
            }
        }

        println!("Checking {} directories...", dirs.len());

        // Walk recursively for files
        for entry in WalkDir::new(&self.root).into_iter().filter_map(Result::ok) {
            if entry.file_type().is_file() {
                let file_path = entry.path();
                let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };

                let file_name_lower = file_name.to_lowercase();
                // Skip files that don't match include patterns (if any specified)
                if !self.config.include.is_empty()
                    && !self
                        .config
                        .include
                        .iter()
                        .any(|pattern| file_name_lower.contains(&pattern.to_lowercase()))
                {
                    continue;
                }
                // Skip files that match exclude patterns
                if self
                    .config
                    .exclude
                    .iter()
                    .any(|pattern| file_name_lower.contains(&pattern.to_lowercase()))
                {
                    continue;
                }

                let relative_file = file_path.strip_prefix(&self.root).unwrap_or(file_path);

                for dir in &dirs {
                    if file_name_lower.contains(&dir.name) {
                        // Check if the file is already in the target directory
                        if file_path.starts_with(&dir.path) {
                            continue;
                        }

                        println!("Dir:  {}\nFile: {}", dir.relative.display(), relative_file.display());

                        if !self.config.dryrun {
                            print!("{}", "Move file? (y/n): ".magenta());
                            io::stdout().flush()?;

                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            if input.trim().eq_ignore_ascii_case("y") {
                                let Some(file_name) = file_path.file_name() else {
                                    print_error!("Could not get file name for path: {}", file_path.display());
                                    continue;
                                };
                                let new_path = dir.path.join(file_name);

                                if new_path.exists() && !self.config.overwrite {
                                    print_warning!(
                                        "File already exists at destination (use --force to overwrite): {}",
                                        new_path.display()
                                    );
                                    continue;
                                }

                                match fs::rename(file_path, &new_path) {
                                    Ok(()) => println!("Moved"),
                                    Err(e) => eprintln!("Failed to move file: {e}"),
                                }
                            } else {
                                println!("Skipped");
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Move files to the target directory, creating it if needed.
    fn move_files_to_target_dir(&self, dir_path: &Path, files: &[PathBuf]) -> anyhow::Result<()> {
        if !dir_path.exists() {
            fs::create_dir(dir_path)?;
            if let Some(name) = dir_path.file_name().and_then(|n| n.to_str()) {
                println!("  Created directory: {name}");
            }
        }

        // Move files
        for file_path in files {
            let Some(file_name) = file_path.file_name() else {
                print_error!("Could not get file name for path: {}", file_path.display());
                continue;
            };
            let new_path = dir_path.join(file_name);

            if new_path.exists() && !self.config.overwrite {
                print_warning!(
                    "Skipping existing file: {}",
                    cli_tools::path_to_string_relative(&new_path)
                );
                continue;
            }

            match fs::rename(file_path, &new_path) {
                Ok(()) => {
                    if self.config.verbose {
                        println!("  Moved: {}", file_name.to_string_lossy());
                    }
                }
                Err(e) => print_error!("Failed to move {}: {e}", file_path.display()),
            }
        }
        println!("  Moved {} files", files.len());

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
                prefix_groups.entry(prefix).or_default().push(file_path.clone());
            }
        }

        // Apply prefix overrides: if a group's prefix starts with an override, use the override
        let prefix_groups = self.apply_prefix_overrides(prefix_groups);

        Ok(prefix_groups)
    }

    /// Find the best prefix for a file by checking if other files share the same prefix.
    /// For short simple prefixes (≤4 chars), tries longer prefixes first.
    /// Returns None if only a short prefix exists with no shared longer prefix.
    fn find_best_prefix(file_name: &str, all_files: &[(PathBuf, String)]) -> Option<String> {
        let simple_prefix = file_name.split('.').next().filter(|p| !p.is_empty())?;

        // If simple prefix is longer than 4 chars, use it directly
        if simple_prefix.len() > 4 {
            return Some(simple_prefix.to_string());
        }

        // For short prefixes, try to find shared longer prefixes
        // First try 3-part prefix
        if let Some(three_part) = Self::get_n_part_prefix(file_name, 3) {
            let matches = all_files
                .iter()
                .filter(|(_, name)| name != file_name && Self::get_n_part_prefix(name, 3) == Some(three_part))
                .count();
            if matches > 0 {
                return Some(three_part.to_string());
            }
        }

        // Then try 2-part prefix
        if let Some(two_part) = Self::get_n_part_prefix(file_name, 2) {
            let matches = all_files
                .iter()
                .filter(|(_, name)| name != file_name && Self::get_n_part_prefix(name, 2) == Some(two_part))
                .count();
            if matches > 0 {
                return Some(two_part.to_string());
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
            println!("No file groups with 3 or more matching prefixes found.");
            return Ok(());
        }

        println!(
            "Found {} group(s) with {}+ files sharing the same prefix:\n",
            groups_to_process.len(),
            self.config.min_group_size
        );

        for (prefix, files) in groups_to_process {
            let dir_path = self.root.join(&prefix);
            let dir_exists = dir_path.exists();

            println!("{}: {} files", prefix.cyan().bold(), files.len());
            for file_path in &files {
                if let Some(name) = file_path.file_name().and_then(|n| n.to_str()) {
                    println!("  {name}");
                }
            }

            if dir_exists {
                println!("  {} Directory already exists", "→".green());
            } else {
                println!("  {} Will create directory: {}", "→".yellow(), prefix);
            }

            if !self.config.dryrun {
                print!("{}", "Create directory and move files? (y/n): ".magenta());
                io::stdout().flush()?;

                let mut input = String::new();
                io::stdin().read_line(&mut input)?;

                if input.trim().eq_ignore_ascii_case("y") {
                    if let Err(e) = self.move_files_to_target_dir(&dir_path, &files) {
                        print_error!("Failed to process {}: {e}", prefix);
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
                .cloned()
                .unwrap_or(prefix);

            result.entry(target_prefix).or_default().extend(files);
        }

        result
    }

    /// Extract a prefix consisting of the first n dot-separated parts.
    /// Returns None if there aren't enough parts.
    fn get_n_part_prefix(file_name: &str, n: usize) -> Option<&str> {
        // Need at least n+1 parts (n for prefix, 1 for extension)
        if file_name.split('.').count() <= n {
            return None;
        }

        // Find the position after the nth dot
        let mut pos = 0;
        for _ in 0..n {
            pos = file_name[pos..].find('.')? + pos + 1;
        }

        // Return everything before the last dot we found (subtract 1 to exclude the dot)
        Some(&file_name[..pos - 1])
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
            Some("LongName".to_string())
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
            Some("Some.Name.Thing".to_string())
        );
    }

    #[test]
    fn test_find_best_prefix_short_prefix_fallback_to_two_part() {
        // No 3-part matches, but 2-part matches exist
        let files = make_test_files(&["Some.Name.Thing.mp4", "Some.Name.Other.mp4", "Some.Name.More.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("Some.Name.Thing.mp4", &files),
            Some("Some.Name".to_string())
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
            Some("Some.Name.Thing".to_string())
        );
    }

    #[test]
    fn test_find_best_prefix_exactly_four_char_prefix() {
        // 4-char prefix is still "short", needs longer match
        let files = make_test_files(&["ABCD.Name.Thing.mp4", "ABCD.Name.Other.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCD.Name.Thing.mp4", &files),
            Some("ABCD.Name".to_string())
        );
    }

    #[test]
    fn test_find_best_prefix_five_char_prefix_uses_simple() {
        // 5-char prefix is "long", uses simple prefix directly
        let files = make_test_files(&["ABCDE.Name.Thing.mp4", "ABCDE.Name.Other.mp4"]);
        assert_eq!(
            DirMove::find_best_prefix("ABCDE.Name.Thing.mp4", &files),
            Some("ABCDE".to_string())
        );
    }

    fn make_test_config(prefix_overrides: Vec<String>) -> Config {
        Config {
            create: false,
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
        // Override "Some" should NOT match "Something" (must be prefix match)
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
}
