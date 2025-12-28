//! Main add logic module for qtorrent.
//!
//! Handles the core workflow of parsing torrents and adding them to qBittorrent.

use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use cli_tools::dot_rename::DotRename;

use crate::QtorrentArgs;
use crate::config::Config;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient};
use crate::torrent::{FileFilter, FilteredFiles, Torrent};

/// Main handler for adding torrents to qBittorrent.
pub struct QTorrent {
    config: Config,
    /// Optional `DotRename` instance for formatting names (when `use_dots_formatting` is enabled).
    dot_rename: Option<DotRename>,
}

/// Information about a torrent file to be added.
struct TorrentInfo {
    /// Path to the torrent file.
    path: std::path::PathBuf,
    /// Parsed torrent data.
    torrent: Torrent,
    /// Raw torrent file bytes.
    bytes: Vec<u8>,
    /// Whether this is a multi-file torrent.
    is_multi_file: bool,
    /// Custom name to rename to (None = use torrent's internal name).
    rename_to: Option<String>,
    /// Indices of files to exclude (for setting priority to 0).
    excluded_indices: Vec<usize>,
}

impl TorrentInfo {
    /// Get the display name for this torrent (`rename_to` or internal name).
    #[allow(clippy::option_if_let_else)]
    fn display_name(&self) -> Cow<'_, str> {
        if let Some(ref name) = self.rename_to {
            Cow::Borrowed(name.as_str())
        } else if let Some(name) = self.torrent.name() {
            Cow::Borrowed(name)
        } else {
            Cow::Borrowed("unknown")
        }
    }

    /// Get the suggested name derived from the torrent filename.
    ///
    /// This returns the raw name without any filtering applied.
    /// Use `clean_suggested_name` to apply `remove_from_name` filtering.
    #[allow(clippy::option_if_let_else)]
    fn suggested_name_raw(&self) -> Cow<'_, str> {
        // Try to get name from torrent filename first
        let torrent_filename = self.path.file_stem().and_then(|stem| stem.to_str());

        // Get the internal name from the torrent
        let internal_name = self.torrent.name();

        // For multi-file torrents, this becomes the folder name
        if self.is_multi_file {
            // Prefer torrent filename over internal name
            return if let Some(name) = torrent_filename {
                Cow::Borrowed(name)
            } else if let Some(name) = internal_name {
                Cow::Borrowed(name)
            } else {
                Cow::Borrowed("unknown")
            };
        }

        // For single-file torrents, preserve the file extension
        if let Some(filename) = torrent_filename {
            // If internal name exists and has an extension, preserve that extension
            if let Some(internal) = internal_name
                && let Some(extension) = Path::new(internal).extension()
            {
                let extension_str = extension.to_string_lossy();
                // Check if filename already has this extension
                if !filename
                    .to_lowercase()
                    .ends_with(&format!(".{}", extension_str.to_lowercase()))
                {
                    return Cow::Owned(format!("{filename}.{extension_str}"));
                }
            }
            return Cow::Borrowed(filename);
        }

        // Fall back to internal name
        if let Some(name) = internal_name {
            Cow::Borrowed(name)
        } else {
            Cow::Borrowed("unknown")
        }
    }
}

impl QTorrent {
    /// Get the suggested name with `remove_from_name` substrings removed and dots formatting applied.
    fn clean_suggested_name(&self, info: &TorrentInfo) -> String {
        let mut name = info.suggested_name_raw().into_owned();

        // Remove configured substrings
        for substring in &self.config.remove_from_name {
            name = name.replace(substring, "");
        }

        // Trim any leading/trailing whitespace that might result from removal
        name = name.trim().to_string();

        // Apply dots formatting if enabled
        if let Some(ref dot_rename) = self.dot_rename {
            name = dot_rename.format_name(&name);
        }

        name
    }

