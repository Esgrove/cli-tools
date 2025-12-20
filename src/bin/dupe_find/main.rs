mod config;
mod dupe_find;
mod tui;

use std::path::PathBuf;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::dupe_find::DupeFind;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Find duplicate video files based on identifier patterns")]
struct Args {
    /// Input directories to search
    #[arg(value_hint = clap::ValueHint::DirPath)]
    paths: Vec<PathBuf>,

    /// Identifier patterns to search for (regex)
    #[arg(short = 'g', long, num_args = 1, action = clap::ArgAction::Append, name = "PATTERN")]
    pattern: Vec<String>,

    /// File extensions to include
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION")]
    extension: Vec<String>,

    /// Move duplicates to a "Duplicates" directory
    #[arg(short = 'm', long = "move")]
    move_files: bool,

    /// Only print changes without moving files
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Use default paths from config file
    #[arg(short = 'd', long)]
    default: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        DupeFind::new(args)?.run()
    }
}
