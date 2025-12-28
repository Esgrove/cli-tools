//! Main add logic module for qtorrent.
//!
//! Handles the core workflow of parsing torrents and adding them to qBittorrent.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::config::Config;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient};
use crate::torrent::{Torrent, format_size};

/// Main handler for adding torrents to qBittorrent.
pub struct TorrentAdder {
    config: Config,
}

/// Information about a torrent file to be added.
struct TorrentInfo {
    /// Path to the torrent file.
    path: std::path::PathBuf,
    /// Parsed torrent data.
    torrent: Torrent,
    /// Raw torrent file bytes.
    bytes: Vec<u8>,
    /// Suggested output filename (derived from torrent filename).
    suggested_name: String,
}

impl TorrentAdder {
    /// Create a new `TorrentAdder` with the given configuration.
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the main add workflow.
    ///
    /// # Errors
    /// Returns an error if torrents cannot be parsed or added.
    pub async fn run(self) -> Result<()> {
        if self.config.torrent_paths.is_empty() {
            bail!("No torrent files specified");
        }

        // Parse all torrent files first
        let torrents = self.parse_torrents();

        if torrents.is_empty() {
            println!("{}", "No valid single-file torrents to add.".yellow());
            return Ok(());
        }

        // Dry-run mode: just show what would be done
        if self.config.dryrun {
            self.print_dryrun_summary(&torrents);
            return Ok(());
        }

        // Check for credentials before connecting
        if !self.config.has_credentials() {
            bail!(
                "qBittorrent credentials not configured.\n\
                 Set username and password via command line arguments or in config file:\n\
                 ~/.config/cli-tools.toml under [qtorrent] section"
            );
        }

        // Connect to qBittorrent and add torrents one by one
        self.add_torrents_individually(torrents).await
    }

    /// Parse all torrent files and filter to single-file torrents.
    fn parse_torrents(&self) -> Vec<TorrentInfo> {
        let mut torrents = Vec::new();

        for path in &self.config.torrent_paths {
            match Self::parse_single_torrent(path) {
                Ok(Some(info)) => torrents.push(info),
                Ok(None) => {
                    // Multi-file torrent, skipped
                }
                Err(error) => {
                    cli_tools::print_error!("Failed to parse {}: {error}", path.display());
                }
            }
        }

        torrents
    }

    /// Parse a single torrent file.
    /// Returns `None` if the torrent is a multi-file torrent.
    fn parse_single_torrent(path: &Path) -> Result<Option<TorrentInfo>> {
        let bytes = fs::read(path).context("Failed to read torrent file")?;
        let torrent = Torrent::from_buffer(&bytes)?;

        // Skip multi-file torrents
        if torrent.is_multi_file() {
            let name = torrent.name().unwrap_or("Unknown");
            println!("{} Skipping multi-file torrent: {}", "→".yellow(), name.cyan());
            return Ok(None);
        }

        // Get suggested name from the torrent filename (without .torrent extension)
        let suggested_name = Self::get_suggested_name(path, &torrent);

        Ok(Some(TorrentInfo {
            path: path.to_path_buf(),
            torrent,
            bytes,
            suggested_name,
        }))
    }

    /// Get the suggested output filename based on the torrent filename.
    fn get_suggested_name(path: &Path, torrent: &Torrent) -> String {
        // Try to get name from torrent filename first
        let torrent_filename = path.file_stem().and_then(|stem| stem.to_str()).map(ToString::to_string);

        // Get the internal name from the torrent
        let internal_name = torrent.name().map(ToString::to_string);

        // Prefer torrent filename if it differs from internal name
        // (often the torrent filename has better formatting)
        if let Some(filename) = torrent_filename {
            // If internal name exists and has an extension, preserve that extension
            if let Some(ref internal) = internal_name
                && let Some(extension) = Path::new(internal).extension()
            {
                let extension_str = extension.to_string_lossy();
                // Check if filename already has this extension
                if !filename
                    .to_lowercase()
                    .ends_with(&format!(".{}", extension_str.to_lowercase()))
                {
                    return format!("{filename}.{extension_str}");
                }
            }
            return filename;
        }

        // Fall back to internal name
        internal_name.unwrap_or_else(|| "unknown".to_string())
    }

    /// Print dry-run summary of all torrents.
    fn print_dryrun_summary(&self, torrents: &[TorrentInfo]) {
        println!("\n{}", "Torrents to add (dry-run):".bold());
        println!("{}", "─".repeat(60));

        for info in torrents {
            self.print_torrent_info(info);
        }

        println!("\n{}", "─".repeat(60));
        println!("Total: {} torrent(s)", torrents.len());
        self.print_options();
        println!("\n{}", "Dry-run mode: No torrents will be added.".cyan());
    }

