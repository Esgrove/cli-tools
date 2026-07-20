//! Command-line entry point for the video conversion tool.
//!
//! Defines CLI arguments and subcommands, initializes logging, and dispatches conversion or completion generation.

pub(crate) mod classification;
mod cli;
mod config;
mod convert;
mod database;
mod ffmpeg;
mod helpers;
mod logger;
mod stats;
mod types;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

pub use crate::cli::{DatabaseMode, SortOrder};
use crate::convert::VideoConvert;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
pub(crate) struct VideoConvertArgs {
    #[command(subcommand)]
    command: Option<VideoConvertCommand>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types
    #[arg(short = 'a', long)]
    all: bool,

    /// Skip files with bitrate lower than LIMIT kbps
    #[arg(short = 'b', long, name = "BITRATE", default_value_t = 8000)]
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

    /// Delete source file if converted x265 file already exists
    #[arg(short = 'x', long)]
    delete_duplicates: bool,

    /// Movie mode: preserve MKV container, metadata, and selected stream languages
    #[arg(short = 'm', long)]
    movie: bool,

    /// Skip remuxing
    #[arg(short = 'M', long)]
    skip_remux: bool,

    /// Sort files
    #[arg(short = 's', long, name = "ORDER", num_args = 0..=1, default_missing_value = "bitrate")]
    sort: Option<SortOrder>,

    /// Print verbose output
    #[arg(short = 'v', long, global = true)]
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

    /// Remove stale entries from the scan cache
    #[arg(short = 'X', long = "clean-cache", group = "db_mode")]
    clean_cache: bool,

    /// Maximum bitrate in kbps
    #[arg(short = 'B', long = "max-bitrate", name = "MAX_BITRATE")]
    max_bitrate: Option<u64>,

    /// Minimum duration in seconds
    #[arg(short = 'u', long = "min-duration", name = "MIN_DURATION")]
    min_duration: Option<f64>,

    /// Maximum duration in seconds
    #[arg(short = 'U', long = "max-duration", name = "MAX_DURATION")]
    max_duration: Option<f64>,

    /// Skip files where either width or height is smaller than PIXELS
    #[arg(short = 'R', long = "min-resolution", name = "PIXELS")]
    min_resolution: Option<u32>,

    /// Maximum number of files to display
    #[arg(short = 'L', long = "display-limit", name = "LIMIT")]
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
        } else if self.clean_cache {
            Some(DatabaseMode::CleanScanCache)
        } else {
            None
        }
    }
}

/// Subcommands for vconvert.
#[derive(Subcommand)]
enum VideoConvertCommand {
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
    let args = VideoConvertArgs::parse();
    if let Some(VideoConvertCommand::Completion { shell, install }) = &args.command {
        cli_tools::generate_shell_completion(
            *shell,
            VideoConvertArgs::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        )
    } else {
        VideoConvert::new(args)?.run()
    }
}

#[cfg(test)]
mod test_video_convert_args_parsing {
    use super::*;

    #[test]
    fn uses_expected_defaults() {
        let args = VideoConvertArgs::try_parse_from(["vconvert"]).expect("Failed to parse default arguments");

        assert!(args.command.is_none());
        assert!(args.path.is_none());
        assert_eq!(args.bitrate, 8000);
        assert!(args.count.is_none());
        assert!(args.include.is_empty());
        assert!(args.exclude.is_empty());
        assert!(args.extension.is_empty());
        assert!(args.sort.is_none());
        assert!(!args.verbose);
        assert_eq!(args.database_mode(), None);
    }

    #[test]
    fn parses_paths_filters_and_limits() {
        let args = VideoConvertArgs::try_parse_from([
            "vconvert",
            "movies",
            "--bitrate",
            "9000",
            "--count",
            "4",
            "--include",
            "Director",
            "--include",
            "Extended",
            "--exclude",
            "Sample",
            "--extension",
            "MKV",
            "--extension",
            "MP4",
            "--max-bitrate",
            "20000",
            "--min-duration",
            "60.5",
            "--max-duration",
            "7200",
            "--min-resolution",
            "720",
            "--display-limit",
            "25",
            "--recurse",
            "--movie",
            "--verbose",
        ])
        .expect("Failed to parse filtering arguments");

        assert_eq!(args.path, Some(PathBuf::from("movies")));
        assert_eq!(args.bitrate, 9000);
        assert_eq!(args.count, Some(4));
        assert_eq!(args.include, ["Director", "Extended"]);
        assert_eq!(args.exclude, ["Sample"]);
        assert_eq!(args.extension, ["MKV", "MP4"]);
        assert_eq!(args.max_bitrate, Some(20_000));
        assert_eq!(args.min_duration, Some(60.5));
        assert_eq!(args.max_duration, Some(7200.0));
        assert_eq!(args.min_resolution, Some(720));
        assert_eq!(args.display_limit, Some(25));
        assert!(args.recurse);
        assert!(args.movie);
        assert!(args.verbose);
    }

    #[test]
    fn sort_flag_uses_bitrate_when_value_is_omitted() {
        let args = VideoConvertArgs::try_parse_from(["vconvert", "--sort"])
            .expect("Failed to parse sort flag without a value");

        assert_eq!(args.sort, Some(SortOrder::Bitrate));
    }

    #[test]
    fn parses_explicit_sort_order() {
        let args = VideoConvertArgs::try_parse_from(["vconvert", "--sort", "duration-asc"])
            .expect("Failed to parse explicit sort order");

        assert_eq!(args.sort, Some(SortOrder::DurationAsc));
    }

    #[test]
    fn rejects_conflicting_extension_modes() {
        let result = VideoConvertArgs::try_parse_from(["vconvert", "--all", "--other"]);

        assert!(result.is_err());
    }
}

#[cfg(test)]
mod test_database_mode_selection {
    use super::*;

    #[test]
    fn maps_each_database_flag_to_its_mode() {
        let cases = [
            ("--from-db", DatabaseMode::Process),
            ("--clear-db", DatabaseMode::Clear),
            ("--show-db", DatabaseMode::Show),
            ("--list-extensions", DatabaseMode::ListExtensions),
            ("--clean-cache", DatabaseMode::CleanScanCache),
        ];

        for (flag, expected_mode) in cases {
            let args =
                VideoConvertArgs::try_parse_from(["vconvert", flag]).expect("Failed to parse database mode argument");
            assert_eq!(args.database_mode(), Some(expected_mode));
        }
    }

    #[test]
    fn rejects_multiple_database_modes() {
        let result = VideoConvertArgs::try_parse_from(["vconvert", "--from-db", "--show-db"]);

        assert!(result.is_err());
    }
}

#[cfg(test)]
mod test_completion_command_parsing {
    use super::*;

    #[test]
    fn parses_shell_and_install_option() {
        let args = VideoConvertArgs::try_parse_from(["vconvert", "--verbose", "completion", "bash", "--install"])
            .expect("Failed to parse completion command");

        assert!(args.verbose);
        assert!(matches!(
            args.command,
            Some(VideoConvertCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
    }

    #[test]
    fn command_definition_is_valid() {
        VideoConvertArgs::command().debug_assert();
    }
}
