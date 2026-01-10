mod config;
mod convert;
mod database;
mod logger;
mod stats;

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use colored::Colorize;
use serde::Deserialize;

use crate::config::{Config, VideoConvertConfig};
use crate::convert::VideoConvert;
use crate::database::Database;

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

/// Database operation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseMode {
    /// Process files from database instead of scanning.
    Process,
    /// Clear all entries from the database.
    Clear,
    /// Show database statistics and contents.
    Show,
    /// List file extension counts in the database.
    ListExtensions,
}

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
pub(crate) struct VideoConvertArgs {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types
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
    #[arg(short = 's', long, name = "ORDER", num_args = 0..=1, default_missing_value = "bitrate")]
    sort: Option<SortOrder>,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short, long)]
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
            Self::Bitrate => "bitrate",
            Self::BitrateAsc => "bitrate-asc",
            Self::Size => "size",
            Self::SizeAsc => "size-asc",
            Self::Duration => "duration",
            Self::DurationAsc => "duration-asc",
            Self::Resolution => "resolution",
            Self::ResolutionAsc => "resolution-asc",
            Self::Name => "name",
            Self::NameDesc => "name-desc",
        };
        write!(f, "{name}")
    }
}

fn main() -> Result<()> {
    let args = VideoConvertArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, VideoConvertArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else if let Some(db_mode) = args.database_mode() {
        handle_database_mode(db_mode, args)
    } else {
        VideoConvert::new(args)?.run()
    }
}

/// Handle database-specific operations.
fn handle_database_mode(mode: DatabaseMode, args: VideoConvertArgs) -> Result<()> {
    match mode {
        DatabaseMode::Clear => {
            let database = Database::open_default()?;
            let cleared = database.clear()?;
            println!("{}", format!("Cleared {cleared} entries from database").green());
            Ok(())
        }
        DatabaseMode::Show => show_database_contents(args),
        DatabaseMode::ListExtensions => list_extensions(args.verbose),
        DatabaseMode::Process => VideoConvert::new(args)?.run_from_database(),
    }
}

/// List file extension counts in the database.
fn list_extensions(verbose: bool) -> Result<()> {
    let database = Database::open_default()?;
    if verbose {
        println!("{}", format!("Database: {}", Database::path().display()).bold());
        println!();
    }

    let ext_stats = database.get_extension_stats()?;
    if ext_stats.is_empty() {
        println!("No files in database");
    } else {
        // Calculate column widths for right-alignment
        let max_count_width = ext_stats.iter().map(|e| e.count.to_string().len()).max().unwrap_or(1);
        let max_size_width = ext_stats
            .iter()
            .map(|e| cli_tools::format_size(e.total_size).len())
            .max()
            .unwrap_or(1);

        for ext in &ext_stats {
            let size_str = cli_tools::format_size(ext.total_size);
            println!(
                ".{:<4} {:>max_count_width$} files  {:>max_size_width$}",
                ext.extension, ext.count, size_str
            );
        }
        println!();
        let total_files: u64 = ext_stats.iter().map(|e| e.count).sum();
        let total_size: u64 = ext_stats.iter().map(|e| e.total_size).sum();
        println!(
            "{:<5} {:>max_count_width$} files  {:>max_size_width$}",
            "Total",
            total_files,
            cli_tools::format_size(total_size)
        );
    }
    Ok(())
}

/// Show database statistics and contents.
fn show_database_contents(args: VideoConvertArgs) -> Result<()> {
    let verbose = args.verbose;
    let user_config = VideoConvertConfig::get_user_config();
    let config = Config::try_from_args(args, user_config)?;
    let database = Database::open_default()?;
    if verbose {
        println!("{}", format!("Database: {}", Database::path().display()).bold());
        println!();
    }

    let stats = database.get_stats()?;
    println!("{stats}");

    if stats.total_files > 0 {
        // Show extension statistics
        let ext_stats = database.get_extension_stats()?;
        if !ext_stats.is_empty() {
            println!();
            println!("{}", "File extensions:".bold());

            // Calculate column widths for right-alignment
            let max_count_width = ext_stats.iter().map(|e| e.count.to_string().len()).max().unwrap_or(1);

            for ext in &ext_stats {
                println!(
                    "  .{}: {:>max_count_width$} files ({})",
                    ext.extension,
                    ext.count,
                    cli_tools::format_size(ext.total_size)
                );
            }
        }

        println!();
        println!("{}", "Pending files:".bold());

        let files = database.get_pending_files(&config.db_filter)?;
        let total_matching = files.len();
        let display_count = config
            .display_limit
            .map_or(total_matching, |limit| total_matching.min(limit));

        for file in files.iter().take(display_count) {
            // Use fixed widths for consistent alignment:
            // size: 9 chars (e.g. "19.20 GB "), bitrate: 10 chars (e.g. "15.0 Mbps "), duration: 10 chars (e.g. "3h 01m 18s")
            let size_str = cli_tools::format_size(file.size_bytes);
            let bitrate_str = format!("{:.1} Mbps", file.bitrate_kbps as f64 / 1000.0);
            let duration_str = cli_tools::format_duration(std::time::Duration::from_secs_f64(file.duration));
            let action_str = match file.action {
                database::PendingAction::Convert => "CONVERT".yellow(),
                database::PendingAction::Remux => "REMUX".cyan(),
            };
            println!(
                "  [{action_str}] {:<5} {:>10} {:>10} {:>10} - {}",
                file.extension,
                size_str,
                bitrate_str,
                duration_str,
                file.full_path.display()
            );
        }

        if files.is_empty() {
            println!("  (no files match the current filter)");
        } else {
            println!();
            let remaining = total_matching - display_count;
            if remaining > 0 {
                println!("Showing {display_count} of {total_matching} matching files ({remaining} more)...");
            } else {
                println!("Showing {total_matching} of {} files", stats.total_files);
            }
        }
    }
    Ok(())
}
