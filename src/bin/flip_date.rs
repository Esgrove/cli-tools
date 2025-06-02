use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use walkdir::WalkDir;

static FILE_EXTENSIONS: [&str; 7] = ["m4a", "mp3", "txt", "rtf", "csv", "mp4", "mkv"];

#[derive(Parser)]
#[command(
    author,
    version,
    name = "flip-date",
    about = "Flip dates in file and directory names to start with year"
)]
struct Args {
    /// Optional input directory or file
    path: Option<String>,

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

    /// Use recursive path handling
    #[arg(short, long)]
    recursive: bool,

    /// Swap year and day around
    #[arg(short, long)]
    swap: bool,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug)]
struct RenameItem {
    path: PathBuf,
    filename: String,
    new_name: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    if args.dir {
        date_flip_directories(path, args.recursive, args.print)
    } else {
        let extensions = args.extensions.unwrap_or_default();
        let extensions_owned;
        let file_extensions: &[&str] = if extensions.is_empty() {
            &FILE_EXTENSIONS
        } else {
            extensions_owned = extensions.iter().map(String::as_str).collect::<Vec<_>>();
            &extensions_owned
        };

        date_flip_files(
            &path,
            file_extensions,
            args.recursive,
            args.print,
            args.year,
            args.force,
            args.swap,
            args.verbose,
        )
    }
}

/// Flip date to start with year for all matching files from the given path.
fn date_flip_files(
    path: &PathBuf,
    file_extensions: &[&str],
    recursive: bool,
    dryrun: bool,
    starts_with_year: bool,
    overwrite_existing: bool,
    swap_year: bool,
    verbose: bool,
) -> Result<()> {
    let (files, root) = files_to_rename(path, file_extensions, recursive)?;
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

        if let Some(new_name) = cli_tools::date::reorder_filename_date(&filename, starts_with_year, swap_year, verbose)
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

    let heading = if dryrun {
        "Dryrun:".cyan().bold()
    } else {
        "Rename:".magenta().bold()
    };

    for item in files_to_rename {
        println!("{heading}");
        cli_tools::show_diff(&item.filename, &item.new_name);
        if !dryrun {
            let new_path = root.join(item.new_name);
            if new_path.exists() && !overwrite_existing {
                eprintln!("{}", "File already exists".yellow());
            } else {
                fs::rename(item.path, new_path).context("Failed to rename file")?;
            }
        }
    }

    Ok(())
}

/// Flip date to start with year for all matching directories from given path.
fn date_flip_directories(path: PathBuf, recursive: bool, dryrun: bool) -> Result<()> {
    let directories = directories_to_rename(path, recursive)?;
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
        if !dryrun {
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
fn files_to_rename(path: &PathBuf, file_extensions: &[&str], recursive: bool) -> Result<(Vec<PathBuf>, PathBuf)> {
    let (mut files, root) = if path.is_file() {
        (
            vec![path.clone()],
            path.parent().context("Failed to get file parent")?.to_path_buf(),
        )
    } else {
        let list: Vec<PathBuf> = WalkDir::new(path)
            .min_depth(1)
            .max_depth(if recursive { usize::MAX } else { 1 })
            .into_iter()
            .filter_map(std::result::Result::ok)
            .map(walkdir::DirEntry::into_path)
            .filter(|path| {
                path.is_file()
                    && path.extension().is_some_and(|ext| {
                        // I want debug formatting here for extension since it shows all characters
                        #[allow(clippy::unnecessary_debug_formatting)]
                        file_extensions.contains(
                            &ext.to_str()
                                .unwrap_or_else(|| panic!("Invalid file extension: {ext:#?}")),
                        )
                    })
            })
            .collect();
        (list, path.clone())
    };

    files.sort();
    Ok((files, root))
}

/// Get list of directories to process
fn directories_to_rename(path: PathBuf, recursive: bool) -> Result<Vec<RenameItem>> {
    let mut directories_to_rename = Vec::new();

    let walker = WalkDir::new(path)
        .min_depth(1)
        .max_depth(if recursive { 100 } else { 1 });

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
