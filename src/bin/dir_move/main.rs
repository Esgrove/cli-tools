mod config;
mod dir_move;
mod types;
mod utils;

use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::dir_move::DirMove;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Move files to directories based on name")]
struct DirMoveArgs {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Auto-confirm all prompts without asking
    #[arg(short = 'a', long)]
    auto: bool,

    /// Create directories for files with matching prefixes
    #[arg(short = 'c', long)]
    create: bool,

    /// Print debug information
    #[arg(short = 'D', long)]
    debug: bool,

    /// Overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Ignore prefix when matching filenames
    #[arg(short = 'i', long = "ignore", num_args = 1, action = clap::ArgAction::Append, name = "IGNORE")]
    prefix_ignore: Vec<String>,

    /// Override prefix to use for directory names
    #[arg(short = 'o', long = "override", num_args = 1, action = clap::ArgAction::Append, name = "OVERRIDE")]
    prefix_override: Vec<String>,

    /// Directory name to "unpack" by moving its contents to the parent directory
    #[arg(short = 'u', long = "unpack", num_args = 1, action = clap::ArgAction::Append, name = "NAME")]
    unpack_directory: Vec<String>,

    /// Minimum number of matching files needed to create a group
    #[arg(short = 'g', long, name = "COUNT")]
    group: Option<usize>,

    /// Minimum character count for prefixes to be valid group names (excluding dots)
    #[arg(short = 'm', long = "min-chars", name = "CHARS")]
    min_prefix_chars: Option<usize>,

    /// Only print changes without moving files
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = DirMoveArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, DirMoveArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DirMove::try_from_args(args)?.run()
    }
}
