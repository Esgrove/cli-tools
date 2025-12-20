mod config;
mod dots;

use std::path::PathBuf;

use anyhow::Context;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use regex::Regex;

use crate::dots::Dots;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Rename files to use dot formatting")]
pub(crate) struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert casing
    #[arg(short = 'c', long)]
    case: bool,

    /// Enable debug prints
    #[arg(short = 'D', long)]
    debug: bool,

    /// Rename directories
    #[arg(short = 'd', long)]
    directory: bool,

    /// Overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Increment conflicting file name with running index
    #[arg(short = 'i', long)]
    increment: bool,

    /// Only print changes without renaming files
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Append prefix to the start
    #[arg(short = 'x', long)]
    prefix: Option<String>,

    /// Prefix files with directory name
    #[arg(short = 'b', long, conflicts_with = "prefix")] // , value_hint = clap::ValueHint::DirPath
    prefix_dir: bool,

    /// Force `prefix_dir` to always be at the start of the filename (implies --prefix-dir)
    #[arg(short = 'B', long, conflicts_with = "prefix")]
    prefix_dir_start: bool,

    /// Suffix files with directory name
    #[arg(short = 'j', long, conflicts_with = "suffix")] // , value_hint = clap::ValueHint::DirPath
    suffix_dir: bool,

    /// Append suffix to the end
    #[arg(short = 'u', long)]
    suffix: Option<String>,

    /// Substitute pattern with replacement in filenames
    #[arg(short = 's', long, num_args = 2, action = clap::ArgAction::Append, value_names = ["PATTERN", "REPLACEMENT"])]
    substitute: Vec<String>,

    /// Remove random strings
    #[arg(short = 'm', long)]
    random: bool,

    /// Remove pattern from filenames
    #[arg(short = 'z', long, num_args = 1, action = clap::ArgAction::Append, name = "PATTERN")]
    remove: Vec<String>,

    /// Substitute regex pattern with replacement in filenames
    #[arg(short = 'g', long, num_args = 2, action = clap::ArgAction::Append, value_names = ["PATTERN", "REPLACEMENT"])]
    regex: Vec<String>,

    /// Assume year is last in short dates
    #[arg(short = 'y', long)]
    year: bool,

    /// Create shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

impl Args {
    /// Collect substitutes to replace pairs.
    fn parse_substitutes(&self) -> Vec<(String, String)> {
        self.substitute
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    let pattern = chunk[0].trim().to_string();
                    let replace = chunk[1].trim().to_string();
                    if pattern.is_empty() {
                        eprintln!("Empty replace pattern: '{pattern}' -> '{replace}'");
                        None
                    } else {
                        Some((pattern, replace))
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Collect removes to replace pairs.
    fn parse_removes(&self) -> Vec<(String, String)> {
        self.remove
            .iter()
            .filter_map(|remove| {
                let pattern = remove.trim().to_string();
                let replace = String::new();
                if pattern.is_empty() {
                    eprintln!("Empty remove pattern: '{pattern}'");
                    None
                } else {
                    Some((pattern, replace))
                }
            })
            .collect()
    }

    /// Collect and compile regex substitutes to replace pairs.
    fn parse_regex_substitutes(&self) -> anyhow::Result<Vec<(Regex, String)>> {
        self.regex
            .chunks(2)
            .filter_map(|chunk| {
                if chunk.len() == 2 {
                    match Regex::new(&chunk[0]).with_context(|| format!("Invalid regex: '{}'", chunk[0])) {
                        Ok(regex) => Some(Ok((regex, chunk[1].clone()))),
                        Err(e) => Some(Err(e)),
                    }
                } else {
                    None
                }
            })
            .collect()
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        Dots::run_with_args(args)
    }
}
