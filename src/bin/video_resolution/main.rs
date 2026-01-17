mod cli;
mod config;
mod resolution;

use std::path::PathBuf;

use clap::Parser;

use config::Config;

#[derive(Parser, Debug)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Add video resolution to filenames")]
pub struct Args {
    /// Optional input directory or file path
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Enable debug prints
    #[arg(short = 'D', long)]
    debug: bool,

    /// Delete files with width or height smaller than limit (default: 500)
    #[arg(short = 'x', long)]
    #[allow(clippy::option_option)]
    delete: Option<Option<u32>>,

    /// Overwrite existing files
    #[arg(short = 'f', long)]
    force: bool,

    /// Only print file names without renaming or deleting
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::try_from_args(&args)?;
    cli::run(config).await
}
