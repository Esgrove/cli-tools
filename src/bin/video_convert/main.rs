mod cli;
mod config;
mod convert;
mod database;
mod logger;
mod stats;
mod types;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

pub use crate::cli::{DatabaseMode, SortOrder};
use crate::convert::VideoConvert;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
pub(crate) struct VideoConvertArgs {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types
    #[arg(short = 'a', long)]
    all: bool,

    /// Skip files with bitrate lower than LIMIT kbps
    #[arg(short = 'b', long, name = "LIMIT", default_value_t = 8000)]
    bitrate: u64,

    /// Limit the number of files to convert
    #[arg(short = 'c', long)]
    count: Option<usize>,

    /// Delete input files immediately instead of moving to trash
    #[arg(short = 'd', long)]
    delete: bool,

    /// Print commands without running them
    #[arg(short = 'p', long)]
    print: bool,

    /// Overwrite existing output files
    #[arg(short = 'f', long)]
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
    #[arg(short = 'o', long, conflicts_with = "all")]
    other: bool,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Skip conversion
    #[arg(short = 'k', long)]
    skip_convert: bool,

    /// Skip remuxing
    #[arg(short = 'm', long)]
    skip_remux: bool,

    /// Sort files
    #[arg(short = 's', long, name = "ORDER", num_args = 0..=1, default_missing_value = "bitrate")]
    sort: Option<SortOrder>,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Process files from database instead of scanning
    #[arg(short = 'D', long = "from-db", group = "db_mode")]
    from_db: bool,

    /// Clear all entries from the database
    #[arg(short = 'C', long = "clear-db", group = "db_mode")]
    clear_db: bool,

    /// Show database statistics and contents
    #[arg(short = 'S', long = "show-db", group = "db_mode")]
    show_db: bool,

    /// List file extension counts in the database
    #[arg(short = 'E', long = "list-extensions", group = "db_mode")]
    list_extensions: bool,

    /// Maximum bitrate in kbps
    #[arg(short = 'B', long = "max-bitrate", name = "MAX_BITRATE")]
    max_bitrate: Option<u64>,

    /// Minimum duration in seconds
    #[arg(short = 'u', long = "min-duration", name = "MIN_DURATION")]
    min_duration: Option<f64>,

    /// Maximum duration in seconds
    #[arg(short = 'U', long = "max-duration", name = "MAX_DURATION")]
    max_duration: Option<f64>,

    /// Maximum number of files to display
    #[arg(short = 'L', long = "display-limit", name = "DISPLAY_LIMIT")]
    display_limit: Option<usize>,
}

impl VideoConvertArgs {
    /// Get the database operation mode if any database flag is set.
    pub const fn database_mode(&self) -> Option<DatabaseMode> {
        if self.from_db {
            Some(DatabaseMode::Process)
        } else if self.clear_db {
            Some(DatabaseMode::Clear)
        } else if self.show_db {
            Some(DatabaseMode::Show)
        } else if self.list_extensions {
            Some(DatabaseMode::ListExtensions)
        } else {
            None
        }
    }
}

fn main() -> Result<()> {
    let args = VideoConvertArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, VideoConvertArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        VideoConvert::new(args)?.run()
    }
}
