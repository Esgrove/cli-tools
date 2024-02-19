extern crate colored;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use walkdir::WalkDir;

use std::fs;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, name = "dots", about = "Replace whitespaces in filenames with dots")]
struct Args {
    /// Optional input directory or file
    path: Option<String>,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Only print changes without renaming
    #[arg(short, long)]
    print: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = cli_tools::resolve_input_path(args.path)?;
    replace_whitespaces(input_path, args.print, args.force, args.verbose)
}

fn replace_whitespaces(root: PathBuf, dryrun: bool, overwrite: bool, verbose: bool) -> Result<()> {
    if verbose {
        println!("{}", format!("Formatting files under {}", root.display()).bold())
    }

    // Collect all files that need renaming
    let mut files_to_rename: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| !cli_tools::is_hidden(e))
        .filter_map(|e| e.ok())
    {
        let path = entry.path().to_path_buf();
        if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if file_name.contains(' ') && !file_name.contains(" - ") {
                    let new_file_name = file_name.replace(' ', ".");
                    let new_path = path.with_file_name(new_file_name);
                    files_to_rename.push((path, new_path));
                }
            }
        }
    }

    files_to_rename.sort_by_key(|k| k.0.clone().to_string_lossy().to_lowercase());
    if verbose {
        println!("Found {} files to rename", files_to_rename.len())
    }

    let mut num_renamed: usize = 0;
    for (path, new_path) in files_to_rename {
        let old_str = cli_tools::get_relative_path_or_filename(&path, &root);
        let new_str = cli_tools::get_relative_path_or_filename(&new_path, &root);
        if dryrun {
            println!("{}", "Dryrun:".bold());
            println!("{old_str}\n{new_str}");
            num_renamed += 1;
        } else if new_path.exists() && !overwrite {
            println!(
                "{}",
                format!("Skipping rename to already existing file: {new_str}").yellow()
            )
        } else {
            match fs::rename(&path, &new_path) {
                Ok(_) => {
                    println!("{}", "Rename:".bold().magenta());
                    println!("{old_str}\n{new_str}");
                    num_renamed += 1;
                }
                Err(e) => {
                    eprintln!("{}", format!("Error renaming {old_str}: {e}").red());
                }
            }
        }
    }

    if dryrun {
        println!("Dryrun: would have renamed {} files", num_renamed);
    } else {
        println!("{}", format!("Renamed {} files", num_renamed).green());
    }
    Ok(())
}
