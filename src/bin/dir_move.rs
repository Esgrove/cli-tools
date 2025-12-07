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
    overwrite: bool,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    dirmove: MoveConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
struct Config {
    create: bool,
    dryrun: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    overwrite: bool,
    recurse: bool,
    verbose: bool,
}

#[derive(Debug)]
struct DirectoryInfo {
    path: PathBuf,
    relative: PathBuf,
    name: String,
}

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
        Self {
            create: args.create || user_config.create,
            dryrun: args.print || user_config.dryrun,
            include,
            exclude,
            overwrite: args.force || user_config.overwrite,
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
        let mut prefix_groups: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            let file_path = entry.path();
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if !self.config.include.is_empty() && !self.config.include.iter().any(|pattern| file_name.contains(pattern))
            {
                continue;
            }
            if !self.config.exclude.is_empty() && self.config.exclude.iter().any(|pattern| file_name.contains(pattern))
            {
                continue;
            }

            // Get prefix and group files
            if let Some(prefix) = Self::get_file_prefix(file_name) {
                prefix_groups.entry(prefix.to_string()).or_default().push(file_path);
            }
        }

        Ok(prefix_groups)
    }

    /// Create directories for files with matching prefixes and move files into them.
    /// Only considers files directly in the base path (not recursive).
    fn create_dirs_and_move_files(&self) -> anyhow::Result<()> {
        let prefix_groups = self.collect_files_by_prefix()?;

        // Filter to only groups with 3+ files
        let groups_to_process: Vec<_> = prefix_groups
            .into_iter()
            .filter(|(_, files)| files.len() >= 3)
            .sorted_by(|a, b| a.0.cmp(&b.0))
            .collect();

        if groups_to_process.is_empty() {
            println!("No file groups with 3 or more matching prefixes found.");
            return Ok(());
        }

        println!(
            "Found {} group(s) with 3+ files sharing the same prefix:\n",
            groups_to_process.len()
        );

        for (prefix, files) in groups_to_process {
            let dir_path = self.root.join(&prefix);
            let dir_exists = dir_path.exists();

            println!("{} ({} files)", prefix.cyan().bold(), files.len());
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

    /// Extract the prefix from a filename (the part before the first dot).
    /// Returns None if the filename has no dot or starts with a dot.
    fn get_file_prefix(file_name: &str) -> Option<&str> {
        // Skip hidden files (starting with dot)
        if file_name.starts_with('.') {
            return None;
        }

        file_name.split('.').next().filter(|prefix| !prefix.is_empty())
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