    /// Create a new `TorrentAdder` from command line arguments.
    ///
    /// Loads user configuration and merges it with CLI arguments.
    #[must_use]
    pub fn new(args: QtorrentArgs) -> Self {
        let config = Config::from_args(args);
        let dot_rename = if config.use_dots_formatting {
            Some(DotRename::for_name_formatting())
        } else {
            None
        };
        Self { config, dot_rename }
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
    fn create_file_filter(&self) -> FileFilter<'_> {
        FileFilter::new(
            &self.config.skip_extensions,
            &self.config.skip_names,
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
    fn parse_torrent(path: &Path, filter: &FileFilter<'_>) -> Result<TorrentInfo> {
        let bytes = fs::read(path).context("Failed to read torrent file")?;
        let torrent = Torrent::from_buffer(&bytes)?;

        let is_multi_file = torrent.is_multi_file();

        // Filter files for multi-file torrents and collect excluded indices
        let excluded_indices = if is_multi_file && !filter.is_empty() {
            torrent
                .filter_files(filter)
                .excluded
                .iter()
                .map(|file| file.index)
                .collect()
        } else {
            Vec::new()
        };

        Ok(TorrentInfo {
            path: path.to_path_buf(),
            torrent,
            bytes,
            is_multi_file,
            rename_to: None,
            excluded_indices,
        })
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
            println!("  {}  {}", "Folder name:".dimmed(), info.display_name().green());
            self.print_multi_file_info(info);
        } else {
            println!("  {}  {}", "Output name:".dimmed(), info.display_name().green());
            println!("  {}   {}", "Total size:".dimmed(), size);
        }
    }

    /// Print file information for multi-file torrents.
    fn print_multi_file_info(&self, info: &TorrentInfo) {
        let filter = self.create_file_filter();
        let filtered = info.torrent.filter_files(&filter);
        let included_count = filtered.included.len();
        let excluded_count = filtered.excluded.len();
        let total_count = included_count + excluded_count;

        // Always show file counts
        if excluded_count > 0 {
            println!(
                "  {} {} ({} included, {} skipped)",
                "Files:".dimmed(),
                total_count,
                format!("{included_count}").green(),
                format!("{excluded_count}").yellow()
            );
            println!(
                "  {} {} (skipping {})",
                "Download size:".dimmed(),
                cli_tools::format_size(filtered.included_size()).green(),
                cli_tools::format_size(filtered.excluded_size()).yellow()
            );
        } else {
            println!("  {} {}", "Files:".dimmed(), total_count);
            println!(
                "  {} {}",
                "Total size:".dimmed(),
                cli_tools::format_size(filtered.included_size())
            );
        }

        // In verbose mode, show all files sorted by size (largest first)
        if self.config.verbose {
            Self::print_all_files_sorted(&filtered);
        }
    }

    /// Print all files sorted by size (largest first), showing include/exclude status.
    fn print_all_files_sorted(filtered: &FilteredFiles<'_>) {
        // Combine all files with their status
        let mut all_files: Vec<_> = filtered
            .included
            .iter()
            .map(|file| (file, true))
            .chain(filtered.excluded.iter().map(|file| (file, false)))
            .collect();

        // Sort by size descending
        all_files.sort_by(|a, b| b.0.size.cmp(&a.0.size));

        println!("\n  {}", "Files:".bold());
        for (file, included) in all_files {
            if included {
                println!(
                    "    {} {} ({})",
                    "✓".green(),
                    file.path,
                    cli_tools::format_size(file.size)
                );
            } else {
                let reason = file.exclusion_reason.as_deref().unwrap_or("excluded");
                println!(
                    "    {} {} ({}) - {}",
                    "✗".red(),
                    file.path,
                    cli_tools::format_size(file.size),
                    reason.dimmed()
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

            // Offer to rename the output name/folder
            if let Some(new_name) = self.prompt_rename(&info)? {
                info.rename_to = Some(new_name);
            }

            // Print final details before confirmation
            self.print_final_details(&info);

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

    /// Print final details about the torrent before confirmation.
    fn print_final_details(&self, info: &TorrentInfo) {
        println!();
        println!("  {}", "Will add with:".bold());

        let name_label = if info.is_multi_file {
            "Folder name:"
        } else {
            "Output name:"
        };
        println!("    {} {}", name_label.dimmed(), info.display_name().green());

        if let Some(ref save_path) = self.config.save_path {
            println!("    {} {}", "Save path:".dimmed(), save_path);
        }
        if let Some(ref category) = self.config.category {
            println!("    {} {}", "Category:".dimmed(), category);
        }
        if let Some(ref tags) = self.config.tags {
            println!("    {} {}", "Tags:".dimmed(), tags);
        }
        if self.config.paused {
            println!("    {} {}", "State:".dimmed(), "paused".yellow());
        }
        if !info.excluded_indices.is_empty() {
            println!(
                "    {} {}",
                "Files to skip:".dimmed(),
                format!("{}", info.excluded_indices.len()).yellow()
            );
        }
        println!();
    }

    /// Prompt user to rename the output name for a torrent.
    ///
    /// Shows the suggested name and allows the user to modify it.
    /// Returns `Some(new_name)` if the user wants to rename, `None` to keep original.
    fn prompt_rename(&self, info: &TorrentInfo) -> Result<Option<String>> {
        if self.config.yes {
            // With --yes flag, skip rename prompt
            return Ok(None);
        }

        let label = if info.is_multi_file {
            "Rename folder?"
        } else {
            "Rename file?"
        };

        let suggested = self.clean_suggested_name(info);

        println!(
            "  {} [{}]",
            label.cyan(),
            "press Enter to skip, or type new name".dimmed()
        );
        print!("  {} ", format!("({suggested}):").dimmed());
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).context("Failed to read input")?;

        let input = input.trim();
        if input.is_empty() {
            Ok(None)
        } else {
            let new_name = if info.is_multi_file {
                "New folder name:"
            } else {
                "New file name:"
            };
            println!("  {} {}", new_name.dimmed(), input.green());
            Ok(Some(input.to_string()))
        }
    }

    /// Add a single torrent to qBittorrent.
    async fn add_single_torrent(&self, client: &QBittorrentClient, info: TorrentInfo) -> Result<()> {
        let info_hash = info.torrent.info_hash_hex()?;
        let display_name = info.display_name().into_owned();
        let is_multi_file = info.is_multi_file;
        let excluded_indices = info.excluded_indices;

        let params = AddTorrentParams {
            torrent_path: info.path.to_string_lossy().to_string(),
            torrent_bytes: info.bytes,
            save_path: self.config.save_path.clone(),
            category: self.config.category.clone(),
            tags: self.config.tags.clone(),
            rename: info.rename_to,
            skip_checking: false,
            paused: self.config.paused,
            root_folder: is_multi_file,
        };

        client.add_torrent(params).await?;

        if is_multi_file {
            println!("  {} Added with folder name: {}", "✓".green(), display_name.green());
        } else {
            println!("  {} Added with name: {}", "✓".green(), display_name.green());
        }

        // Set file priorities to skip excluded files
        if !excluded_indices.is_empty() {
            // Wait a moment for the torrent to be fully added
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            match client.set_file_priorities(&info_hash, &excluded_indices, 0).await {
                Ok(()) => {
                    println!("  {} Set {} file(s) to skip", "✓".green(), excluded_indices.len());
                }
                Err(error) => {
                    cli_tools::print_warning!("Could not set file priorities (torrent may still be loading): {error}");
                    println!(
                        "  {} You may need to manually skip {} file(s) in qBittorrent",
                        "⚠".yellow(),
                        excluded_indices.len()
                    );
                }
            }
        }

        Ok(())
    }
}
