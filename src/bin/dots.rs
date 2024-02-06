extern crate colored;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use walkdir::WalkDir;

use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    name = "dots",
    about = "Replace whitespaces with dots in filenames"
)]
struct Args {
    /// Optional input directory or file
    input_dir: String,

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

fn replace_whitespaces<P: Into<PathBuf>>(
    path: P,
    dryrun: bool,
    overwrite: bool,
    verbose: bool,
) -> Result<()> {
    let path = path.into();
    let mut num_renamed: usize = 0;
    if verbose {
        println!(
            "{}",
            format!("Formatting files under {}", path.display()).bold()
        )
    }
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if file_name.contains(' ') {
                    let new_file_name = file_name.replace(' ', ".");
                    let new_path = path.with_file_name(new_file_name);
                    if dryrun {
                        println!("Dryrun: {} to {}", path.display(), new_path.display());
                        num_renamed += 1;
                    } else if new_path.exists() && !overwrite {
                        println!(
                            "{}",
                            format!(
                                "Skipping rename to already existing file: {}",
                                new_path.display()
                            )
                            .yellow()
                        )
                    } else {
                        match fs::rename(path, &new_path) {
                            Ok(_) => {
                                println!("Renamed {} to {}", path.display(), new_path.display());
                                num_renamed += 1;
                            }
                            Err(e) => {
                                eprintln!("{}", format!("Error renaming {:?}: {}", path, e).red());
                            }
                        }
                    }
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

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = args.input_dir.trim();
    if input_path.is_empty() {
        anyhow::bail!("empty input path");
    }
    let filepath = Path::new(input_path);
    if !filepath.is_dir() {
        anyhow::bail!(
            "Input directory does not exist or is not accessible: '{}'",
            filepath.display()
        );
    }
    let absolute_input_path = fs::canonicalize(filepath)?;
    replace_whitespaces(absolute_input_path, args.print, args.force, args.verbose)
}
