//! Video statistics command-line entrypoint.
//!
//! Parses input paths and display options, combines them with the user config,
//! then delegates collection to `StatsCollector`.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod collector;
mod config;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use cli_tools_common::CompletionArgs;

use crate::collector::StatsCollector;
use crate::config::Config;

/// Subcommands for vstats.
#[derive(Subcommand)]
enum VideoStatsCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),
}

#[derive(Parser)]
#[command(author, version = cli_tools_version::VERSION_INFO, name = env!("CARGO_BIN_NAME"), about = "Collect and print video file statistics")]
pub(crate) struct VideoStatsArgs {
    #[command(subcommand)]
    command: Option<VideoStatsCommand>,

    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Skip the shared scan cache and re-probe every file
    #[arg(short = 'C', long)]
    no_cache: bool,

    /// Print verbose per-file output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = VideoStatsArgs::parse();
    if let Some(VideoStatsCommand::Completion(completion)) = &args.command {
        completion.generate::<VideoStatsArgs>(args.verbose, env!("CARGO_BIN_NAME"))
    } else {
        StatsCollector::new(Config::from_args(&args)?).run()
    }
}

#[cfg(test)]
mod test_video_stats_args {
    use super::*;
    use clap::CommandFactory;
    use clap_complete::Shell;

    #[test]
    fn parses_defaults_and_operational_options() {
        let defaults = VideoStatsArgs::try_parse_from(["vstats"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.path.is_none());
        assert!(!defaults.recurse);
        assert!(!defaults.no_cache);
        assert!(!defaults.verbose);

        let configured = VideoStatsArgs::try_parse_from(["vstats", "videos", "--recurse", "--no-cache", "--verbose"])
            .expect("operational arguments should parse");
        assert_eq!(configured.path, Some(PathBuf::from("videos")));
        assert!(configured.recurse);
        assert!(configured.no_cache);
        assert!(configured.verbose);
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args = VideoStatsArgs::try_parse_from(["vstats", "completion", "bash", "--install"])
            .expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(VideoStatsCommand::Completion(CompletionArgs {
                shell: Shell::Bash,
                install: true
            }))
        ));
        VideoStatsArgs::command().debug_assert();
    }
}
