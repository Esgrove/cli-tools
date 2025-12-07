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

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Only print changes without renaming files
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
        self.move_files_to_dir()
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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DirMove::new(args)?.run()
    }
}
