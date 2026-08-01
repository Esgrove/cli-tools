//! Video statistics command-line entrypoint.
//!
//! Parses input paths and display options, then delegates collection to `StatsCollector`.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod collector;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use crate::collector::StatsCollector;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Collect and print video file statistics")]
pub(crate) struct VideoStatsArgs {
    #[command(subcommand)]
    command: Option<VideoStatsCommand>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Print verbose per-file output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

/// Subcommands for vstats.
#[derive(Subcommand)]
enum VideoStatsCommand {
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
    let args = VideoStatsArgs::parse();
    if let Some(VideoStatsCommand::Completion { shell, install }) = &args.command {
        cli_tools::generate_shell_completion(
            *shell,
            VideoStatsArgs::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        )
    } else {
        StatsCollector::new(&args)?.run()
    }
}

#[cfg(test)]
mod test_video_stats_args {
    use super::*;

    #[test]
    fn parses_defaults_and_operational_options() {
        let defaults = VideoStatsArgs::try_parse_from(["vstats"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.path.is_none());
        assert!(!defaults.recurse);
        assert!(!defaults.verbose);

        let configured = VideoStatsArgs::try_parse_from(["vstats", "videos", "--recurse", "--verbose"])
            .expect("operational arguments should parse");
        assert_eq!(configured.path, Some(PathBuf::from("videos")));
        assert!(configured.recurse);
        assert!(configured.verbose);
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = VideoStatsArgs::try_parse_from(["vstats", "completion", "bash", "--install"])
            .expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(VideoStatsCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
        VideoStatsArgs::command().debug_assert();
    }
}
