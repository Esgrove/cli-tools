mod config;
mod convert;
mod logger;
mod stats;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::convert::VideoConvert;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
pub(crate) struct VideoConvertArgs {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types (default is only .mp4 and .mkv)
    #[arg(short, long)]
    all: bool,

    /// Skip files with bitrate lower than LIMIT kbps
    #[arg(short, long, name = "LIMIT", default_value_t = 8000)]
    bitrate: u64,

    /// Delete input files immediately instead of moving to trash
    #[arg(short, long)]
    delete: bool,

    /// Print commands without running them
    #[arg(short, long)]
    print: bool,

    /// Overwrite existing output files
    #[arg(short, long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'i', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Override file extensions to convert
    #[arg(short = 't', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION", conflicts_with_all = ["all", "other"])]
    extension: Vec<String>,

    /// Number of files to convert
    #[arg(short, long, default_value_t = 1)]
    number: usize,

    /// Convert all known video file types except MP4 files
    #[arg(short, long, conflicts_with = "all")]
    other: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Skip conversion
    #[arg(short = 'c', long)]
    skip_convert: bool,

    /// Skip remuxing
    #[arg(short = 'm', long)]
    skip_remux: bool,

    /// Sort files by bitrate (highest first)
    #[arg(short = 's', long)]
    sort_by_bitrate: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = VideoConvertArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, VideoConvertArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        VideoConvert::new(args)?.run()
    }
}
