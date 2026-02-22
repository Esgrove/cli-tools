mod config;
mod thumbnail;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use crate::thumbnail::ThumbnailCreator;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Create thumbnail sheets for video files using ffmpeg")]
pub(crate) struct ThumbnailArgs {
    #[command(subcommand)]
    command: Option<ThumbnailCommand>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Overwrite existing thumbnail files
    #[arg(short = 'f', long)]
    force: bool,

    /// Print commands without running them
    #[arg(short = 'p', long)]
    print: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Number of columns in the thumbnail grid
    #[arg(short = 'c', long, name = "COLS")]
    cols: Option<u32>,

    /// Number of rows in the thumbnail grid
    #[arg(short = 'w', long, name = "ROWS")]
    rows: Option<u32>,

    /// Thumbnail width in pixels
    #[arg(short = 's', long, name = "WIDTH")]
    scale: Option<u32>,

    /// Padding between tiles in pixels
    #[arg(short = 'a', long, name = "PIXELS")]
    padding: Option<u32>,

    /// Font size for timestamp overlay
    #[arg(short = 't', long, name = "SIZE")]
    fontsize: Option<u32>,

    /// JPEG quality (1-31, lower is better)
    #[arg(short = 'q', long, name = "QUALITY")]
    quality: Option<u32>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

/// Subcommands for thumbs.
#[derive(Subcommand)]
enum ThumbnailCommand {
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

fn main() -> Result<()> {
    let args = ThumbnailArgs::parse();
    if let Some(ThumbnailCommand::Completion { shell, install }) = &args.command {
        cli_tools::generate_shell_completion(*shell, ThumbnailArgs::command(), *install, env!("CARGO_BIN_NAME"))
    } else {
        ThumbnailCreator::new(&args)?.run()
    }
}
