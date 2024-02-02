extern crate colored;

use anyhow::{Result};
use clap::Parser;
use colored::Colorize;

use std::path::Path;
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(author, about, version)]
struct Args {
    /// Optional input directory or file
    input_dir: String,

    /// Do not ask for confirmation
    #[arg(short, long)]
    force: bool,

    /// Only print changes
    #[arg(short, long)]
    print: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn replace_whitespaces<P: Into<PathBuf>>(path: P) -> Result<()> {
    let path = path.into();
    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if file_name.contains(' ') {
                    let new_file_name = file_name.replace(' ', ".");
                    let new_path = path.with_file_name(new_file_name);
                    match fs::rename(&path, &new_path) {
                        Ok(_) => println!("Renamed {} to {}", path.display(), new_path.display()),
                        Err(e) => eprintln!("{}", format!("Error renaming {:?}: {}", path, e).red()),
                    }
                }
            }
        }
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
    replace_whitespaces(absolute_input_path)
}
