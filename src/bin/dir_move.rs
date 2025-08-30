use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use colored::Colorize;
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

    /// Filter file names to rename
    #[arg(short = 'w', long, num_args = 1, action = clap::ArgAction::Append, name = "FILTER_PATTERN")]
    filter: Vec<String>,

    /// Directory names to ignore
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, name = "IGNORE_PATTERN")]
    ignore: Vec<String>,

    /// Only print changes without renaming files
    #[arg(short, long)]
    print: bool,

    /// Recursive directory iteration
    #[arg(short, long)]
    recursive: bool,

    /// Generate shell completion
    #[arg(short = 'l', long)]
    completion: Option<Shell>,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct MoveConfig {
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    filter: Vec<String>,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
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
    filter_names: Vec<String>,
    ignore_dirs: Vec<String>,
    overwrite: bool,
    recursive: bool,
    verbose: bool,
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
        let mut filter_names = user_config.filter;
        filter_names.extend(args.filter);
        let mut ignore_dirs = user_config.ignore;
        ignore_dirs.extend(args.ignore);
        Self {
            dryrun: args.print || user_config.dryrun,
            filter_names,
            ignore_dirs,
            overwrite: args.force || user_config.overwrite,
            recursive: args.recursive || user_config.recursive,
            verbose: args.verbose || user_config.verbose,
        }
    }
}

#[derive(Debug)]
struct DirectoryInfo {
    path: PathBuf,
    relative: PathBuf,
    name: String,
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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = cli_tools::resolve_input_path(args.path.as_deref())?;
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        let config = Config::from_args(args);
        move_files_to_dir(&root, &config)
    }
}

fn move_files_to_dir(base_path: &Path, config: &Config) -> anyhow::Result<()> {
    // Collect directories in the base path
    let mut dirs: Vec<DirectoryInfo> = Vec::new();
    // TODO: implement recursive option for dirs
    let _ = config.recursive;
    for entry in fs::read_dir(base_path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let dir_name = entry.file_name().to_string_lossy().to_lowercase();
            if !config.ignore_dirs.is_empty()
                && config
                    .ignore_dirs
                    .iter()
                    .any(|ignore| dir_name.contains(&ignore.to_lowercase()))
            {
                if config.verbose {
                    println!("Ignoring directory: {}", entry.path().display());
                }
                continue;
            }
            dirs.push(DirectoryInfo::new(entry.path(), base_path));
        }
    }

    println!("Checking {} directories...", dirs.len());

    // Walk recursively for files
    for entry in WalkDir::new(base_path).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            let file_path = entry.path();
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            let file_name_lower = file_name.to_lowercase();
            if !config.filter_names.is_empty()
                && !config
                    .filter_names
                    .iter()
                    .any(|filter| file_name_lower.contains(&filter.to_lowercase()))
            {
                continue;
            }

            let relative_file = file_path.strip_prefix(base_path).unwrap_or(file_path);

            for dir in &dirs {
                if file_name_lower.contains(&dir.name) {
                    // Check if the file is already in the target directory
                    if file_path.starts_with(&dir.path) {
                        continue;
                    }

                    println!("Dir:  {}\nFile: {}", dir.relative.display(), relative_file.display());

                    if !config.dryrun {
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

                            if new_path.exists() && !config.overwrite {
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
