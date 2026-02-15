//! Info subcommand module.
//!
//! Connects to qBittorrent and displays statistics about existing torrents,
//! including total count, sizes, and completion status.
//! Supports sorting by name, size, or save path, and a compact list mode.

use std::collections::HashMap;

use anyhow::{Result, bail};
use colored::Colorize;

use crate::QtorrentArgs;
use crate::SortOrder;
use crate::config::Config;
use crate::qbittorrent::{QBittorrentClient, TorrentListItem};

/// Options controlling how torrent info is printed.
struct PrintOptions {
    /// Sort order for torrent listing.
    sort: SortOrder,
    /// Show torrents on one line.
    list: bool,
    /// Show additional detail.
    verbose: bool,
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

/// Run the info subcommand.
///
/// Connects to qBittorrent, fetches the torrent list, and prints statistics.
///
/// # Errors
/// Returns an error if connection fails or credentials are missing.
#[allow(clippy::similar_names)]
pub async fn run(args: QtorrentArgs, sort: SortOrder, list: bool) -> Result<()> {
    let config = Config::from_args(args)?;
    let options = PrintOptions {
        sort,
        list,
        verbose: config.verbose,
    };

    if !config.has_credentials() {
        bail!(
            "qBittorrent credentials not configured.\n\
             Set username and password via command line arguments or in config file:\n\
             ~/.config/cli-tools.toml under [qtorrent] section"
        );
    }

    if options.verbose {
        cli_tools::print_cyan("Connecting to qBittorrent...");
    }

    let mut client = QBittorrentClient::new(&config.host, config.port);
    client.login(&config.username, &config.password).await?;

    let app_version = client.get_app_version().await?;
    let api_version = client.get_api_version().await?;

    if options.verbose {
        println!(
            "{} (App {app_version}, API v{api_version})\n",
            "Connected successfully".green()
        );
    }

    let torrents = client.get_torrent_list().await?;

    print_statistics(&torrents, &options);

    if let Err(error) = client.logout().await {
        cli_tools::print_yellow!("Failed to logout: {error}");
    }

    Ok(())
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

/// Sort a slice of torrent references by the given sort order.
fn sort_torrents(torrents: &mut [&TorrentListItem], sort: SortOrder) {
    match sort {
        SortOrder::Name => {
            torrents.sort_unstable_by(|first, second| first.name.to_lowercase().cmp(&second.name.to_lowercase()));
        }
        SortOrder::Size => {
            torrents.sort_unstable_by(|first, second| {
                second
                    .size
                    .cmp(&first.size)
                    .then_with(|| first.name.to_lowercase().cmp(&second.name.to_lowercase()))
            });
        }
        SortOrder::Path => {
            torrents.sort_unstable_by(|first, second| {
                first
                    .save_path
                    .to_lowercase()
                    .cmp(&second.save_path.to_lowercase())
                    .then_with(|| first.name.to_lowercase().cmp(&second.name.to_lowercase()))
            });
        }
    }
}

/// Print torrent statistics summary and optionally individual torrent details.
fn print_statistics(torrents: &HashMap<String, TorrentListItem>, options: &PrintOptions) {
    if torrents.is_empty() {
        println!("{}", "No torrents found".dimmed());
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

    // Sort each category
    sort_torrents(&mut completed, options.sort);
    sort_torrents(&mut downloading, options.sort);
    sort_torrents(&mut not_started, options.sort);

    // Print summary
    print_stat_line("Total torrents", torrents.len(), total_size);
    print_stat_line("Completed", completed.len(), completed_size);

    if !downloading.is_empty() {
        print_stat_line("Downloading", downloading.len(), downloading_size);
    }

    if !not_started.is_empty() {
        print_stat_line("Not started", not_started.len(), not_started_size);
    }

    // Print individual torrent details in list or verbose mode
    if options.list {
        print_torrent_sections_list(&completed, &downloading, &not_started, options.verbose);
    } else if options.verbose {
        print_torrent_sections_verbose(&completed, &downloading, &not_started);
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

/// Print completed and incomplete sections in list mode (one line per torrent).
fn print_torrent_sections_list(
    completed: &[&TorrentListItem],
    downloading: &[&TorrentListItem],
    not_started: &[&TorrentListItem],
    verbose: bool,
) {
    if !completed.is_empty() {
        for torrent in completed {
            print_torrent_list_line(torrent, verbose);
        }
    }

    let incomplete_count = downloading.len() + not_started.len();
    if incomplete_count > 0 {
        for torrent in downloading {
            print_torrent_list_line(torrent, verbose);
        }
        for torrent in not_started {
            print_torrent_list_line(torrent, verbose);
        }
    }
}

/// Print completed and incomplete sections in verbose mode (multi-line per torrent).
fn print_torrent_sections_verbose(
    completed: &[&TorrentListItem],
    downloading: &[&TorrentListItem],
    not_started: &[&TorrentListItem],
) {
    if !completed.is_empty() {
        println!("\n{}", format!("Completed ({}):", completed.len()).green());
        for torrent in completed {
            print_torrent_detail(torrent);
        }
    }

    let incomplete_count = downloading.len() + not_started.len();
    if incomplete_count > 0 {
        println!("\n{}", format!("Incomplete ({incomplete_count}):").yellow());
        for torrent in downloading {
            print_torrent_detail(torrent);
        }
        for torrent in not_started {
            print_torrent_detail(torrent);
        }
    }
}

/// Format progress percentage with color coding.
///
/// The result is right-padded to a fixed width before colorizing,
/// so ANSI escape codes don't interfere with column alignment.
fn format_progress(torrent: &TorrentListItem) -> String {
    let progress_percent = torrent.progress * 100.0;

    if torrent.is_completed() {
        format!("{:>5}", format!("{progress_percent:.0}%")).green().to_string()
    } else if torrent.progress > 0.0 {
        format!("{:>5}", format!("{progress_percent:.0}%")).yellow().to_string()
    } else {
        format!("{:>5}", format!("{progress_percent:.0}%")).red().to_string()
    }
}

/// Print a single torrent as one compact line.
///
/// When verbose is enabled, additional columns are shown: ratio, added date, and completed date.
fn print_torrent_list_line(torrent: &TorrentListItem, verbose: bool) {
    let size = cli_tools::format_size(torrent.size.max(0) as u64);
    let progress = format_progress(torrent);

    let tags_str = if torrent.tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", torrent.tags)
    };

    if verbose {
        let ratio = format!("{:.2}", torrent.ratio);
        let added = cli_tools::format_timestamp(torrent.added_on);
        let completed = torrent
            .completion_on
            .map_or_else(|| "-".to_string(), cli_tools::format_timestamp);

        println!(
            "{progress}  {size:>10}  {:<16}  {ratio:>6}  {}  {}  {}{}",
            torrent.save_path.dimmed(),
            added.dimmed(),
            completed.dimmed(),
            torrent.name,
            tags_str.dimmed(),
        );
    } else {
        println!(
            "{progress}  {size:>10}  {:<16}  {}{}",
            torrent.save_path.dimmed(),
            torrent.name,
            tags_str.dimmed(),
        );
    }
}

/// Print full details for a single torrent (multi-line format).
fn print_torrent_detail(torrent: &TorrentListItem) {
    let size = cli_tools::format_size(torrent.size.max(0) as u64);
    let progress = format_progress(torrent);

    println!("  {}", torrent.name.bold());
    println!("    {:<14} {:>6}  {:<14} {}", "Progress:", progress, "Size:", size);
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
