//! Configuration module for qtorrent.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use cli_tools::print_error;
use serde::Deserialize;

use crate::QtorrentArgs;

/// Default qBittorrent `WebUI` host.
const DEFAULT_HOST: &str = "localhost";

/// Default qBittorrent `WebUI` port.
const DEFAULT_PORT: u16 = 8080;

/// User configuration from the config file.
#[derive(Debug, Default, Deserialize)]
pub struct QtorrentConfig {
    /// qBittorrent `WebUI` host.
    #[serde(default)]
    host: Option<String>,
    /// qBittorrent `WebUI` port.
    #[serde(default)]
    port: Option<u16>,
    /// qBittorrent `WebUI` username.
    #[serde(default)]
    username: Option<String>,
    /// qBittorrent `WebUI` password.
    #[serde(default)]
    password: Option<String>,
    /// Default save path for torrents.
    #[serde(default)]
    save_path: Option<String>,
    /// Default category for torrents.
    #[serde(default)]
    category: Option<String>,
    /// Default tags for torrents.
    #[serde(default)]
    tags: Option<String>,
    /// Add torrents in paused state by default.
    #[serde(default)]
    paused: bool,
    /// Enable verbose output by default.
    #[serde(default)]
    verbose: bool,
    /// Enable dry-run mode by default.
    #[serde(default)]
    dryrun: bool,
    /// Skip confirmation prompts by default.
    #[serde(default)]
    yes: bool,
    /// File extensions to skip (without dot, e.g., "nfo", "txt", "jpg").
    #[serde(default)]
    skip_extensions: Vec<String>,
    /// File or folder names to skip (case-insensitive partial match).
    #[serde(default)]
    skip_names: Vec<String>,
    /// Minimum file size in MB. Files smaller than this will be skipped.
    #[serde(default)]
    min_file_size_mb: Option<f64>,
}

/// Final config combined from CLI arguments and user config file.
#[derive(Debug)]
pub struct Config {
    /// qBittorrent `WebUI` host.
    pub host: String,
    /// qBittorrent `WebUI` port.
    pub port: u16,
    /// qBittorrent `WebUI` username.
    pub username: String,
    /// qBittorrent `WebUI` password.
    pub password: String,
    /// Save path for torrents.
    pub save_path: Option<String>,
    /// Category for torrents.
    pub category: Option<String>,
    /// Tags for torrents.
    pub tags: Option<String>,
    /// Add torrent in paused state.
    pub paused: bool,
    /// Verbose output.
    pub verbose: bool,
    /// Dry-run mode (don't actually add torrents).
    pub dryrun: bool,
    /// Skip confirmation prompts.
    pub yes: bool,
    /// Input torrent file paths.
    pub torrent_paths: Vec<PathBuf>,
    /// File extensions to skip (lowercase, without dot).
    pub skip_extensions: Vec<String>,
    /// File or folder names to skip (lowercase for case-insensitive matching).
    pub skip_names: Vec<String>,
    /// Minimum file size in bytes. Files smaller than this will be skipped.
    pub min_file_size_bytes: Option<i64>,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    qtorrent: QtorrentConfig,
}

impl QtorrentConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    pub fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|error| {
                        print_error!("Error reading config file {}: {error}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|error| {
                        print_error!("Error parsing config file: {error}");
                    })
                    .ok()
            })
            .map(|config| config.qtorrent)
            .unwrap_or_default()
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    ///
    /// # Errors
    /// Returns an error if required credentials are missing.
    pub fn try_from_args(args: QtorrentArgs, user_config: QtorrentConfig) -> Result<Self> {
        // Resolve torrent file paths
        let mut torrent_paths = Vec::new();
        for path in args.torrents {
            let resolved = cli_tools::resolve_required_input_path(&path)?;
            torrent_paths.push(resolved);
        }

        // Get credentials from args or config, with args taking priority
        let host = args
            .host
            .or(user_config.host)
            .unwrap_or_else(|| DEFAULT_HOST.to_string());

        let port = args.port.or(user_config.port).unwrap_or(DEFAULT_PORT);

        let username = args.username.or(user_config.username).unwrap_or_default();

        let password = args.password.or(user_config.password).unwrap_or_default();

        // Get other options from args or config
        let save_path = args.save_path.or(user_config.save_path);
        let category = args.category.or(user_config.category);
        let tags = args.tags.or(user_config.tags);
        let paused = args.paused || user_config.paused;
        let verbose = args.verbose || user_config.verbose;
        let dryrun = args.dryrun || user_config.dryrun;
        let yes = args.yes || user_config.yes;

        // File filtering options - merge CLI args with config, CLI takes priority
        let skip_extensions: Vec<String> = if args.skip_extensions.is_empty() {
            user_config
                .skip_extensions
                .into_iter()
                .map(|extension| extension.to_lowercase().trim_start_matches('.').to_string())
                .collect()
        } else {
            args.skip_extensions
                .into_iter()
                .map(|extension| extension.to_lowercase().trim_start_matches('.').to_string())
                .collect()
        };

        let skip_names: Vec<String> = if args.skip_names.is_empty() {
            user_config
                .skip_names
                .into_iter()
                .map(|name| name.to_lowercase())
                .collect()
        } else {
            args.skip_names.into_iter().map(|name| name.to_lowercase()).collect()
        };

        // Convert MB to bytes for easier comparison
        let min_file_size_bytes = args
            .min_file_size_mb
            .or(user_config.min_file_size_mb)
            .map(|mb| (mb * 1024.0 * 1024.0) as i64);

        Ok(Self {
            host,
            port,
            username,
            password,
            save_path,
            category,
            tags,
            paused,
            verbose,
            dryrun,
            yes,
            torrent_paths,
            skip_extensions,
            skip_names,
            min_file_size_bytes,
        })
    }

    /// Check if credentials are provided.
    #[must_use]
    pub const fn has_credentials(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty()
    }

    /// Check if any file filtering is configured.
    #[must_use]
    pub const fn has_file_filters(&self) -> bool {
        !self.skip_extensions.is_empty() || !self.skip_names.is_empty() || self.min_file_size_bytes.is_some()
    }
}
