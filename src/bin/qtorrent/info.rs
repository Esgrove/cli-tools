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
            torrents.sort_unstable_by_key(|torrent| torrent.name.to_lowercase());
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
    for line in statistics_lines(torrents, options) {
        println!("{line}");
    }
}

/// Lines of the torrent statistics report.
///
/// The lines are built rather than printed directly so they can be checked in a test.
fn statistics_lines(torrents: &HashMap<String, TorrentListItem>, options: &PrintOptions) -> Vec<String> {
    if torrents.is_empty() {
        return vec!["No torrents found".dimmed().to_string()];
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

    let mut lines = vec![
        stat_line("Total torrents", torrents.len(), total_size),
        stat_line("Completed", completed.len(), completed_size),
    ];

    if !downloading.is_empty() {
        lines.push(stat_line("Downloading", downloading.len(), downloading_size));
    }

    if !not_started.is_empty() {
        lines.push(stat_line("Not started", not_started.len(), not_started_size));
    }

    // Individual torrent details in list or verbose mode
    if options.list {
        lines.extend(torrent_section_list_lines(
            &completed,
            &downloading,
            &not_started,
            options.verbose,
        ));
    } else if options.verbose {
        lines.extend(torrent_section_detail_lines(&completed, &downloading, &not_started));
    }
    lines
}

/// One summary stat line.
fn stat_line(label: &str, count: usize, size: u64) -> String {
    format!(
        "{:<20} {:>5} {:>10}",
        label,
        count.to_string().bold(),
        cli_tools::format_size(size).dimmed(),
    )
}

/// Completed and incomplete sections in list mode, one line per torrent.
fn torrent_section_list_lines(
    completed: &[&TorrentListItem],
    downloading: &[&TorrentListItem],
    not_started: &[&TorrentListItem],
    verbose: bool,
) -> Vec<String> {
    completed
        .iter()
        .chain(downloading)
        .chain(not_started)
        .map(|torrent| torrent_list_line(torrent, verbose))
        .collect()
}

/// Completed and incomplete sections in verbose mode, several lines per torrent.
fn torrent_section_detail_lines(
    completed: &[&TorrentListItem],
    downloading: &[&TorrentListItem],
    not_started: &[&TorrentListItem],
) -> Vec<String> {
    let mut lines = Vec::new();
    if !completed.is_empty() {
        lines.push(format!("\n{}", format!("Completed ({}):", completed.len()).green()));
        for torrent in completed {
            lines.extend(torrent_detail_lines(torrent));
        }
    }

    let incomplete_count = downloading.len() + not_started.len();
    if incomplete_count > 0 {
        lines.push(format!("\n{}", format!("Incomplete ({incomplete_count}):").yellow()));
        for torrent in downloading.iter().chain(not_started) {
            lines.extend(torrent_detail_lines(torrent));
        }
    }
    lines
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

/// One torrent as a single compact line.
///
/// When verbose is enabled, additional columns are shown: ratio, added date, and completed date.
fn torrent_list_line(torrent: &TorrentListItem, verbose: bool) -> String {
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

        format!(
            "{progress}  {size:>10}  {:<16}  {ratio:>6}  {}  {}  {}{}",
            torrent.save_path.dimmed(),
            added.dimmed(),
            completed.dimmed(),
            torrent.name,
            tags_str.dimmed(),
        )
    } else {
        format!(
            "{progress}  {size:>10}  {:<16}  {}{}",
            torrent.save_path.dimmed(),
            torrent.name,
            tags_str.dimmed(),
        )
    }
}

/// Full details for one torrent, as several lines.
fn torrent_detail_lines(torrent: &TorrentListItem) -> Vec<String> {
    let size = cli_tools::format_size(torrent.size.max(0) as u64);
    let progress = format_progress(torrent);

    let mut lines = vec![
        format!("  {}", torrent.name.bold()),
        format!("    {:<14} {:>6}  {:<14} {}", "Progress:", progress, "Size:", size),
        format!(
            "    {:<14} {:<20}  {:<14} {}",
            "Ratio:",
            format!("{:.2}", torrent.ratio),
            "Save path:",
            torrent.save_path.dimmed(),
        ),
        format!(
            "    {:<14} {}",
            "Added:",
            cli_tools::format_timestamp(torrent.added_on).dimmed(),
        ),
    ];

    if let Some(completed_on) = torrent.completion_on {
        lines.push(format!(
            "    {:<14} {}",
            "Completed:",
            cli_tools::format_timestamp(completed_on).dimmed(),
        ));
    }

    if !torrent.tags.is_empty() {
        lines.push(format!("    {:<14} {}", "Tags:", torrent.tags.cyan()));
    }
    lines
}

#[cfg(test)]
mod test_torrent_classification {
    use super::*;

    fn torrent(name: &str, progress: f64, completion_on: Option<i64>, size: i64, save_path: &str) -> TorrentListItem {
        TorrentListItem {
            hash: name.to_string(),
            name: name.to_string(),
            added_on: 1,
            completion_on,
            progress,
            ratio: 0.0,
            save_path: save_path.to_string(),
            size,
            tags: String::new(),
        }
    }

    #[test]
    fn classifies_completed_downloading_and_not_started_torrents() {
        assert!(matches!(
            classify_torrent(&torrent("complete", 1.0, None, 1, "/a")),
            TorrentStatus::Completed
        ));
        assert!(matches!(
            classify_torrent(&torrent("timestamp", 0.5, Some(10), 1, "/a")),
            TorrentStatus::Completed
        ));
        assert!(matches!(
            classify_torrent(&torrent("downloading", 0.5, None, 1, "/a")),
            TorrentStatus::Downloading
        ));
        assert!(matches!(
            classify_torrent(&torrent("waiting", 0.0, None, 1, "/a")),
            TorrentStatus::NotStarted
        ));
    }

    #[test]
    fn sorts_names_case_insensitively() {
        let alpha = torrent("alpha", 0.0, None, 1, "/b");
        let beta = torrent("Beta", 0.0, None, 2, "/a");
        let mut torrents = vec![&beta, &alpha];

        sort_torrents(&mut torrents, SortOrder::Name);

        assert_eq!(
            torrents.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "Beta"]
        );
    }

    #[test]
    fn sorts_size_descending_with_name_tiebreaker() {
        let alpha = torrent("alpha", 0.0, None, 10, "/b");
        let beta = torrent("Beta", 0.0, None, 20, "/a");
        let gamma = torrent("gamma", 0.0, None, 20, "/c");
        let mut torrents = vec![&gamma, &alpha, &beta];

        sort_torrents(&mut torrents, SortOrder::Size);

        assert_eq!(
            torrents.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["Beta", "gamma", "alpha"]
        );
    }

    #[test]
    fn sorts_paths_case_insensitively_with_name_tiebreaker() {
        let alpha = torrent("alpha", 0.0, None, 10, "/same");
        let beta = torrent("Beta", 0.0, None, 20, "/A");
        let gamma = torrent("gamma", 0.0, None, 20, "/SAME");
        let mut torrents = vec![&gamma, &alpha, &beta];

        sort_torrents(&mut torrents, SortOrder::Path);

        assert_eq!(
            torrents.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["Beta", "alpha", "gamma"]
        );
    }

    #[test]
    fn formats_progress_for_each_status() {
        assert!(format_progress(&torrent("complete", 1.0, None, 1, "/a")).contains("100%"));
        assert!(format_progress(&torrent("downloading", 0.5, None, 1, "/a")).contains("50%"));
        assert!(format_progress(&torrent("waiting", 0.0, None, 1, "/a")).contains("0%"));
    }
}