    /// Print information about a single torrent.
    fn print_torrent_info(&self, info: &TorrentInfo) {
        let internal_name = info.torrent.name().unwrap_or("Unknown");
        let size = format_size(info.torrent.total_size());

        println!("\n{} {}", "File:".bold(), info.path.display());
        println!("  {} {}", "Internal name:".dimmed(), internal_name);
        println!("  {} {}", "Output name:".dimmed(), info.suggested_name.green());
        println!("  {} {}", "Size:".dimmed(), size);

        if self.config.verbose {
            if let Ok(hash) = info.torrent.info_hash_hex() {
                println!("  {} {}", "Info hash:".dimmed(), hash);
            }
            if let Some(ref announce) = info.torrent.announce {
                println!("  {} {}", "Tracker:".dimmed(), announce);
            }
        }
    }

    /// Print configured options.
    fn print_options(&self) {
        if let Some(ref save_path) = self.config.save_path {
            println!("{} {}", "Save path:".bold(), save_path);
        }
        if let Some(ref category) = self.config.category {
            println!("{} {}", "Category:".bold(), category);
        }
        if self.config.paused {
            println!("{} {}", "State:".bold(), "paused".yellow());
        }
    }

    /// Connect to qBittorrent and add torrents one by one with individual confirmation.
    async fn add_torrents_individually(&self, torrents: Vec<TorrentInfo>) -> Result<()> {
        // Connect to qBittorrent
        println!("{}", "Connecting to qBittorrent...".cyan());
        let mut client = QBittorrentClient::new(&self.config.host, self.config.port);

        client.login(&self.config.username, &self.config.password).await?;

        if self.config.verbose {
            if let Ok(version) = client.get_app_version().await {
                println!("  {} {}", "qBittorrent version:".dimmed(), version);
            }
            if let Ok(api_version) = client.get_api_version().await {
                println!("  {} {}", "API version:".dimmed(), api_version);
            }
        }

        println!("{}\n", "Connected successfully.".green());

        // Process each torrent individually
        let mut success_count = 0;
        let mut skipped_count = 0;
        let mut error_count = 0;
        let total = torrents.len();

        for (index, info) in torrents.into_iter().enumerate() {
            println!("{}", "─".repeat(60));
            println!("{} ({}/{})", "Torrent:".bold(), index + 1, total);
            self.print_torrent_info(&info);

            // Ask for confirmation unless --yes flag is set
            let should_add = if self.config.yes {
                true
            } else {
                cli_tools::confirm_with_user("Add this torrent?", true)
                    .map_err(|error| anyhow::anyhow!("Failed to get confirmation: {error}"))?
            };

            if !should_add {
                println!("{}", "Skipped.".yellow());
                skipped_count += 1;
                continue;
            }

            match self.add_single_torrent(&client, info).await {
                Ok(()) => success_count += 1,
                Err(error) => {
                    error_count += 1;
                    cli_tools::print_error!("{error}");
                }
            }
        }

        // Logout
        if let Err(error) = client.logout().await {
            cli_tools::print_warning!("Failed to logout: {error}");
        }

        // Print final summary
        println!("\n{}", "─".repeat(60));
        println!("{}", "Summary:".bold());
        if success_count > 0 {
            println!("  {} {}", "Added:".green(), success_count);
        }
        if skipped_count > 0 {
            println!("  {} {}", "Skipped:".yellow(), skipped_count);
        }
        if error_count > 0 {
            println!("  {} {}", "Failed:".red(), error_count);
        }

        Ok(())
    }

    /// Add a single torrent to qBittorrent.
    async fn add_single_torrent(&self, client: &QBittorrentClient, info: TorrentInfo) -> Result<()> {
        let params = AddTorrentParams {
            torrent_path: info.path.to_string_lossy().to_string(),
            torrent_bytes: info.bytes,
            save_path: self.config.save_path.clone(),
            category: self.config.category.clone(),
            tags: self.config.tags.clone(),
            rename: Some(info.suggested_name.clone()),
            skip_checking: false,
            paused: self.config.paused,
            root_folder: Some(false), // Don't create subfolder for single-file torrents
            auto_tmm: None,
        };

        client.add_torrent(params).await?;

        println!("  {} Added with name: {}", "✓".green(), info.suggested_name.green());

        Ok(())
    }
}
