use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use regex::Regex;
use walkdir::WalkDir;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\({\]}\)]+").expect("Failed to create regex pattern for brackets"));

static RE_WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

static RE_DOTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.{2,}").unwrap());

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

    /// Recursive directory iteration
    #[arg(short, long)]
    recursive: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = cli_tools::resolve_input_path(args.path)?;
    replace_whitespaces(input_path, args.print, args.recursive, args.force, args.verbose)
}

fn replace_whitespaces(root: PathBuf, dryrun: bool, recursive: bool, overwrite: bool, verbose: bool) -> Result<()> {
    if verbose {
        println!("{}", format!("Formatting files under {}", root.display()).bold())
    }

    let max_depth = if recursive { 100 } else { 1 };

    // Collect all files that need renaming
    let mut files_to_rename: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in WalkDir::new(&root)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| !cli_tools::is_hidden(e))
        .filter_map(|e| e.ok())
    {
        let path = entry.path().to_path_buf();
        if path.is_file() {
            if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                let new_file_name = format_name(file_name);
                let new_path = path.with_file_name(new_file_name);
                if path != new_path {
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
            cli_tools::show_diff(&old_str, &new_str);
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
                    cli_tools::show_diff(&old_str, &new_str);
                    num_renamed += 1;
                }
                Err(e) => {
                    eprintln!("{}", format!("Error renaming: {old_str}\n{e}").red());
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

fn format_name(file_name: &str) -> String {
    let mut new_file_name = file_name
        .replace("-=-", ".")
        .replace("WEBDL", ".")
        .replace(" - ", " ")
        .replace([' ', '_', '='], ".")
        .replace(".-.", ".")
        .replace(".&.", ".")
        .replace(",.", ".")
        .replace(".rq", "")
        .replace(".HEVC", "");

    new_file_name = RE_WHITESPACE.replace_all(&new_file_name, "").to_string();
    new_file_name = RE_BRACKETS.replace_all(&new_file_name, "").to_string();
    new_file_name = RE_DOTS.replace_all(&new_file_name, ".").to_string();

    new_file_name.trim().to_string()
}

#[cfg(test)]
mod dots_tests {
    use super::*;

    #[test]
    fn test_format_basic() {
        assert_eq!(format_name("Some file.txt"), "Some.file.txt");
    }

    #[test]
    fn test_format_name_no_brackets() {
        assert_eq!(format_name("John Doe - Document"), "John.Doe.Document");
    }

    #[test]
    fn test_format_name_with_brackets() {
        assert_eq!(
            format_name("Project Report - [Final Version]"),
            "Project.Report.Final.Version"
        );
    }

    #[test]
    fn test_format_name_with_parentheses() {
        assert_eq!(format_name("Meeting Notes (2023) - Draft"), "Meeting.Notes.2023.Draft");
    }

    #[test]
    fn test_format_name_with_newlines() {
        assert_eq!(
            format_name("Meeting \tNotes \n(2023) - Draft\r\n"),
            "Meeting.Notes.2023.Draft"
        );
    }

    #[test]
    fn test_format_name_empty_string() {
        assert_eq!(format_name(""), "");
    }

    #[test]
    fn test_format_name_no_changes() {
        assert_eq!(format_name("SingleWord"), "SingleWord");
    }
}
