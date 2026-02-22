mod config;
mod flip_date;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Flip dates in file and directory names to start with year"
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<FlipDateCommand>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

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

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Swap year and day around
    #[arg(short, long)]
    swap: bool,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,
}

/// Subcommands for flipdate.
#[derive(Subcommand)]
enum FlipDateCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion {
        /// Shell to generate completion for
        #[arg(value_enum)]
        shell: Shell,

        /// Install completion script to the shell's completion directory
        #[arg(short = 'I', long)]
        install: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(FlipDateCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    let config = flip_date::Config::from_args(args)?;
    if config.directory_mode {
        flip_date::date_flip_directories(path, &config)
    } else {
        flip_date::date_flip_files(&path, &config)
    }
}
