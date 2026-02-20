mod config;
mod flip_date;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Flip dates in file and directory names to start with year"
)]
pub struct Args {
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
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let path = cli_tools::resolve_input_path(args.path.as_deref())?;
    let config = flip_date::Config::from_args(args)?;
    if config.directory_mode {
        flip_date::date_flip_directories(path, &config)
    } else {
        flip_date::date_flip_files(&path, &config)
    }
}
