//! qtorrent - Add torrents to qBittorrent with automatic file renaming.
//!
//! This CLI tool parses `.torrent` files and adds them to qBittorrent via the `WebUI` API,
//! automatically renaming the output file based on the torrent filename.

mod add;
mod config;
mod qbittorrent;
mod stats;
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
#[allow(clippy::doc_markdown)]
pub struct QtorrentArgs {
    /// Optional input path(s) with torrent files or directories
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    pub path: Vec<PathBuf>,

    /// qBittorrent WebUI host
    #[arg(short = 'H', long, name = "HOST")]
    host: Option<String>,

    /// qBittorrent WebUI port
    #[arg(short = 'P', long, name = "PORT")]
    port: Option<u16>,

    /// qBittorrent WebUI username
    #[arg(short = 'u', long, name = "USER")]
    username: Option<String>,

    /// qBittorrent WebUI password
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

    /// Directory names to skip (case-insensitive full name match)
    #[arg(short = 'k', long = "skip-name", name = "NAME", value_delimiter = ',')]
    skip_names: Vec<String>,

    /// Minimum file size in MB (files smaller than this will be skipped)
    #[arg(short = 'm', long = "min-size", name = "MB")]
    min_file_size_mb: Option<f64>,

    /// Recurse into subdirectories when searching for torrent files
    #[arg(short = 'r', long)]
    pub recurse: bool,

    /// Skip rename prompts for existing/duplicate torrents
    #[arg(short = 'x', long)]
    pub skip_existing: bool,

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
        QTorrent::new(args)?.run().await
    }
}

#[cfg(test)]
mod cli_args_tests {
    use super::*;

    #[test]
    fn parses_multiple_paths() {
        let args =
            QtorrentArgs::try_parse_from(["test", "/path/one.torrent", "/path/two.torrent"]).expect("should parse");
        assert_eq!(args.path.len(), 2);
    }

    #[test]
    fn parses_host_and_port() {
        let args = QtorrentArgs::try_parse_from(["test", "-H", "192.168.1.100", "-P", "9090"]).expect("should parse");
        assert_eq!(args.host, Some("192.168.1.100".to_string()));
        assert_eq!(args.port, Some(9090));
    }

    #[test]
    fn parses_credentials() {
        let args = QtorrentArgs::try_parse_from(["test", "-u", "admin", "-w", "secret"]).expect("should parse");
        assert_eq!(args.username, Some("admin".to_string()));
        assert_eq!(args.password, Some("secret".to_string()));
    }

    #[test]
    fn parses_save_path_and_category() {
        let args = QtorrentArgs::try_parse_from(["test", "-s", "/downloads", "-c", "movies"]).expect("should parse");
        assert_eq!(args.save_path, Some("/downloads".to_string()));
        assert_eq!(args.category, Some("movies".to_string()));
    }

    #[test]
    fn parses_tags() {
        let args = QtorrentArgs::try_parse_from(["test", "-t", "hd,new,important"]).expect("should parse");
        assert_eq!(args.tags, Some("hd,new,important".to_string()));
    }

    #[test]
    fn parses_comma_delimited_skip_extensions() {
        let args = QtorrentArgs::try_parse_from(["test", "-e", "nfo,txt,jpg"]).expect("should parse");
        assert_eq!(args.skip_extensions, vec!["nfo", "txt", "jpg"]);
    }

    #[test]
    fn parses_comma_delimited_skip_names() {
        let args = QtorrentArgs::try_parse_from(["test", "-k", "sample,subs,screens"]).expect("should parse");
        assert_eq!(args.skip_names, vec!["sample", "subs", "screens"]);
    }

    #[test]
    fn parses_min_file_size() {
        let args = QtorrentArgs::try_parse_from(["test", "-m", "50.5"]).expect("should parse");
        assert_eq!(args.min_file_size_mb, Some(50.5));
    }

    #[test]
    fn parses_combined_boolean_flags() {
        let args = QtorrentArgs::try_parse_from(["test", "-apyrxv"]).expect("should parse");
        assert!(args.paused);
        assert!(args.dryrun);
        assert!(args.yes);
        assert!(args.recurse);
        assert!(args.skip_existing);
        assert!(args.verbose);
    }

    #[test]
    fn parses_long_form_flags() {
        let args = QtorrentArgs::try_parse_from([
            "test",
            "--paused",
            "--dryrun",
            "--yes",
            "--recurse",
            "--skip-existing",
            "--verbose",
        ])
        .expect("should parse");
        assert!(args.paused);
        assert!(args.dryrun);
        assert!(args.yes);
        assert!(args.recurse);
        assert!(args.skip_existing);
        assert!(args.verbose);
    }

    #[test]
    fn parses_long_form_skip_ext() {
        let args = QtorrentArgs::try_parse_from(["test", "--skip-ext", "nfo,txt"]).expect("should parse");
        assert_eq!(args.skip_extensions, vec!["nfo", "txt"]);
    }

    #[test]
    fn parses_long_form_skip_name() {
        let args = QtorrentArgs::try_parse_from(["test", "--skip-name", "sample,subs"]).expect("should parse");
        assert_eq!(args.skip_names, vec!["sample", "subs"]);
    }

