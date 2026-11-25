//! Adding torrents to qBittorrent with automatic file renaming.
//!
//! Parses `.torrent` files and filters their files, suggests output names with optional dots formatting,
//! resolves tags, and talks to the qBittorrent `WebUI` API to add, rename, and list torrents.
//! Nothing here reads command line arguments, prompts, or prints,
//! so other projects can use the components directly.
//! Adding reports the steps after each add request through the `AddObserver` trait.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod add;
pub mod collector;
pub mod connection;
pub mod naming;
pub mod qbittorrent;
pub mod statistics;
pub mod stats;
pub mod tags;
pub mod torrent;
pub mod utils;

#[cfg(test)]
mod test_server;

pub use add::{AddEvent, AddObserver, AddOptions, ParsedTorrents, TorrentAdder, find_existing_torrent, parse_torrents};
pub use collector::TorrentPathCollector;
pub use connection::{Connection, ConnectionSettings};
pub use naming::{NameSuggester, NamingOptions};
pub use qbittorrent::{QBittorrentClient, TorrentListItem};
pub use statistics::{SortOrder, TorrentStatistics, TorrentStatus};
pub use stats::TorrentStats;
pub use tags::{TagOverwrite, TagPathOverwrite, TagRules};
pub use torrent::{FileFilter, parse_torrent};
pub use utils::TorrentInfo;
