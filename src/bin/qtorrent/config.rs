//! Configuration module for qtorrent.
//!
//! Handles reading configuration from CLI arguments and the user config file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use walkdir::WalkDir;

use cli_tools::print_error;

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
    /// Skip rename prompts for existing/duplicate torrents.
    #[serde(default)]
    skip_existing: bool,
    /// Recurse into subdirectories when searching for torrent files.
    #[serde(default)]
    recurse: bool,
    /// File extensions to skip (without dot, e.g., "nfo", "txt", "jpg").
    #[serde(default)]
    skip_extensions: Vec<String>,
    /// Directory names to skip (case-insensitive match
    #[serde(default)]
    skip_names: Vec<String>,
    /// Minimum file size in MB. Files smaller than this will be skipped.
    #[serde(default)]
    min_file_size_mb: Option<f64>,
    /// Substrings to remove from torrent filename when generating suggested name.
    #[serde(default)]
    remove_from_name: Vec<String>,
    /// Apply dots formatting to suggested name (uses dots config from config file).
    #[serde(default)]
    use_dots_formatting: bool,
    /// If torrent filename contains any of these strings, ignore it and use internal name instead.
    #[serde(default)]
    ignore_torrent_names: Vec<String>,
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
    /// Skip rename prompts for existing/duplicate torrents.
    pub skip_existing: bool,
    /// Recurse into subdirectories when searching for torrent files.
    pub recurse: bool,
    /// Input paths from command line arguments.
    pub input_paths: Vec<PathBuf>,
    /// File extensions to skip (lowercase, without dot).
    pub skip_extensions: Vec<String>,
    /// Directory names to skip (lowercase for case-insensitive full name matching).
    pub skip_names: Vec<String>,
    /// Minimum file size in bytes. Files smaller than this will be skipped.
    pub min_file_size_bytes: Option<u64>,
    /// Substrings to remove from torrent filename when generating suggested name.
    pub remove_from_name: Vec<String>,
    /// Apply dots formatting to suggested name (uses dots config from config file).
    pub use_dots_formatting: bool,
    /// If torrent filename contains any of these strings, ignore it and use internal name instead.
    pub ignore_torrent_names: Vec<String>,
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
            .and_then(|config_string| Self::from_toml_str(&config_string).ok())
            .unwrap_or_default()
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML string is invalid.
    pub fn from_toml_str(toml_str: &str) -> Result<Self> {
        toml::from_str::<UserConfig>(toml_str)
            .map(|config| config.qtorrent)
            .map_err(|e| anyhow!("Failed to parse config: {e}"))
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    #[must_use]
    pub fn from_args(args: QtorrentArgs) -> Self {
        let user_config = QtorrentConfig::get_user_config();

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
        let skip_existing = args.skip_existing || user_config.skip_existing;
        let recurse = args.recurse || user_config.recurse;

        // Resolve input paths
        let input_paths = args.path;

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
            .map(|mb| (mb * 1024.0 * 1024.0) as u64);

        // Substrings to remove from suggested name
        let remove_from_name = user_config.remove_from_name;

        // Dots formatting option
        let use_dots_formatting = user_config.use_dots_formatting;

        // Filename ignore patterns
        let ignore_torrent_names = user_config.ignore_torrent_names;

        Self {
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
            skip_existing,
            recurse,
            input_paths,
            skip_extensions,
            skip_names,
            min_file_size_bytes,
            remove_from_name,
            use_dots_formatting,
            ignore_torrent_names,
        }
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

    /// Collect torrent file paths from the configured input paths.
    ///
    /// If no paths are provided, the current working directory is used.
    /// If a path is a directory, it is searched for `.torrent` files.
    /// If a path is a `.torrent` file, it is used directly.
    /// If `recurse` is true, directories are searched recursively.
    ///
    /// # Errors
    /// Returns an error if paths cannot be resolved or directories cannot be read.
    pub fn collect_torrent_paths(&self) -> Result<Vec<PathBuf>> {
        let mut torrent_paths = Vec::new();

        if self.input_paths.is_empty() {
            // No paths provided, use current working directory
            let current_directory = cli_tools::resolve_input_path(None)?;
            Self::collect_torrents_from_directory(&current_directory, &mut torrent_paths, self.recurse)?;
        } else {
            for path in &self.input_paths {
                let resolved = cli_tools::resolve_required_input_path(path)?;
                if resolved.is_dir() {
                    Self::collect_torrents_from_directory(&resolved, &mut torrent_paths, self.recurse)?;
                } else if Self::is_torrent_file(&resolved) {
                    torrent_paths.push(resolved);
                }
            }
        }

        torrent_paths.sort_unstable();
        torrent_paths.dedup();

        Ok(torrent_paths)
    }

    /// Collect all `.torrent` files from the given directory.
    ///
    /// If `recurse` is true, subdirectories are searched recursively.
    fn collect_torrents_from_directory(
        directory: &Path,
        torrent_paths: &mut Vec<PathBuf>,
        recurse: bool,
    ) -> Result<()> {
        if recurse {
            for entry in WalkDir::new(directory)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                let path = entry.path();
                if path.is_file() && Self::is_torrent_file(path) {
                    torrent_paths.push(path.to_path_buf());
                }
            }
        } else {
            for entry in
                fs::read_dir(directory).with_context(|| format!("Failed to read directory: {}", directory.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && Self::is_torrent_file(&path) {
                    torrent_paths.push(path);
                }
            }
        }
        Ok(())
    }

    /// Check if the given path is a `.torrent` file.
    fn is_torrent_file(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
    }
}

#[cfg(test)]
mod qtorrent_config_tests {
    use super::*;

    #[test]
    fn from_toml_str_parses_empty_config() {
        let toml = "";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse empty config");
        assert!(config.host.is_none());
        assert!(config.port.is_none());
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert!(!config.paused);
        assert!(!config.verbose);
        assert!(!config.dryrun);
    }

    #[test]
    fn from_toml_str_parses_qtorrent_section() {
        let toml = r#"
[qtorrent]
host = "192.168.1.100"
port = 9090
username = "admin"
password = "secret"
paused = true
verbose = true
dryrun = true
yes = true
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.host, Some("192.168.1.100".to_string()));
        assert_eq!(config.port, Some(9090));
        assert_eq!(config.username, Some("admin".to_string()));
        assert_eq!(config.password, Some("secret".to_string()));
        assert!(config.paused);
        assert!(config.verbose);
        assert!(config.dryrun);
        assert!(config.yes);
    }

    #[test]
    fn from_toml_str_parses_save_path_and_category() {
        let toml = r#"
[qtorrent]
save_path = "/downloads/torrents"
category = "movies"
tags = "hd,new"
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.save_path, Some("/downloads/torrents".to_string()));
        assert_eq!(config.category, Some("movies".to_string()));
        assert_eq!(config.tags, Some("hd,new".to_string()));
    }

    #[test]
    fn from_toml_str_parses_file_filtering_options() {
        let toml = r#"
[qtorrent]
skip_extensions = ["nfo", "txt", "jpg"]
skip_names = ["sample", "subs"]
min_file_size_mb = 50.0
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.skip_extensions, vec!["nfo", "txt", "jpg"]);
        assert_eq!(config.skip_names, vec!["sample", "subs"]);
        assert_eq!(config.min_file_size_mb, Some(50.0));
    }

    #[test]
    fn from_toml_str_parses_name_processing_options() {
        let toml = r#"
[qtorrent]
remove_from_name = ["-RELEASE", ".WEB."]
use_dots_formatting = true
ignore_torrent_names = ["unknown", "noname"]
"#;
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert_eq!(config.remove_from_name, vec!["-RELEASE", ".WEB."]);
        assert!(config.use_dots_formatting);
        assert_eq!(config.ignore_torrent_names, vec!["unknown", "noname"]);
    }

    #[test]
    fn from_toml_str_parses_recurse_and_skip_existing() {
        let toml = r"
[qtorrent]
recurse = true
skip_existing = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.recurse);
        assert!(config.skip_existing);
    }

    #[test]
    fn from_toml_str_invalid_toml_returns_error() {
        let toml = "this is not valid toml {{{";
        let result = QtorrentConfig::from_toml_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn from_toml_str_ignores_other_sections() {
        let toml = r"
[other_section]
some_value = true

[qtorrent]
verbose = true
";
        let config = QtorrentConfig::from_toml_str(toml).expect("should parse config");
        assert!(config.verbose);
        assert!(!config.dryrun);
    }
}