#[cfg(test)]
mod test_statistics_lines {
    use super::*;

    /// Message without the terminal color codes.
    fn plain(message: &str) -> String {
        let mut result = String::with_capacity(message.len());
        let mut characters = message.chars();
        while let Some(character) = characters.next() {
            if character != '\u{1b}' {
                result.push(character);
                continue;
            }
            for escape in characters.by_ref() {
                if escape == 'm' {
                    break;
                }
            }
        }
        result
    }

    /// Torrent with the given name, progress, size, and tags.
    fn torrent(name: &str, progress: f64, size: i64, tags: &str) -> TorrentListItem {
        TorrentListItem {
            hash: name.to_string(),
            name: name.to_string(),
            added_on: 1_700_000_000,
            completion_on: if progress >= 1.0 { Some(1_700_000_100) } else { None },
            progress,
            ratio: 1.25,
            save_path: "/downloads".to_string(),
            size,
            tags: tags.to_string(),
        }
    }

    /// Torrents keyed by hash, as the client returns them.
    fn torrents(items: Vec<TorrentListItem>) -> HashMap<String, TorrentListItem> {
        items.into_iter().map(|item| (item.hash.clone(), item)).collect()
    }

    /// Print options for the given modes.
    fn options(list: bool, verbose: bool) -> PrintOptions {
        PrintOptions {
            sort: SortOrder::Name,
            list,
            verbose,
        }
    }

