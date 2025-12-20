mod config;
mod dots;

use std::path::PathBuf;

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use regex::Regex;

use crate::dots::Dots;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Rename files to use dot formatting")]
pub(crate) struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath, global = true)]
    path: Option<PathBuf>,

    /// Convert casing
    #[arg(short = 'c', long, global = true)]
    case: bool,

    /// Enable debug prints
    #[arg(short = 'D', long, global = true)]
    debug: bool,

    /// Rename directories
    #[arg(short = 'd', long, global = true)]
    directory: bool,

    /// Overwrite existing files
    #[arg(short = 'f', long, global = true)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE", global = true)]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE", global = true)]
    exclude: Vec<String>,

    /// Increment conflicting file name with running index
    #[arg(short = 'i', long, global = true)]
    increment: bool,

    /// Only print changes without renaming files
    #[arg(short = 'p', long, global = true)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long, global = true)]
    recurse: bool,

    /// Append prefix to the start
    #[arg(short = 'x', long)]
    prefix: Option<String>,

    /// Prefix files with directory name
    #[arg(short = 'b', long, conflicts_with = "prefix")]
    prefix_dir: bool,

    /// Force `prefix_dir` to always be at the start of the filename (implies --prefix-dir)
    #[arg(short = 'B', long, conflicts_with = "prefix")]
    prefix_dir_start: bool,

    /// Prefix files with their parent directory name (implies --prefix-dir --recurse)
    #[arg(short = 'R', long, conflicts_with = "prefix")]
    prefix_dir_recursive: bool,

    /// Suffix files with directory name
    #[arg(short = 'j', long, conflicts_with = "suffix")]
    suffix_dir: bool,

    /// Suffix files with their parent directory name (implies --suffix-dir --recurse)
    #[arg(short = 'J', long, conflicts_with = "suffix")]
    suffix_dir_recursive: bool,

    /// Append suffix to the end
    #[arg(short = 'u', long)]
    suffix: Option<String>,

    /// Substitute pattern with replacement in filenames
    #[arg(short = 's', long, num_args = 2, action = clap::ArgAction::Append, value_names = ["PATTERN", "REPLACEMENT"], global = true)]
    substitute: Vec<String>,

    /// Remove random strings
    #[arg(short = 'm', long, global = true)]
    random: bool,

    /// Remove pattern from filenames
    #[arg(short = 'z', long, num_args = 1, action = clap::ArgAction::Append, name = "PATTERN", global = true)]
    remove: Vec<String>,

    /// Substitute regex pattern with replacement in filenames
    #[arg(short = 'g', long, num_args = 2, action = clap::ArgAction::Append, value_names = ["PATTERN", "REPLACEMENT"], global = true)]
    regex: Vec<String>,

    /// Assume year is last in short dates
    #[arg(short = 'y', long, global = true)]
    year: bool,

    /// Create shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Prefix files with a name or parent directory name
    #[command(name = "prefix")]
    Prefix {
        /// Input directory
        #[arg(value_hint = clap::ValueHint::DirPath)]
        path: Option<PathBuf>,

        /// Prefix name (if not specified, uses parent directory name)
        #[arg(short = 'x', long)]
        name: Option<String>,

        /// Force prefix to always be at the start of the filename
        #[arg(short = 'S', long)]
        start: bool,

        /// Use each file's parent directory name as prefix (implies --recurse)
        #[arg(short = 'R', long)]
        recursive: bool,
    },

    /// Suffix files with a name or parent directory name
    #[command(name = "suffix")]
    Suffix {
        /// Input directory
        #[arg(value_hint = clap::ValueHint::DirPath)]
        path: Option<PathBuf>,

        /// Suffix name (if not specified, uses parent directory name)
        #[arg(short = 'x', long)]
        name: Option<String>,

        /// Use each file's parent directory name as suffix (implies --recurse)
        #[arg(short = 'R', long)]
        recursive: bool,
    },
}

impl Args {
    /// Apply subcommand options to the main args.
    fn apply_subcommand(&mut self) {
        match &self.command {
            Some(Command::Prefix {
                path,
                name,
                start,
                recursive,
            }) => {
                if path.is_some() {
                    self.path = path.clone();
                }
                if let Some(prefix_name) = name {
                    self.prefix = Some(prefix_name.clone());
                } else if *recursive {
                    self.prefix_dir_recursive = true;
                } else {
                    self.prefix_dir = true;
                }
                if *start {
                    self.prefix_dir_start = true;
                }
            }
            Some(Command::Suffix { path, name, recursive }) => {
                if path.is_some() {
                    self.path = path.clone();
                }
                if let Some(suffix_name) = name {
                    self.suffix = Some(suffix_name.clone());
                } else if *recursive {
                    self.suffix_dir_recursive = true;
                } else {
                    self.suffix_dir = true;
                }
            }
            None => {}
        }
    }

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
    let mut args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        args.apply_subcommand();
        Dots::run_with_args(args)
    }
}
