//! Statistics about the torrents in qBittorrent.
//!
//! Classifies torrents by download status, sorts them, and sums the counts and sizes of each status.

use std::collections::HashMap;
use std::hash::BuildHasher;

use crate::qbittorrent::TorrentListItem;

/// Sort order for a torrent listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortOrder {
    /// Sort alphabetically by torrent name
    #[default]
    Name,
    /// Sort by total size (largest first)
    Size,
    /// Sort by save path
    Path,
}

/// Torrent categorized by download status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentStatus {
    /// Download is complete.
    Completed,
    /// Download is in progress (0 < progress < 1).
    Downloading,
    /// Download has not started (progress == 0).
    NotStarted,
}

/// Torrents grouped by download status, each group sorted, with the total size of each group.
#[derive(Debug, Default)]
pub struct TorrentStatistics<'a> {
    /// Number of torrents.
    pub total_count: usize,
    /// Total size of all torrents in bytes.
    pub total_size: u64,
    /// Completed torrents.
    pub completed: Vec<&'a TorrentListItem>,
    /// Total size of the completed torrents in bytes.
    pub completed_size: u64,
    /// Torrents that are downloading.
    pub downloading: Vec<&'a TorrentListItem>,
    /// Total size of the downloading torrents in bytes.
    pub downloading_size: u64,
    /// Torrents that have not started downloading.
    pub not_started: Vec<&'a TorrentListItem>,
    /// Total size of the torrents that have not started in bytes.
    pub not_started_size: u64,
}

impl<'a> TorrentStatistics<'a> {
    /// Group the torrents by download status and sort each group in the given order.
    ///
    /// A negative size reported by the API counts as zero.
    #[must_use]
    pub fn new<S: BuildHasher>(torrents: &'a HashMap<String, TorrentListItem, S>, sort: SortOrder) -> Self {
        let mut statistics = Self {
            total_count: torrents.len(),
            ..Self::default()
        };

        for torrent in torrents.values() {
            let size = torrent.size.max(0) as u64;
            statistics.total_size += size;

            match classify_torrent(torrent) {
                TorrentStatus::Completed => {
                    statistics.completed_size += size;
                    statistics.completed.push(torrent);
                }
                TorrentStatus::Downloading => {
                    statistics.downloading_size += size;
                    statistics.downloading.push(torrent);
                }
                TorrentStatus::NotStarted => {
                    statistics.not_started_size += size;
                    statistics.not_started.push(torrent);
                }
            }
        }

        sort_torrents(&mut statistics.completed, sort);
        sort_torrents(&mut statistics.downloading, sort);
        sort_torrents(&mut statistics.not_started, sort);
        statistics
    }
}

/// Classify a torrent by its download status.
#[must_use]
pub fn classify_torrent(torrent: &TorrentListItem) -> TorrentStatus {
    if torrent.is_completed() {
        TorrentStatus::Completed
    } else if torrent.progress > 0.0 {
        TorrentStatus::Downloading
    } else {
        TorrentStatus::NotStarted
    }
}

/// Sort a slice of torrent references by the given sort order.
///
/// Names and paths compare case-insensitively, and ties are broken by name.
pub fn sort_torrents(torrents: &mut [&TorrentListItem], sort: SortOrder) {
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
        assert_eq!(
            classify_torrent(&torrent("complete", 1.0, None, 1, "/a")),
            TorrentStatus::Completed
        );
        assert_eq!(
            classify_torrent(&torrent("timestamp", 0.5, Some(10), 1, "/a")),
            TorrentStatus::Completed
        );
        assert_eq!(
            classify_torrent(&torrent("downloading", 0.5, None, 1, "/a")),
            TorrentStatus::Downloading
        );
        assert_eq!(
            classify_torrent(&torrent("waiting", 0.0, None, 1, "/a")),
            TorrentStatus::NotStarted
        );
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
    fn statistics_group_count_and_size_each_status() {
        let items: HashMap<String, TorrentListItem> = [
            torrent("done", 1.0, None, 1024, "/a"),
            torrent("half", 0.5, None, 2048, "/a"),
            torrent("queued", 0.0, None, 4096, "/a"),
            torrent("broken", 0.0, None, -1, "/a"),
        ]
        .into_iter()
        .map(|item| (item.hash.clone(), item))
        .collect();

        let statistics = TorrentStatistics::new(&items, SortOrder::Name);

        assert_eq!(statistics.total_count, 4);
        assert_eq!(statistics.total_size, 7168);
        assert_eq!(statistics.completed.len(), 1);
        assert_eq!(statistics.completed_size, 1024);
        assert_eq!(statistics.downloading.len(), 1);
        assert_eq!(statistics.downloading_size, 2048);
        assert_eq!(
            statistics
                .not_started
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["broken", "queued"]
        );
        assert_eq!(statistics.not_started_size, 4096);
    }
}
