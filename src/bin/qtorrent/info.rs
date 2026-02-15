//! Info subcommand module.
//!
//! Connects to qBittorrent and displays statistics about existing torrents,
//! including total count, sizes, and completion status.

use std::collections::HashMap;

use anyhow::{Result, bail};
use colored::Colorize;

use crate::QtorrentArgs;
use crate::config::Config;
use crate::qbittorrent::{QBittorrentClient, TorrentListItem};

/// Run the info subcommand.
///
/// Connects to qBittorrent, fetches the torrent list, and prints statistics.
///
/// # Errors
/// Returns an error if connection fails or credentials are missing.
#[allow(clippy::similar_names)]
pub async fn run(args: QtorrentArgs) -> Result<()> {
    let config = Config::from_args(args)?;

    if !config.has_credentials() {
        bail!(
            "qBittorrent credentials not configured.\n\
             Set username and password via command line arguments or in config file:\n\
             ~/.config/cli-tools.toml under [qtorrent] section"
        );
    }

    cli_tools::print_cyan("Connecting to qBittorrent...");
    let mut client = QBittorrentClient::new(&config.host, config.port);
    client.login(&config.username, &config.password).await?;

    let app_version = client.get_app_version().await?;
    let api_version = client.get_api_version().await?;
    println!(
        "{} (App {app_version}, API v{api_version})\n",
        "Connected successfully".green()
    );

    let torrents = client.get_torrent_list().await?;
    print_statistics(&torrents, config.verbose);

    if let Err(error) = client.logout().await {
        cli_tools::print_yellow!("Failed to logout: {error}");
    }

    Ok(())
}

/// Torrent categorized by download status.
enum TorrentStatus {
    /// Download is complete.
    Completed,
    /// Download is in progress (0 < progress < 1).
    Downloading,
    /// Download has not started (progress == 0).
    NotStarted,
}

/// Classify a torrent by its download status.
fn classify_torrent(torrent: &TorrentListItem) -> TorrentStatus {
    if torrent.is_completed() {
        TorrentStatus::Completed
    } else if torrent.progress > 0.0 {
        TorrentStatus::Downloading
    } else {
        TorrentStatus::NotStarted
    }
}

/// Print torrent statistics summary and optionally individual torrent details.
fn print_statistics(torrents: &HashMap<String, TorrentListItem>, verbose: bool) {
    if torrents.is_empty() {
        println!("{}", "No torrents found.".dimmed());
        return;
    }

    let mut completed: Vec<&TorrentListItem> = Vec::new();
    let mut downloading: Vec<&TorrentListItem> = Vec::new();
    let mut not_started: Vec<&TorrentListItem> = Vec::new();

    let mut total_size: u64 = 0;
    let mut completed_size: u64 = 0;
    let mut downloading_size: u64 = 0;
    let mut not_started_size: u64 = 0;

    for torrent in torrents.values() {
        let size = torrent.size.max(0) as u64;
        total_size += size;

        match classify_torrent(torrent) {
            TorrentStatus::Completed => {
                completed_size += size;
                completed.push(torrent);
            }
            TorrentStatus::Downloading => {
                downloading_size += size;
                downloading.push(torrent);
            }
            TorrentStatus::NotStarted => {
                not_started_size += size;
                not_started.push(torrent);
            }
        }
    }

    // Sort each category by name
    completed.sort_unstable_by(|first, second| first.name.to_lowercase().cmp(&second.name.to_lowercase()));
    downloading.sort_unstable_by(|first, second| first.name.to_lowercase().cmp(&second.name.to_lowercase()));
    not_started.sort_unstable_by(|first, second| first.name.to_lowercase().cmp(&second.name.to_lowercase()));

    // Print summary
    print_stat_line("Total torrents", torrents.len(), total_size);
    print_stat_line("Completed", completed.len(), completed_size);

    if !downloading.is_empty() {
        print_stat_line("Downloading", downloading.len(), downloading_size);
    }

    if !not_started.is_empty() {
        print_stat_line("Not started", not_started.len(), not_started_size);
    }

    // Print individual torrent details in verbose mode
    if verbose {
        if !completed.is_empty() {
            println!("\n{}", format!("Completed ({}):", completed.len()).green());
            for torrent in &completed {
                print_torrent_detail(torrent);
            }
        }

        let incomplete_count = downloading.len() + not_started.len();
        if incomplete_count > 0 {
            println!("\n{}", format!("Incomplete ({incomplete_count}):").yellow());
            for torrent in &downloading {
                print_torrent_detail(torrent);
            }
            for torrent in &not_started {
                print_torrent_detail(torrent);
            }
        }
    }
}

/// Print a single summary stat line.
fn print_stat_line(label: &str, count: usize, size: u64) {
    println!(
        "{:<20} {:>5} {:>10}",
        label,
        count.to_string().bold(),
        cli_tools::format_size(size).dimmed(),
    );
}

/// Print details for a single torrent.
fn print_torrent_detail(torrent: &TorrentListItem) {
    let size = cli_tools::format_size(torrent.size.max(0) as u64);
    let progress_percent = torrent.progress * 100.0;

    let progress_str = if torrent.is_completed() {
        format!("{progress_percent:.0}%").green()
    } else if torrent.progress > 0.0 {
        format!("{progress_percent:.1}%").yellow()
    } else {
        format!("{progress_percent:.0}%").red()
    };

    println!("  {}", torrent.name.bold());
    println!("    {:<14} {:>6}  {:<14} {}", "Progress:", progress_str, "Size:", size,);
    println!(
        "    {:<14} {:<20}  {:<14} {}",
        "Ratio:",
        format!("{:.2}", torrent.ratio),
        "Save path:",
        torrent.save_path.dimmed(),
    );
    println!(
        "    {:<14} {}",
        "Added:",
        cli_tools::format_timestamp(torrent.added_on).dimmed(),
    );

    if let Some(completed_on) = torrent.completion_on {
        println!(
            "    {:<14} {}",
            "Completed:",
            cli_tools::format_timestamp(completed_on).dimmed(),
        );
    }

    if !torrent.tags.is_empty() {
        println!("    {:<14} {}", "Tags:", torrent.tags.cyan());
    }
}
