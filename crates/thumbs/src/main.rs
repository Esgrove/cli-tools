//! Thumbnail sheet command-line entrypoint.
//!
//! Parses grid and rendering options, installs a Ctrl+C handler,
//! and delegates work to `ThumbnailCreator`.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod config;
mod thumbnail;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};
use cli_tools_common::CompletionArgs;
use colored::Colorize;

use crate::thumbnail::ThumbnailCreator;

/// Subcommands for thumbs.
#[derive(Subcommand)]
enum ThumbnailCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),
}

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

    /// Custom font file path for timestamp and header overlays
    #[arg(short = 'F', long, name = "FONT")]
    font: Option<PathBuf>,

    /// Number of concurrent thumbnail jobs
    #[arg(short = 'j', long, name = "JOBS")]
    jobs: Option<usize>,

    /// Skip the shared scan cache and re-probe every file
    #[arg(short = 'C', long)]
    no_cache: bool,

    /// JPEG quality (1-31, lower is better)
    #[arg(short = 'q', long, name = "QUALITY")]
    quality: Option<u32>,

    /// Print verbose output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = ThumbnailArgs::parse();
    if let Some(ThumbnailCommand::Completion(completion)) = &args.command {
        completion.generate::<ThumbnailArgs>(args.verbose, env!("CARGO_BIN_NAME"))
    } else {
        let abort_flag = Arc::new(AtomicBool::new(false));
        let handler_flag = Arc::clone(&abort_flag);
        let _ = ctrlc::set_handler(move || {
            println!("\n{}", "Received Ctrl+C, stopping after active jobs...".yellow().bold());
            handler_flag.store(true, Ordering::SeqCst);
        });
        ThumbnailCreator::new(&args)?.run_with_abort(&abort_flag)
    }
}

#[cfg(test)]
mod test_thumbnail_args {
    use super::*;
    use clap::CommandFactory;
    use clap_complete::Shell;

    #[test]
    fn parses_defaults_and_rendering_options() {
        let defaults = ThumbnailArgs::try_parse_from(["thumbs"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.path.is_none());
        assert!(!defaults.force);
        assert!(!defaults.print);
        assert!(!defaults.recurse);
        assert!(!defaults.no_cache);
        assert!(!defaults.verbose);
        assert!(defaults.cols.is_none());
        assert!(defaults.rows.is_none());
        assert!(defaults.scale.is_none());
        assert!(defaults.padding.is_none());
        assert!(defaults.fontsize.is_none());
        assert!(defaults.font.is_none());
        assert!(defaults.jobs.is_none());
        assert!(defaults.quality.is_none());

        let configured = ThumbnailArgs::try_parse_from([
            "thumbs",
            "video.mp4",
            "-f",
            "-p",
            "-r",
            "-C",
            "-v",
            "-c",
            "5",
            "-w",
            "6",
            "-s",
            "640",
            "-a",
            "12",
            "-t",
            "24",
            "-j",
            "4",
            "-q",
            "3",
        ])
        .expect("rendering options should parse");
        assert_eq!(configured.path, Some(PathBuf::from("video.mp4")));
        assert!(configured.force);
        assert!(configured.print);
        assert!(configured.recurse);
        assert!(configured.no_cache);
        assert!(configured.verbose);
        assert_eq!(configured.cols, Some(5));
        assert_eq!(configured.rows, Some(6));
        assert_eq!(configured.scale, Some(640));
        assert_eq!(configured.padding, Some(12));
        assert_eq!(configured.fontsize, Some(24));
        assert_eq!(configured.jobs, Some(4));
        assert_eq!(configured.quality, Some(3));
    }

    #[test]
    fn rejects_invalid_numeric_values() {
        assert!(ThumbnailArgs::try_parse_from(["thumbs", "--cols", "not-a-number"]).is_err());
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = ThumbnailArgs::try_parse_from(["thumbs", "completion", "bash", "--install"])
            .expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(ThumbnailCommand::Completion(CompletionArgs {
                shell: Shell::Bash,
                install: true
            }))
        ));
        ThumbnailArgs::command().debug_assert();
    }
}
