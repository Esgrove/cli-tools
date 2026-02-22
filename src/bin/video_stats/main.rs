mod collector;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::collector::StatsCollector;

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Collect and print video file statistics")]
pub(crate) struct VideoStatsArgs {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Recurse into subdirectories
    #[arg(short = 'r', long)]
    recurse: bool,

    /// Print verbose per-file output
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,
}

fn main() -> Result<()> {
    let args = VideoStatsArgs::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, VideoStatsArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        StatsCollector::new(&args)?.run()
    }
}
