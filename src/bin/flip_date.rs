use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use serde::Deserialize;
use walkdir::WalkDir;

static FILE_EXTENSIONS: [&str; 9] = ["m4a", "mp3", "txt", "rtf", "csv", "mp4", "mkv", "mov", "avi"];

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Flip dates in file and directory names to start with year"
)]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Use directory rename mode
    #[arg(short, long)]
    dir: bool,

    /// Overwrite existing
    #[arg(short, long)]
    force: bool,

    /// Specify file extension(s)
    #[arg(short, long, num_args = 1, action = clap::ArgAction::Append, value_name = "EXTENSION", conflicts_with = "dir")]
    extensions: Option<Vec<String>>,

    /// Assume year is first in short dates
    #[arg(short, long)]
    year: bool,

    /// Only print changes without renaming
    #[arg(short, long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Swap year and day around
    #[arg(short, long)]
    swap: bool,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct DateConfig {
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    dryrun: bool,
    #[serde(default)]
    file_extensions: Vec<String>,
    overwrite: bool,
    #[serde(default)]
    recurse: bool,
    #[serde(default)]
    swap_year: bool,
    #[serde(default)]
    verbose: bool,
    #[serde(default)]
    year_first: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    flip_date: DateConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
struct Config {
    directory_mode: bool,
    dryrun: bool,
    file_extensions: Vec<String>,
    overwrite: bool,
    recurse: bool,
    swap_year: bool,
    verbose: bool,
    year_first: bool,
}

#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

impl DateConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|config_string| toml::from_str::<UserConfig>(&config_string).ok())
            .map(|config| config.flip_date)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn from_args(args: Args) -> Self {
        let user_config = DateConfig::get_user_config();

        // Determine which extensions to use (args > config > default)
        let file_extensions = args
            .extensions
            .filter(|extensions| !extensions.is_empty())
            .or({
                if user_config.file_extensions.is_empty() {
                    None
                } else {
                    Some(user_config.file_extensions)
                }
            })
            .unwrap_or_else(|| FILE_EXTENSIONS.iter().map(std::string::ToString::to_string).collect());

        Self {
            directory_mode: args.dir || user_config.directory,
            dryrun: args.print || user_config.dryrun,
            file_extensions,
            overwrite: args.force || user_config.overwrite,
            recurse: args.recurse || user_config.recurse,
            swap_year: args.swap || user_config.swap_year,
            verbose: args.verbose || user_config.verbose,
            year_first: args.year || user_config.year_first,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    let config = Config::from_args(args);
    if config.directory_mode {
        date_flip_directories(path, &config)
    } else {
        date_flip_files(&path, &config)
    }
}

/// Flip date to start with year for all matching files from the given path.
fn date_flip_files(path: &PathBuf, config: &Config) -> Result<()> {
    let (files, root) = files_to_rename(path, &config.file_extensions, config.recurse)?;
    if files.is_empty() {
        anyhow::bail!("No files to process");
    }

    let mut files_to_rename: Vec<RenameItem> = Vec::new();
    for file in files {
        let filename = file
            .file_name()
            .context("Failed to get filename")?
            .to_string_lossy()
            .into_owned();

        if let Some(new_name) =
            cli_tools::date::reorder_filename_date(&filename, config.year_first, config.swap_year, config.verbose)
            && new_name.to_lowercase() != filename.to_lowercase()
        {
            files_to_rename.push(RenameItem {
                path: file,
                filename,
                new_name,
            });
        }
    }

    // Case-insensitive sort by filename
    files_to_rename.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));

    let heading = if config.dryrun {
        "Dryrun:".cyan().bold()
    } else {
        "Rename:".magenta().bold()
    };

    for item in files_to_rename {
        let new_path = root.join(&item.new_name);
        if new_path == item.path {
            continue;
        }
        println!("{heading}");
        cli_tools::show_diff(&item.filename, &item.new_name);
        if !config.dryrun {
            if new_path.exists() && !config.overwrite {
                eprintln!("{}", "File already exists".yellow());
            } else {
                fs::rename(item.path, new_path).context("Failed to rename file")?;
            }
        }
    }

    Ok(())
}

/// Flip date to start with year for all matching directories from the given path.
fn date_flip_directories(path: PathBuf, config: &Config) -> Result<()> {
    let directories = directories_to_rename(path, config.recurse)?;
    if directories.is_empty() {
        anyhow::bail!("No directories to rename")
    }

    let max_chars: usize = directories
        .iter()
        .map(|r| r.filename.chars().count())
        .max()
        .context("Failed to get max path length")?;

    for directory in directories {
        let new_path = directory.path.with_file_name(directory.new_name.clone());
        println!(
            "{:<width$}  ==>  {}",
            directory.filename,
            directory.new_name,
            width = max_chars
        );
        if !config.dryrun {
            fs::rename(&directory.path, &new_path).with_context(|| {
                format!(
                    "Failed to rename {} to {}",
                    directory.path.display(),
                    new_path.display()
                )
            })?;
        }
    }

    Ok(())
}

/// Get list of files to process
fn files_to_rename(path: &PathBuf, file_extensions: &[String], recurse: bool) -> Result<(Vec<PathBuf>, PathBuf)> {
    let (mut files, root) = if path.is_file() {
        (
            vec![path.clone()],
            path.parent().context("Failed to get file parent")?.to_path_buf(),
        )
    } else {
        let list: Vec<PathBuf> = WalkDir::new(path)
            .min_depth(1)
            .max_depth(if recurse { usize::MAX } else { 1 })
            .into_iter()
            .filter_map(std::result::Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|ext| {
                        file_extensions
                            .iter()
                            .any(|e| ext.to_str().is_some_and(|ext_str| e == ext_str))
                    })
            })
            .collect();
        (list, path.clone())
    };

    files.sort_unstable();
    Ok((files, root))
}

/// Get list of directories to process
fn directories_to_rename(path: PathBuf, recurse: bool) -> Result<Vec<RenameItem>> {
    let mut directories_to_rename = Vec::new();

    let walker = WalkDir::new(path).min_depth(1).max_depth(if recurse { 100 } else { 1 });

    for entry in walker {
        let entry = entry.context("Failed to read directory entry")?;
        if entry.path().is_dir() {
            let filename = entry.file_name().to_string_lossy().into_owned();
            if let Some(new_name) = cli_tools::date::reorder_directory_date(&filename) {
                directories_to_rename.push(RenameItem {
                    path: entry.path().to_path_buf(),
                    filename,
                    new_name,
                });
            }
        }
    }

    // Case-insensitive sort by filename
    directories_to_rename.sort_by(|a, b| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()));

    Ok(directories_to_rename)
}
