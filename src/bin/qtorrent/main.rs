//! qtorrent - Add torrents to qBittorrent with automatic file renaming.
//!
//! This CLI tool parses `.torrent` files and adds them to qBittorrent
//! via the `WebUI` API, automatically renaming the output file based on
//! the torrent filename.

mod add;
mod config;
mod qbittorrent;
mod torrent;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::add::TorrentAdder;
use crate::config::{Config, QtorrentConfig};

/// Add torrents to qBittorrent with automatic file renaming.
///
/// Parses single-file `.torrent` files and adds them to qBittorrent,
/// automatically setting the output filename based on the torrent filename.
/// Multi-file torrents are skipped.
#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Add torrents to qBittorrent with automatic file renaming"
)]
pub struct QtorrentArgs {
    /// Torrent file(s) to add
    #[arg(value_hint = clap::ValueHint::FilePath)]
    torrents: Vec<PathBuf>,

    /// qBittorrent `WebUI` host
    #[arg(short = 'H', long, name = "HOST")]
    host: Option<String>,

    /// qBittorrent `WebUI` port
    #[arg(short = 'P', long, name = "PORT")]
    port: Option<u16>,

    /// qBittorrent `WebUI` username
    #[arg(short, long, name = "USER")]
    username: Option<String>,

    /// qBittorrent `WebUI` password
    #[arg(short = 'w', long, name = "PASS")]
    password: Option<String>,

    /// Save path for downloaded files
    #[arg(short, long, name = "PATH")]
    save_path: Option<String>,

    /// Category for the torrent
    #[arg(short, long, name = "CATEGORY")]
    category: Option<String>,

    /// Tags for the torrent (comma-separated)
    #[arg(short, long, name = "TAGS")]
    tags: Option<String>,

    /// Add torrent in paused state
    #[arg(short = 'a', long)]
    paused: bool,

    /// Print what would be done without actually adding torrents
    #[arg(short = 'n', long)]
    dryrun: bool,

    /// Skip confirmation prompts
    #[arg(short, long)]
    yes: bool,

    /// Print verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = QtorrentArgs::parse();

    // Handle shell completion generation
    if let Some(ref shell) = args.completion {
        return cli_tools::generate_shell_completion(*shell, QtorrentArgs::command(), true, env!("CARGO_BIN_NAME"));
    }

    // Load user configuration
    let user_config = QtorrentConfig::get_user_config();

    // Merge CLI args with user config
    let config = Config::try_from_args(args, user_config)?;

    // Run the main logic
    TorrentAdder::new(config).run().await
}
