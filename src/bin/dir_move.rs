use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
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

    /// Filter items to rename
    #[arg(short = 'w', long, num_args = 1, action = clap::ArgAction::Append, name = "FILTER_PATTERN")]
    filter: Vec<String>,

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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = cli_tools::resolve_input_path(args.path.as_ref().map(|p| p.to_str().unwrap_or("")))?;
    args.completion.as_ref().map_or_else(
        || move_files_to_dir(&root, args.print, args.force, args.verbose),
        |shell| cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME")),
    )
}

pub fn move_files_to_dir(base_path: &Path, dryrun: bool, overwrite: bool, verbose: bool) -> anyhow::Result<()> {
    // Collect directories in the base path
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(base_path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }

    // Walk recursively for files
    for entry in WalkDir::new(base_path).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() {
            let file_path = entry.path();
            let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            let relative_file = file_path.strip_prefix(base_path).unwrap_or(file_path);

            for dir in &dirs {
                let dir_name_lower = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_lowercase()
                    .replace('.', " ");
                let file_name_lower = file_name.to_lowercase().replace('.', " ");

                if file_name_lower.contains(&dir_name_lower) {
                    // Check if the file is already in the target directory
                    if file_path.starts_with(dir) {
                        continue;
                    }

                    let relative_dir = dir.strip_prefix(base_path).unwrap_or(dir);
                    if verbose {
                        println!(
                            "Match found:\n  Dir:  {}\n  File: {}",
                            relative_dir.display(),
                            relative_file.display()
                        );
                    }

                    if !dryrun {
                        print!("Move this file? (y/N): ");
                        io::stdout().flush()?;

                        let mut input = String::new();
                        io::stdin().read_line(&mut input)?;
                        if input.trim().eq_ignore_ascii_case("y") {
                            let Some(file_name) = file_path.file_name() else {
                                print_error!("Could not get file name for path: {}", file_path.display());
                                continue;
                            };
                            let new_path = dir.join(file_name);

                            if new_path.exists() && !overwrite {
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
