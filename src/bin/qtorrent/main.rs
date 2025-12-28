//! qtorrent - Add torrents to qBittorrent with automatic file renaming.
//!
//! This CLI tool parses `.torrent` files and adds them to qBittorrent via the `WebUI` API,
//! automatically renaming the output file based on the torrent filename.

mod add;
mod config;
mod qbittorrent;
mod torrent;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;

use crate::add::QTorrent;

/// Add torrents to qBittorrent with automatic file renaming.
///
/// Parses `.torrent` files and adds them to qBittorrent,
/// automatically setting the output filename or folder name based on the torrent filename.
/// For multi-file torrents,
/// offers to rename the root folder and supports filtering files by extension, name, or minimum size.
#[derive(Parser)]
#[command(
    author,
    version,
    name = env!("CARGO_BIN_NAME"),
    about = "Add torrents to qBittorrent with automatic file renaming"
)]
pub struct QtorrentArgs {
    /// Optional input path(s) with torrent files or directories
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    pub path: Vec<PathBuf>,

    /// qBittorrent `WebUI` host
    #[arg(short = 'H', long, name = "HOST")]
    host: Option<String>,

    /// qBittorrent `WebUI` port
    #[arg(short = 'P', long, name = "PORT")]
    port: Option<u16>,

    /// qBittorrent `WebUI` username
    #[arg(short = 'u', long, name = "USER")]
    username: Option<String>,

    /// qBittorrent `WebUI` password
    #[arg(short = 'w', long, name = "PASS")]
    password: Option<String>,

    /// Save path for downloaded files
    #[arg(short = 's', long, name = "PATH")]
    save_path: Option<String>,

    /// Category for the torrent
    #[arg(short = 'c', long, name = "CATEGORY")]
    category: Option<String>,

    /// Tags for the torrent (comma-separated)
    #[arg(short = 't', long, name = "TAGS")]
    tags: Option<String>,

    /// Add torrent in paused state
    #[arg(short = 'a', long)]
    paused: bool,

    /// Print what would be done without actually adding torrents
    #[arg(short = 'p', long)]
    dryrun: bool,

    /// Skip confirmation prompts
    #[arg(short = 'y', long)]
    yes: bool,

    /// File extensions to skip (e.g., nfo, txt, jpg)
    #[arg(short = 'e', long = "skip-ext", name = "EXT", value_delimiter = ',')]
    skip_extensions: Vec<String>,

    /// File or folder names to skip (case-insensitive partial match)
    #[arg(short = 'k', long = "skip-name", name = "NAME", value_delimiter = ',')]
    skip_names: Vec<String>,

    /// Minimum file size in MB (files smaller than this will be skipped)
    #[arg(short = 'm', long = "min-size", name = "MB")]
    min_file_size_mb: Option<f64>,

    /// Recurse into subdirectories when searching for torrent files
    #[arg(short = 'r', long)]
    pub recurse: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Print verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = QtorrentArgs::parse();

    // Handle shell completion generation
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, QtorrentArgs::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        QTorrent::new(args).run().await
    }
}