    #[test]
    fn parses_long_form_min_size() {
        let args = QtorrentArgs::try_parse_from(["test", "--min-size", "100"]).expect("should parse");
        assert_eq!(args.min_file_size_mb, Some(100.0));
    }

    #[test]
    fn empty_by_default() {
        let args = QtorrentArgs::try_parse_from(["test"]).expect("should parse");
        assert!(args.path.is_empty());
        assert!(args.host.is_none());
        assert!(args.port.is_none());
        assert!(args.username.is_none());
        assert!(args.password.is_none());
        assert!(args.skip_extensions.is_empty());
        assert!(args.skip_names.is_empty());
        assert!(args.min_file_size_mb.is_none());
        assert!(!args.paused);
        assert!(!args.dryrun);
    }

    #[test]
    fn parses_complex_combination() {
        let args = QtorrentArgs::try_parse_from([
            "test",
            "/path/to/torrent.torrent",
            "-H",
            "192.168.1.100",
            "-P",
            "8080",
            "-u",
            "admin",
            "-w",
            "pass",
            "-s",
            "/downloads",
            "-c",
            "movies",
            "-t",
            "hd,new",
            "-e",
            "nfo,txt",
            "-k",
            "sample",
            "-m",
            "50",
            "-a",
            "-r",
            "-v",
        ])
        .expect("should parse");

        assert_eq!(args.path.len(), 1);
        assert_eq!(args.host, Some("192.168.1.100".to_string()));
        assert_eq!(args.port, Some(8080));
        assert_eq!(args.username, Some("admin".to_string()));
        assert_eq!(args.password, Some("pass".to_string()));
        assert_eq!(args.save_path, Some("/downloads".to_string()));
        assert_eq!(args.category, Some("movies".to_string()));
        assert_eq!(args.tags, Some("hd,new".to_string()));
        assert_eq!(args.skip_extensions, vec!["nfo", "txt"]);
        assert_eq!(args.skip_names, vec!["sample"]);
        assert_eq!(args.min_file_size_mb, Some(50.0));
        assert!(args.paused);
        assert!(args.recurse);
        assert!(args.verbose);
    }

    #[test]
    fn rejects_invalid_port() {
        let result = QtorrentArgs::try_parse_from(["test", "-P", "not_a_number"]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_min_size() {
        let result = QtorrentArgs::try_parse_from(["test", "-m", "not_a_number"]);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod config_from_args_tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn config_has_host_and_port() {
        let args = QtorrentArgs::try_parse_from(["test"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // Should have host and port (from CLI, config, or defaults)
        assert!(!config.host.is_empty());
        assert!(config.port > 0);
    }

    #[test]
    fn config_cli_overrides_host_and_port() {
        let args = QtorrentArgs::try_parse_from(["test", "-H", "192.168.1.1", "-P", "9000"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI values should take priority
        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 9000);
    }

    #[test]
    fn config_includes_cli_skip_extensions_normalized() {
        let args = QtorrentArgs::try_parse_from(["test", "-e", ".NFO,.TXT"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI extensions should be included as lowercase without leading dots
        assert!(config.skip_extensions.contains(&"nfo".to_string()));
        assert!(config.skip_extensions.contains(&"txt".to_string()));
    }

    #[test]
    fn config_includes_cli_skip_names_normalized() {
        let args = QtorrentArgs::try_parse_from(["test", "-k", "SAMPLE,Subs"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI names should be included as lowercase
        assert!(config.skip_names.contains(&"sample".to_string()));
        assert!(config.skip_names.contains(&"subs".to_string()));
    }

    #[test]
    fn config_cli_min_size_converts_to_bytes() {
        let args = QtorrentArgs::try_parse_from(["test", "-m", "10"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        // CLI 10 MB = 10 * 1024 * 1024 bytes should take priority
        assert_eq!(config.min_file_size_bytes, Some(10 * 1024 * 1024));
    }

    #[test]
    fn config_has_credentials_when_cli_sets_them() {
        let args = QtorrentArgs::try_parse_from(["test", "-u", "user", "-w", "pass"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.has_credentials());
        assert_eq!(config.username, "user");
        assert_eq!(config.password, "pass");
    }

    #[test]
    fn config_cli_extensions_enable_file_filters() {
        let args = QtorrentArgs::try_parse_from(["test", "-e", "nfo"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.has_file_filters());
        assert!(config.skip_extensions.contains(&"nfo".to_string()));
    }

    #[test]
    fn config_cli_names_enable_file_filters() {
        let args = QtorrentArgs::try_parse_from(["test", "-k", "sample"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.has_file_filters());
        assert!(config.skip_names.contains(&"sample".to_string()));
    }

    #[test]
    fn config_cli_min_size_enables_file_filters() {
        let args = QtorrentArgs::try_parse_from(["test", "-m", "50"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.has_file_filters());
        assert!(config.min_file_size_bytes.is_some());
    }

    #[test]
    fn config_cli_boolean_flags_enable_options() {
        let args = QtorrentArgs::try_parse_from(["test", "-a", "-p", "-y", "-r", "-x", "-v"]).expect("should parse");
        let config = Config::from_args(args).expect("config should parse");
        assert!(config.paused);
        assert!(config.dryrun);
        assert!(config.yes);
        assert!(config.recurse);
        assert!(config.skip_existing);
        assert!(config.verbose);
    }
}
