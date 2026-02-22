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
    #[arg(short = 'v', long)]
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
        cli_tools::generate_shell_completion(*shell, VideoStatsArgs::command(), *install, env!("CARGO_BIN_NAME"))
    } else {
        StatsCollector::new(&args)?.run()
    }
}
