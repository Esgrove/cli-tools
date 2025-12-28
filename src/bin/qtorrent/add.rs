//! Main add logic module for qtorrent.
//!
//! Handles the core workflow of parsing torrents and adding them to qBittorrent.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::QtorrentArgs;
use crate::config::Config;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient};
use crate::torrent::{FileFilter, FilteredFiles, Torrent};

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
    /// Suggested output name (derived from torrent filename).
    suggested_name: String,
    /// Whether this is a multi-file torrent.
    is_multi_file: bool,
    /// Filtered files for multi-file torrents.
    filtered_files: Option<FilteredFiles>,
}

impl TorrentAdder {
    /// Create a new `TorrentAdder` from command line arguments.
    ///
    /// Loads user configuration and merges it with CLI arguments.
    #[must_use]
    pub fn new(args: QtorrentArgs) -> Self {
        let config = Config::from_args(args);
        Self { config }
    }

    /// Run the main add workflow.
    ///
    /// # Errors
    /// Returns an error if torrents cannot be parsed or added.
    pub async fn run(self) -> Result<()> {
        // Collect torrent files from input paths
        let torrent_paths = self.config.collect_torrent_paths()?;
        if torrent_paths.is_empty() {
            bail!("No torrent files found");
        }

        // Parse all torrent files first
        let torrents = self.parse_torrents(&torrent_paths);

        if torrents.is_empty() {
            println!("{}", "No valid torrents to add.".yellow());
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

    /// Create a file filter from the config.
    fn create_file_filter(&self) -> FileFilter {
        FileFilter::new(
            self.config.skip_extensions.clone(),
            self.config.skip_names.clone(),
            self.config.min_file_size_bytes,
        )
    }

    /// Parse all torrent files.
    fn parse_torrents(&self, torrent_paths: &[PathBuf]) -> Vec<TorrentInfo> {
        let mut torrents = Vec::new();
        let filter = self.create_file_filter();

        for path in torrent_paths {
            match Self::parse_torrent(path, &filter) {
                Ok(info) => torrents.push(info),
                Err(error) => {
                    cli_tools::print_error!("Failed to parse {}: {error}", path.display());
                }
            }
        }

        torrents
    }

    /// Parse a single torrent file.
    fn parse_torrent(path: &Path, filter: &FileFilter) -> Result<TorrentInfo> {
        let bytes = fs::read(path).context("Failed to read torrent file")?;
        let torrent = Torrent::from_buffer(&bytes)?;

        let is_multi_file = torrent.is_multi_file();

        // Get suggested name from the torrent filename (without .torrent extension)
        let suggested_name = Self::get_suggested_name(path, &torrent);

        // Filter files for multi-file torrents
        let filtered_files = if is_multi_file && !filter.is_empty() {
            Some(torrent.filter_files(filter))
        } else {
            None
        };

        Ok(TorrentInfo {
            path: path.to_path_buf(),
            torrent,
            bytes,
            suggested_name,
            is_multi_file,
            filtered_files,
        })
    }

    /// Get the suggested output name based on the torrent filename.
    fn get_suggested_name(path: &Path, torrent: &Torrent) -> String {
        // Try to get name from torrent filename first
        let torrent_filename = path.file_stem().and_then(|stem| stem.to_str()).map(ToString::to_string);

        // Get the internal name from the torrent
        let internal_name = torrent.name().map(ToString::to_string);

        // For multi-file torrents, this becomes the folder name
        if torrent.is_multi_file() {
            // Prefer torrent filename over internal name
            return torrent_filename
                .or(internal_name)
                .unwrap_or_else(|| "unknown".to_string());
        }

        // For single-file torrents, preserve the file extension
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
        let size = cli_tools::format_size(info.torrent.total_size());

        println!("\n{} {}", "File:".bold(), info.path.display());
        println!("  {} {}", "Internal name:".dimmed(), internal_name);

        if info.is_multi_file {
            println!("  {} {}", "Folder name:".dimmed(), info.suggested_name.green());
            println!("  {} {} files", "Files:".dimmed(), info.torrent.file_count());
        } else {
            println!("  {} {}", "Output name:".dimmed(), info.suggested_name.green());
        }

        println!("  {} {}", "Total size:".dimmed(), size);

        // Show file filtering info for multi-file torrents
        if let Some(ref filtered) = info.filtered_files {
            let included_count = filtered.included.len();
            let excluded_count = filtered.excluded.len();

            if excluded_count > 0 {
                println!(
                    "  {} {} included, {} will be skipped",
                    "Filtered:".dimmed(),
                    format!("{included_count}").green(),
                    format!("{excluded_count}").yellow()
                );
                println!(
                    "  {} {} (skipping {})",
                    "Download size:".dimmed(),
                    cli_tools::format_size(filtered.included_size()).green(),
                    cli_tools::format_size(filtered.excluded_size()).yellow()
                );

                if self.config.verbose {
                    Self::print_file_details(filtered);
                }
            }
        }

        if self.config.verbose {
            if let Ok(hash) = info.torrent.info_hash_hex() {
                println!("  {} {}", "Info hash:".dimmed(), hash);
            }
            if let Some(ref announce) = info.torrent.announce {
                println!("  {} {}", "Tracker:".dimmed(), announce);
            }
        }
    }

    /// Print detailed file information for filtered files.
    fn print_file_details(filtered: &FilteredFiles) {
        if !filtered.excluded.is_empty() {
            println!("\n  {}", "Files to skip:".yellow());
            for file in &filtered.excluded {
                let reason = file.exclusion_reason.as_deref().unwrap_or("unknown reason");
                println!(
                    "    {} {} ({}) - {}",
                    "✗".red(),
                    file.path,
                    cli_tools::format_size(file.size),
                    reason.dimmed()
                );
            }
        }

        if !filtered.included.is_empty() && filtered.included.len() <= 20 {
            println!("\n  {}", "Files to download:".green());
            for file in &filtered.included {
                println!(
                    "    {} {} ({})",
                    "✓".green(),
                    file.path,
                    cli_tools::format_size(file.size)
                );
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
        if self.config.has_file_filters() {
            println!("{}", "File filters:".bold());
            if !self.config.skip_extensions.is_empty() {
                println!(
                    "  {} {}",
                    "Skip extensions:".dimmed(),
                    self.config.skip_extensions.join(", ")
                );
            }
            if !self.config.skip_names.is_empty() {
                println!("  {} {}", "Skip names:".dimmed(), self.config.skip_names.join(", "));
            }
            if let Some(min_size) = self.config.min_file_size_bytes {
                println!("  {} {} MB", "Min file size:".dimmed(), min_size / (1024 * 1024));
            }
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

        for (index, mut info) in torrents.into_iter().enumerate() {
            println!("{}", "─".repeat(60));
            println!("{} ({}/{})", "Torrent:".bold(), index + 1, total);
            self.print_torrent_info(&info);

            // For multi-file torrents, offer to rename the folder
            if info.is_multi_file
                && let Some(new_name) = self.prompt_folder_rename(&info.suggested_name)?
            {
                info.suggested_name = new_name;
            }

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

    /// Prompt user to rename the folder for a multi-file torrent.
    fn prompt_folder_rename(&self, _current_name: &str) -> Result<Option<String>> {
        if self.config.yes {
            return Ok(None);
        }

        print!(
            "  {} [{}]: ",
            "Rename folder?".cyan(),
            "press Enter to keep, or type new name".dimmed()
        );
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).context("Failed to read input")?;

        let input = input.trim();
        if input.is_empty() {
            Ok(None)
        } else {
            println!("  {} {}", "New folder name:".dimmed(), input.green());
            Ok(Some(input.to_string()))
        }
    }

    /// Add a single torrent to qBittorrent.
    async fn add_single_torrent(&self, client: &QBittorrentClient, info: TorrentInfo) -> Result<()> {
        let info_hash = info.torrent.info_hash_hex()?;

        let params = AddTorrentParams {
            torrent_path: info.path.to_string_lossy().to_string(),
            torrent_bytes: info.bytes,
            save_path: self.config.save_path.clone(),
            category: self.config.category.clone(),
            tags: self.config.tags.clone(),
            rename: Some(info.suggested_name.clone()),
            skip_checking: false,
            paused: self.config.paused,
            root_folder: Some(!info.is_multi_file), // true for multi-file to keep folder structure
            auto_tmm: None,
        };

        client.add_torrent(params).await?;

        if info.is_multi_file {
            println!(
                "  {} Added with folder name: {}",
                "✓".green(),
                info.suggested_name.green()
            );
        } else {
            println!("  {} Added with name: {}", "✓".green(), info.suggested_name.green());
        }

        // Set file priorities to skip excluded files
        if let Some(ref filtered) = info.filtered_files {
            let excluded_indices = filtered.excluded_indices();
            if !excluded_indices.is_empty() {
                // Wait a moment for the torrent to be fully added
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                match client.set_file_priorities(&info_hash, &excluded_indices, 0).await {
                    Ok(()) => {
                        println!("  {} Set {} file(s) to skip", "✓".green(), excluded_indices.len());
                    }
                    Err(error) => {
                        cli_tools::print_warning!(
                            "Could not set file priorities (torrent may still be loading): {error}"
                        );
                        println!(
                            "  {} You may need to manually skip {} file(s) in qBittorrent",
                            "⚠".yellow(),
                            excluded_indices.len()
                        );
                    }
                }
            }
        }

        Ok(())
    }
}
