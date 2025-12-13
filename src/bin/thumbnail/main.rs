mod config;
mod thumbnail;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::thumbnail::ThumbnailCreator;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Create thumbnails for video files using ffmpeg")]
pub(crate) struct ThumbnailArgs {
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

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = ThumbnailArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, ThumbnailArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        ThumbnailCreator::new(&args)?.run()
    }
}