    /// The whole report as one plain string.
    fn report(items: Vec<TorrentListItem>, list: bool, verbose: bool) -> String {
        plain(&statistics_lines(&torrents(items), &options(list, verbose)).join("\n"))
    }

    #[test]
    fn an_empty_list_reports_that_nothing_was_found() {
        let text = report(Vec::new(), false, false);

        assert_eq!(text, "No torrents found");
    }

    #[test]
    fn the_summary_counts_and_sizes_every_category() {
        let text = report(
            vec![
                torrent("done", 1.0, 1024 * 1024, ""),
                torrent("half", 0.5, 2 * 1024 * 1024, ""),
                torrent("queued", 0.0, 4 * 1024 * 1024, ""),
            ],
            false,
            false,
        );

        assert!(text.contains("Total torrents"), "{text}");
        assert!(text.contains("Completed"), "{text}");
        assert!(text.contains("Downloading"), "{text}");
        assert!(text.contains("Not started"), "{text}");
        assert!(text.contains("7.00 MB"), "the total size should be the sum: {text}");
    }

    #[test]
    fn categories_with_nothing_in_them_are_left_out() {
        let text = report(vec![torrent("done", 1.0, 1024, "")], false, false);

        assert!(text.contains("Completed"), "{text}");
        assert!(!text.contains("Downloading"), "{text}");
        assert!(!text.contains("Not started"), "{text}");
    }

    #[test]
    fn list_mode_prints_one_line_per_torrent() {
        let text = report(
            vec![
                torrent("done", 1.0, 1024, ""),
                torrent("half", 0.5, 1024, ""),
                torrent("queued", 0.0, 1024, ""),
            ],
            true,
            false,
        );

        for name in ["done", "half", "queued"] {
            assert!(text.contains(name), "{name} should be listed: {text}");
        }
        assert!(!text.contains("Progress:"), "list mode should stay compact: {text}");
    }

    #[test]
    fn verbose_list_mode_adds_the_ratio_and_the_dates() {
        let compact = report(vec![torrent("done", 1.0, 1024, "")], true, false);
        let verbose = report(vec![torrent("done", 1.0, 1024, "")], true, true);

        assert!(verbose.contains("1.25"), "the ratio should be shown: {verbose}");
        assert!(verbose.len() > compact.len());
    }

    #[test]
    fn verbose_mode_prints_the_section_headings_and_the_details() {
        let text = report(
            vec![torrent("done", 1.0, 1024, "movies"), torrent("half", 0.5, 1024, "")],
            false,
            true,
        );

        assert!(text.contains("Completed (1):"), "{text}");
        assert!(text.contains("Incomplete (1):"), "{text}");
        assert!(text.contains("Progress:"), "{text}");
        assert!(text.contains("Save path:"), "{text}");
        assert!(
            text.contains("Completed:"),
            "a finished torrent has a completion date: {text}"
        );
        assert!(text.contains("movies"), "tags should be shown: {text}");
    }

    #[test]
    fn an_unfinished_torrent_has_no_completion_date_and_no_tag_line() {
        let text = report(vec![torrent("half", 0.5, 1024, "")], false, true);

        assert!(text.contains("Incomplete (1):"), "{text}");
        assert!(!text.contains("Completed:"), "{text}");
        assert!(!text.contains("Tags:"), "{text}");
    }

    #[test]
    fn a_negative_size_is_counted_as_zero() {
        let text = report(vec![torrent("broken", 1.0, -1, "")], false, false);

        assert!(text.contains("0 B"), "{text}");
    }

    #[test]
    fn printing_the_statistics_does_not_panic() {
        let items = torrents(vec![torrent("done", 1.0, 1024, "tag")]);

        print_statistics(&items, &options(false, false));
        print_statistics(&items, &options(true, true));
        print_statistics(&HashMap::new(), &options(false, false));
    }
}
