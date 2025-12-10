mod config;
mod convert;
mod logger;
mod stats;

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use serde::Deserialize;

use crate::convert::VideoConvert;

/// Sort order options for video files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Sort by bitrate (highest first)
    #[default]
    Bitrate,
    /// Sort by bitrate (lowest first)
    BitrateAsc,
    /// Sort by file size (largest first)
    Size,
    /// Sort by file size (smallest first)
    SizeAsc,
    /// Sort by duration (longest first)
    Duration,
    /// Sort by duration (shortest first)
    DurationAsc,
    /// Sort by resolution (highest first)
    Resolution,
    /// Sort by resolution (lowest first)
    ResolutionAsc,
    /// Sort alphabetically by file name
    Name,
    /// Sort reverse alphabetically by file name
    NameDesc,
}

impl FromStr for SortOrder {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "bitrate" => Ok(Self::Bitrate),
            "bitrate_asc" | "bitrate-asc" => Ok(Self::BitrateAsc),
            "size" => Ok(Self::Size),
            "size_asc" | "size-asc" => Ok(Self::SizeAsc),
            "duration" => Ok(Self::Duration),
            "duration_asc" | "duration-asc" => Ok(Self::DurationAsc),
            "resolution" => Ok(Self::Resolution),
            "resolution_asc" | "resolution-asc" => Ok(Self::ResolutionAsc),
            "name" => Ok(Self::Name),
            "name_desc" | "name-desc" => Ok(Self::NameDesc),
            _ => Err(format!("Unknown sort order: {s}")),
        }
    }
}

impl std::fmt::Display for SortOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Bitrate => "bitrate (highest first)",
            Self::BitrateAsc => "bitrate (lowest first)",
            Self::Size => "size (largest first)",
            Self::SizeAsc => "size (smallest first)",
            Self::Duration => "duration (longest first)",
            Self::DurationAsc => "duration (shortest first)",
            Self::Resolution => "resolution (highest first)",
            Self::ResolutionAsc => "resolution (lowest first)",
            Self::Name => "name (alphabetical)",
            Self::NameDesc => "name (reverse alphabetical)",
        };
        write!(f, "{name}")
    }
}

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

    /// Limit the number of files to convert
    #[arg(short, long)]
    count: Option<usize>,

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
    #[arg(short = 'n', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Override file extensions to convert
    #[arg(short = 't', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION", conflicts_with_all = ["all", "other"])]
    extension: Vec<String>,

    /// Convert all known video file types except MP4 files
    #[arg(short, long, conflicts_with = "all")]
    other: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Skip conversion
    #[arg(short = 'k', long)]
    skip_convert: bool,

    /// Skip remuxing
    #[arg(short = 'm', long)]
    skip_remux: bool,

    /// Sort files
    #[arg(short = 's', long, name = "ORDER", default_value_t = SortOrder::Name)]
    sort: SortOrder,

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
