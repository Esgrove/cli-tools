//! Main add logic module.
//!
//! Handles the core workflow of parsing torrents and adding them to qBittorrent.
//! Terminal output lives in `display` and the post-add file updates in `file_updates`.

mod display;
mod file_updates;

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use cli_tools::date::RE_CORRECT_DATE_FORMAT;
use cli_tools::dot_rename::{DotFormat, DotRenameConfig};
use cli_tools::print_cyan;

use crate::QtorrentArgs;
use crate::config::Config;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient, TorrentListItem};
use crate::stats::TorrentStats;
use crate::torrent::parse_torrent;
use crate::utils;
use crate::utils::TorrentInfo;

/// Main handler for adding torrents to qBittorrent.
pub struct QTorrent {
    config: Config,
    dot_rename: Option<DotRenameConfig>,
}

impl QTorrent {
    /// Create a new `QTorrent` from command line arguments.
    ///
    /// Loads user configuration and merges it with CLI arguments.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn new(args: QtorrentArgs) -> Result<Self> {
        let config = Config::from_args(args)?;
        let dot_rename = if config.use_dots_formatting {
            Some(DotRenameConfig::from_user_config()?)
        } else {
            None
        };
        Ok(Self { config, dot_rename })
    }

    /// Run the main workflow to add torrents.
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
            println!("{}", "No valid torrents to add".yellow());
            return Ok(());
        }

        // Dry-run mode: show what would be done
        if self.config.dryrun {
            return self.run_dryrun(torrents).await;
        }

        // Connect to qBittorrent and add torrents one by one
        self.add_torrents_individually(torrents).await
    }

    /// Connect to qBittorrent and add torrents one by one with individual confirmation.
    #[allow(clippy::similar_names)]
    #[allow(clippy::too_many_lines)]
    async fn add_torrents_individually(&self, torrents: Vec<TorrentInfo>) -> Result<()> {
        let mut client = self.connect_to_client().await?;

        // Get the list of existing torrents to check for duplicates
        let existing_torrents = client.get_torrent_list().await?;
        if self.config.verbose {
            println!(
                "{} {}",
                "Existing torrents in qBittorrent:".dimmed(),
                existing_torrents.len()
            );
        }

        // Process each torrent individually
        let total = torrents.len();
        let mut stats = TorrentStats::new(total);

        for (index, mut info) in torrents.into_iter().enumerate() {
            if self.config.verbose {
                println!("{}", "─".repeat(60));
            }

            self.print_torrent_info(&info, index + 1, total);

            // Skip torrent when all files are excluded by filters
            if info.all_files_excluded() {
                println!("  {} All files excluded by filters, skipping torrent", "⊘".yellow());
                stats.inc_skipped();
                continue;
            }

            // Determine destination "downloaded" directory once per torrent
            let torrent_path = info.path.clone();
            let downloaded_directory = utils::find_downloaded_directory(&torrent_path);
            let downloaded_collision = downloaded_directory
                .as_deref()
                .and_then(|directory| utils::existing_torrent_in_downloaded(directory, &torrent_path));

            // Check if a torrent already exists in qBittorrent
            if let Some(existing_item) = Self::check_existing_torrent(&info, &existing_torrents) {
                println!(
                    "  {} Already exists in qBittorrent as: {}",
                    "⊘".yellow(),
                    existing_item.name.cyan()
                );

                if let Some(existing) = downloaded_collision.as_deref()
                    && !self.confirm_downloaded_collision(existing, "Continue anyway?")?
                {
                    println!("{}", "Skipped.".yellow());
                    stats.inc_skipped();
                    Self::trash_duplicate_torrent_file(&torrent_path);
                    continue;
                }

                // Offer to rename the existing torrent
                match self.prompt_rename_existing(&info, &existing_item.name, &client).await {
                    Ok(true) => stats.inc_renamed(),
                    Ok(false) => stats.inc_duplicate(),
                    Err(error) => {
                        cli_tools::print_error!("Failed to rename: {error}");
                        stats.inc_duplicate();
                    }
                }

                // Even though we did not add a new torrent, the .torrent file has been processed
                // (it already exists in qBittorrent), so move it into the downloaded directory.
                if let Some(directory) = downloaded_directory.as_deref() {
                    Self::move_torrent_file(&torrent_path, directory);
                }
                continue;
            }

            let add_confirmed_by_collision = if let Some(existing) = downloaded_collision.as_deref() {
                if !self.confirm_downloaded_collision(existing, "Add anyway?")? {
                    println!("{}", "Skipped.".yellow());
                    stats.inc_skipped();
                    Self::trash_duplicate_torrent_file(&torrent_path);
                    continue;
                }
                true
            } else {
                false
            };

            // Offer to rename the output name/folder
            if let Some(new_name) = self.prompt_rename(&info)? {
                info.rename_to = Some(new_name);
            }

            // Print final details before confirmation
            self.print_final_details(&info);

            // Ask for confirmation unless --yes flag is set or the downloaded collision was already confirmed
            let should_add = if self.config.yes || add_confirmed_by_collision {
                true
            } else {
                cli_tools::get_user_confirmation("Add this torrent?", true)
                    .map_err(|error| anyhow::anyhow!("Failed to get confirmation: {error}"))?
            };

            if !should_add {
                println!("{}", "Skipped.".yellow());
                stats.inc_skipped();
                continue;
            }

            match self.add_single_torrent(&client, info).await {
                Ok(size) => {
                    stats.inc_success(size);
                    if let Some(directory) = downloaded_directory.as_deref() {
                        Self::move_torrent_file(&torrent_path, directory);
                    }
                }
                Err(error) => {
                    stats.inc_error();
                    cli_tools::print_error!("{error}");
                }
            }
        }

        // Logout
        if let Err(error) = client.logout().await {
            cli_tools::print_yellow!("Failed to logout: {error}");
        }

        stats.print_summary();

        Ok(())
    }

    /// Add a single torrent to qBittorrent.
    #[allow(clippy::too_many_lines)]
    async fn add_single_torrent(&self, client: &QBittorrentClient, info: TorrentInfo) -> Result<u64> {
        let info_hash = info.info_hash.clone();
        let display_name = info.display_name().into_owned();
        let effective_is_multi_file = info.effective_is_multi_file;
        let excluded_indices = info.excluded_indices.clone();
        let rename_to = info.rename_to.clone();
        let original_name = info.original_name.clone();

        // Use "Original" to preserve torrent structure, or "NoSubfolder" for single files
        let content_layout = if effective_is_multi_file {
            Some("Original".to_string())
        } else {
            Some("NoSubfolder".to_string())
        };

        let params = AddTorrentParams {
            torrent_path: info.path.to_string_lossy().to_string(),
            torrent_bytes: info.bytes,
            save_path: self.config.save_path.clone(),
            category: self.config.category.clone(),
            tags: info.tags,
            rename: rename_to.clone(),
            skip_checking: false,
            paused: self.config.paused,
            content_layout,
        };

        client.add_torrent(params).await?;

        if effective_is_multi_file {
            println!("  {} Added with folder name: {}", "✓".green(), display_name.green());
        } else {
            println!("  {} Added with name: {}", "✓".green(), display_name.green());
        }

        // Rename actual file/folder on disk if a custom name was specified
        let mut folder_renamed = false;
        if let Some(ref new_name) = rename_to
            && let Some(ref old_name) = original_name
            && new_name != old_name
        {
            // Retry with increasing delays - qBittorrent needs time to fully register the torrent
            let delays_ms = [250, 500, 1000];
            let mut last_error = None;

            for delay in &delays_ms {
                tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;

                let rename_result = if effective_is_multi_file {
                    // For multi-file torrents, rename the root folder
                    client.rename_folder(&info_hash, old_name, new_name).await
                } else {
                    // For single-file torrents, rename the file
                    client.rename_file(&info_hash, old_name, new_name).await
                };

                match rename_result {
                    Ok(()) => {
                        println!("  {} Renamed on disk:", "✓".green());
                        cli_tools::show_diff(old_name, new_name);
                        folder_renamed = true;
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                    }
                }
            }

            if !folder_renamed {
                if let Some(error) = last_error {
                    cli_tools::print_yellow!("Could not rename file/folder after retries: {error}");
                }
                println!(
                    "  {} You may need to manually rename in qBittorrent: {} → {}",
                    "⚠".yellow(),
                    old_name,
                    new_name
                );
            }
        }

        // Rename individual files with dot formatting for multi-file torrents
        if effective_is_multi_file && let Some(dot_rename) = self.dot_formatter() {
            // Wait for torrent to be ready if no folder rename was attempted (no prior delay)
            let folder_rename_was_attempted = rename_to
                .as_ref()
                .is_some_and(|new| original_name.as_ref().is_some_and(|old| new != old));
            if !folder_rename_was_attempted {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            self.rename_torrent_files(client, &info_hash, &dot_rename).await;
        }

        // Set file priorities to skip excluded files
        if !excluded_indices.is_empty() {
            let wait_for_metadata = rename_to.is_none() || original_name.is_none();
            self.apply_excluded_file_priorities(client, &info_hash, &excluded_indices, wait_for_metadata)
                .await;
        }

        Ok(info.included_size)
    }

    async fn run_dryrun(self, torrents: Vec<TorrentInfo>) -> Result<()> {
        // Set suggested names on all torrents for display
        let torrents_with_names: Vec<TorrentInfo> = torrents
            .into_iter()
            .map(|mut info| {
                info.rename_to = Some(self.clean_suggested_name(&info));
                info
            })
            .collect();

        // In offline mode, skip qBittorrent connection entirely
        if self.config.offline {
            self.print_dryrun_summary(&torrents_with_names, None);
            return Ok(());
        }

        // Connect to qBittorrent to check for existing torrents
        if !self.config.has_credentials() {
            cli_tools::print_yellow!("No credentials configured. Use --offline to skip qBittorrent connection.");
            self.print_dryrun_summary(&torrents_with_names, None);
            return Ok(());
        }

        match self.connect_to_client().await {
            Ok(mut client) => {
                let existing_torrents = client.get_torrent_list().await.ok();
                self.print_dryrun_summary(&torrents_with_names, existing_torrents.as_ref());

                if let Err(error) = client.logout().await {
                    cli_tools::print_yellow!("Failed to logout: {error}");
                }
            }
            Err(error) => {
                cli_tools::print_yellow!("Failed to connect: {error}");
                self.print_dryrun_summary(&torrents_with_names, None);
            }
        }

        Ok(())
    }

    #[allow(clippy::similar_names)]
    async fn connect_to_client(&self) -> Result<QBittorrentClient> {
        // Check for credentials before connecting
        if !self.config.has_credentials() {
            bail!(
                "qBittorrent credentials not configured.\n\
                 Set username and password via command line arguments or in config file:\n\
                 ~/.config/cli-tools.toml under [qtorrent] section"
            );
        }

        if self.config.verbose {
            print_cyan("Connecting to qBittorrent...");
        }

        let mut client = QBittorrentClient::new(&self.config.host, self.config.port);

        client.login(&self.config.username, &self.config.password).await?;

        // Check connection works by getting app and api version numbers
        let app_version = client.get_app_version().await?;
        let api_version = client.get_api_version().await?;

        if self.config.verbose {
            println!(
                "{} (App {app_version}, API v{api_version})\n",
                "Connected successfully".green()
            );
        }

        Ok(client)
    }

    /// Warn about an existing torrent file in the downloaded directory and ask whether to continue.
    fn confirm_downloaded_collision(&self, existing: &Path, prompt: &str) -> Result<bool> {
        cli_tools::print_yellow!(
            "A torrent file with the same name already exists in the downloaded directory:\n  {}",
            existing.display()
        );

        if self.config.yes {
            return Ok(true);
        }

        cli_tools::get_user_confirmation(prompt, false)
            .map_err(|error| anyhow::anyhow!("Failed to get confirmation: {error}"))
    }

    /// Prompt user to rename an existing torrent in qBittorrent.
    ///
    /// Returns `true` if the torrent was renamed, `false` if skipped.
    async fn prompt_rename_existing(
        &self,
        info: &TorrentInfo,
        existing_name: &str,
        client: &QBittorrentClient,
    ) -> Result<bool> {
        if self.config.yes || self.config.skip_existing {
            // With --yes or --skip-existing flag, skip rename prompt for existing torrents
            return Ok(false);
        }

        let suggested = self.clean_suggested_name(info);
        let internal_formatted = self.clean_internal_name(info);

        // Skip if existing name already matches suggestion
        let matches_suggested = existing_name == suggested;
        let matches_internal = internal_formatted.as_ref().is_some_and(|name| existing_name == name);
        if matches_suggested || matches_internal {
            println!("  {}", "Name already matches suggestion.".dimmed());
            return Ok(false);
        }

        println!(
            "  {} [{}]",
            "Rename existing?".cyan(),
            "press Enter to skip, or type new name".dimmed()
        );
        println!("  {} {}", "1:".dimmed(), suggested.green());
        if let Some(ref internal) = internal_formatted
            && internal != &suggested
        {
            println!("  {} {}", "2:".dimmed(), internal.green());
        }
        print!("  {} ", "Choice or name:".dimmed());
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).context("Failed to read input")?;

        let input = input.trim();
        if input.is_empty() {
            println!("  {}", "Skipped.".dimmed());
            return Ok(false);
        }

        // Check if user entered a number to select an option
        let new_name = match input {
            "1" => suggested,
            "2" if internal_formatted.is_some() => internal_formatted.expect("internal_formatted checked above"),
            _ => self.format_custom_name(input, info.effective_is_multi_file),
        };

        // Rename the existing torrent
        client
            .set_torrent_name(&info.info_hash, &new_name)
            .await
            .context("Failed to set torrent name")?;

        println!("  {} Renamed:", "✓".green());
        cli_tools::show_diff(existing_name, &new_name);

        // Also try to rename the actual file/folder on disk
        if let Some(ref original_name) = info.original_name
            && original_name != &new_name
        {
            // Wait a moment for the rename to take effect
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            let rename_result = if info.effective_is_multi_file {
                client.rename_folder(&info.info_hash, original_name, &new_name).await
            } else {
                client.rename_file(&info.info_hash, original_name, &new_name).await
            };

            match rename_result {
                Ok(()) => {
                    println!(
                        "  {} Renamed on disk: {} → {}",
                        "✓".green(),
                        original_name.dimmed(),
                        new_name.green()
                    );
                }
                Err(error) => {
                    cli_tools::print_yellow!("Could not rename file/folder on disk: {error}");
                }
            }
        }

        Ok(true)
    }

    const fn dot_formatter(&self) -> Option<DotFormat<'_>> {
        if let Some(dot_rename_config) = &self.dot_rename {
            Some(DotFormat::new(dot_rename_config))
        } else {
            None
        }
    }

    /// Get the suggested name with `remove_from_name` substrings removed and dots formatting applied.
    fn clean_suggested_name(&self, info: &TorrentInfo) -> String {
        let mut name = info.suggested_name_raw(&self.config.ignore_torrent_names).into_owned();

        // Remove configured substrings
        for substring in &self.config.remove_from_name {
            name = name.replace(substring, "");
        }

        // Trim any leading/trailing whitespace that might result from removal
        name = name.trim().to_string();

        // Apply dots formatting if enabled
        if let Some(dot_rename) = self.dot_formatter() {
            // Effective multi-file torrents become directories, so use directory naming (spaces instead of dots)
            if info.effective_is_multi_file {
                name = dot_rename.format_directory_name(&name);
            } else {
                name = utils::format_single_file_name(&dot_rename, &name);
            }
        }

        name
    }

    /// Sanitize a custom name entered by the user and apply dots formatting when enabled.
    fn format_custom_name(&self, name: &str, is_multi_file: bool) -> String {
        let name = utils::sanitize_custom_name(name);
        if self.config.format_custom_name
            && let Some(dot_rename) = self.dot_formatter()
        {
            return if is_multi_file {
                dot_rename.format_directory_name(&name)
            } else {
                utils::format_single_file_name(&dot_rename, &name)
            };
        }
        name
    }

    /// Format the internal torrent name with dots formatting applied.
    fn clean_internal_name(&self, info: &TorrentInfo) -> Option<String> {
        let internal_name = info.torrent.name()?;

        let name = self.dot_formatter().map_or_else(
            || internal_name.to_string(),
            |dot_rename| {
                if info.effective_is_multi_file {
                    dot_rename.format_directory_name(internal_name)
                } else {
                    utils::format_single_file_name(&dot_rename, internal_name)
                }
            },
        );

        Some(name)
    }

    /// Parse all torrent files and resolve tags for each.
    fn parse_torrents(&self, torrent_paths: &[PathBuf]) -> Vec<TorrentInfo> {
        torrent_paths
            .iter()
            .filter_map(|path| {
                parse_torrent(path, &self.config)
                    .inspect_err(|error| {
                        cli_tools::print_error!("Failed to parse {}: {error:#}", path.display());
                    })
                    .ok()
            })
            .collect()
    }

    /// Prompt user to rename the output name for a torrent.
    ///
    /// Shows both the suggested name (from torrent filename) and the formatted internal name,
    /// allowing the user to choose or enter a custom name.
    /// Returns `Some(new_name)` if the user wants to rename, `None` to keep original.
    fn prompt_rename(&self, info: &TorrentInfo) -> Result<Option<String>> {
        if self.config.yes {
            // With --yes flag, skip rename prompt
            return Ok(None);
        }

        let label = if info.effective_is_multi_file {
            "Rename folder?"
        } else {
            "Rename file?"
        };

        let suggested = self.clean_suggested_name(info);
        let internal_formatted = self.clean_internal_name(info);

        // Normalize the two options to check if they're effectively the same
        let (normalized_suggested, normalized_internal) =
            Self::normalize_rename_options(&suggested, internal_formatted.as_deref());

        // Determine if we should show the second option
        let show_internal = normalized_internal
            .as_ref()
            .is_some_and(|internal| internal != &normalized_suggested);

        println!(
            "{} [{}]",
            label.cyan(),
            "press Enter to skip, or type new name".dimmed()
        );
        println!("  {} {}", "1:".dimmed(), normalized_suggested.green());
        if let Some(ref internal) = normalized_internal
            && show_internal
        {
            println!("  {} {}", "2:".dimmed(), internal.green());
        }
        print!("  {} ", "Choice or name:".dimmed());
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).context("Failed to read input")?;

        let input = input.trim();
        if input.is_empty() {
            Ok(None)
        } else {
            // Check if user entered a number to select an option
            let selected_name = match input {
                "1" => normalized_suggested,
                "2" if show_internal && normalized_internal.is_some() => {
                    normalized_internal.expect("normalized_internal checked above")
                }
                _ => {
                    let sanitized = utils::sanitize_custom_name(input);
                    // For single files, restore the original extension if the custom name doesn't have one
                    let custom = if info.effective_is_multi_file {
                        sanitized
                    } else {
                        utils::restore_file_extension(&sanitized, &normalized_suggested)
                    };
                    // Apply dots formatting to custom name if configured
                    self.format_custom_name(&custom, info.effective_is_multi_file)
                }
            };

            let new_name_label = if info.effective_is_multi_file {
                "New folder name:"
            } else {
                "New file name:"
            };
            println!("  {} {}", new_name_label.dimmed(), selected_name.green());
            Ok(Some(selected_name))
        }
    }

    /// Normalize two rename options by ensuring both have the same date and extension if applicable.
    ///
    /// If one option has a file extension and the other doesn't, the extension is added.
    /// If one option has a date (yyyy.mm.dd format) and the other doesn't, the date is added.
    ///
    /// Extensions are checked first to avoid date parts (like `.15`) being mistaken for extensions.
    fn normalize_rename_options(suggested: &str, internal: Option<&str>) -> (String, Option<String>) {
        let Some(internal) = internal else {
            return (suggested.to_string(), None);
        };

        let mut normalized_suggested = suggested.to_string();
        let mut normalized_internal = internal.to_string();

        // Extract extensions from both options FIRST (before adding dates)
        // This avoids date parts like ".15" being mistaken for extensions
        let suggested_ext = utils::extract_file_extension(&normalized_suggested);
        let internal_ext = utils::extract_file_extension(&normalized_internal);

        // If one has an extension and the other doesn't, add the extension
        match (&suggested_ext, &internal_ext) {
            (Some(ext), None) => {
                normalized_internal = format!("{normalized_internal}.{ext}");
            }
            (None, Some(ext)) => {
                normalized_suggested = format!("{normalized_suggested}.{ext}");
            }
            _ => {}
        }

        // Extract dates from both options
        let suggested_date = RE_CORRECT_DATE_FORMAT
            .find(&normalized_suggested)
            .map(|m| m.as_str().to_string());
        let internal_date = RE_CORRECT_DATE_FORMAT
            .find(&normalized_internal)
            .map(|m| m.as_str().to_string());

        // If one has a date and the other doesn't, add the date to the one missing it
        match (&suggested_date, &internal_date) {
            (Some(date), None) => {
                // Add date from suggested to internal (insert before extension if present)
                normalized_internal = utils::insert_date_before_extension(&normalized_internal, date);
            }
            (None, Some(date)) => {
                // Add date from internal to suggested (insert before extension if present)
                normalized_suggested = utils::insert_date_before_extension(&normalized_suggested, date);
            }
            _ => {}
        }

        (normalized_suggested, Some(normalized_internal))
    }

    /// Move the torrent file into the downloaded directory and log the outcome.
    ///
    /// Errors are reported as warnings. They do not abort the surrounding workflow.
    fn move_torrent_file(torrent_path: &Path, downloaded_directory: &Path) {
        match utils::move_torrent_to_downloaded(torrent_path, downloaded_directory) {
            Ok(destination) => {
                println!(
                    "  {} Moved torrent file to: {}",
                    "\u{2192}".green(),
                    destination.display().to_string().cyan()
                );
            }
            Err(error) => {
                cli_tools::print_yellow!("Failed to move torrent file to downloaded directory: {error}");
            }
        }
    }

    /// Move a skipped duplicate torrent file to the trash and log the outcome.
    ///
    /// Errors are reported as warnings. They do not abort the surrounding workflow.
    fn trash_duplicate_torrent_file(torrent_path: &Path) {
        match trash::delete(torrent_path) {
            Ok(()) => {
                println!(
                    "  {} Trashed duplicate torrent file: {}",
                    "\u{1f5d1}".yellow(),
                    torrent_path.display().to_string().cyan()
                );
            }
            Err(error) => {
                cli_tools::print_yellow!(
                    "Failed to trash duplicate torrent file {}: {error}",
                    torrent_path.display()
                );
            }
        }
    }

    /// Check if a torrent already exists in qBittorrent by comparing info hashes.
    ///
    /// Returns the existing `TorrentListItem` if found, `None` otherwise.
    fn check_existing_torrent<'a>(
        info: &TorrentInfo,
        existing_torrents: &'a HashMap<String, TorrentListItem>,
    ) -> Option<&'a TorrentListItem> {
        let hash_lower = info.info_hash.to_lowercase();
        existing_torrents.get(&hash_lower)
    }
}

#[cfg(test)]
mod normalize_rename_options {
    use super::*;

    #[test]
    fn both_same_returns_equal() {
        let (suggested, internal) =
            QTorrent::normalize_rename_options("Name.2024.01.15.mp4", Some("Name.2024.01.15.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn no_internal_returns_suggested_only() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name.2024.01.15.mp4", None);
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert!(internal.is_none());
    }

    #[test]
    fn date_added_to_internal_when_missing() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name.2024.01.15.mp4", Some("Name.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn date_added_to_suggested_when_missing() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name.mp4", Some("Name.2024.01.15.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn extension_added_to_internal_when_missing() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name.mp4", Some("Name"));
        assert_eq!(suggested, "Name.mp4");
        assert_eq!(internal.as_deref(), Some("Name.mp4"));
    }

    #[test]
    fn extension_added_to_suggested_when_missing() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name", Some("Name.mp4"));
        assert_eq!(suggested, "Name.mp4");
        assert_eq!(internal.as_deref(), Some("Name.mp4"));
    }

    #[test]
    fn both_date_and_extension_added() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name.2024.01.15.mp4", Some("Name"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2024.01.15.mp4"));
    }

    #[test]
    fn different_dates_remain_different() {
        let (suggested, internal) =
            QTorrent::normalize_rename_options("Name.2024.01.15.mp4", Some("Name.2023.12.25.mp4"));
        assert_eq!(suggested, "Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Name.2023.12.25.mp4"));
    }

    #[test]
    fn different_extensions_remain_different() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Name.mp4", Some("Name.mkv"));
        assert_eq!(suggested, "Name.mp4");
        assert_eq!(internal.as_deref(), Some("Name.mkv"));
    }

    #[test]
    fn directory_names_without_extension() {
        let (suggested, internal) = QTorrent::normalize_rename_options("Show Name 2024.01.15", Some("Show Name"));
        assert_eq!(suggested, "Show Name 2024.01.15");
        assert_eq!(internal.as_deref(), Some("Show Name.2024.01.15"));
    }

    #[test]
    fn extension_added_when_names_differ() {
        // When one has extension and names are different, extension should be added to the other
        let (suggested, internal) =
            QTorrent::normalize_rename_options("Different.Name.2024.01.15.mp4", Some("Other.Name"));
        assert_eq!(suggested, "Different.Name.2024.01.15.mp4");
        assert_eq!(internal.as_deref(), Some("Other.Name.2024.01.15.mp4"));
    }

    #[test]
    fn non_extension_dots_not_treated_as_extension() {
        // "Show.Name" should not have "Name" treated as an extension
        let (suggested, internal) = QTorrent::normalize_rename_options("Show.Name.mp4", Some("Show.Name"));
        assert_eq!(suggested, "Show.Name.mp4");
        assert_eq!(internal.as_deref(), Some("Show.Name.mp4"));
    }

    #[test]
    fn both_without_extension_different_names() {
        // Neither has a real extension, names differ - no extension added
        let (suggested, internal) = QTorrent::normalize_rename_options("Show.Name.One", Some("Show.Name.Two"));
        assert_eq!(suggested, "Show.Name.One");
        assert_eq!(internal.as_deref(), Some("Show.Name.Two"));
    }
}

#[cfg(test)]
mod test_check_existing_torrent {
    use super::*;
    use crate::qbittorrent::TorrentListItem;
    use crate::torrent::Torrent;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Creates a minimal `TorrentListItem` for testing with the given name.
    fn create_torrent_list_item(name: &str) -> TorrentListItem {
        TorrentListItem {
            hash: String::new(),
            name: name.to_string(),
            added_on: 0,
            completion_on: None,
            progress: 0.0,
            ratio: 0.0,
            save_path: String::new(),
            size: 0,
            tags: String::new(),
        }
    }

    /// Creates a minimal `TorrentInfo` for testing with the given info hash.
    fn create_torrent_info_with_hash(info_hash: &str) -> TorrentInfo {
        TorrentInfo {
            path: PathBuf::from("test.torrent"),
            torrent: Torrent::default(),
            bytes: vec![],
            info_hash: info_hash.to_string(),
            original_is_multi_file: false,
            effective_is_multi_file: false,
            rename_to: None,
            included_size: 1000,
            excluded_indices: vec![],
            single_included_file: None,
            original_name: None,
            tags: None,
        }
    }

    #[test]
    fn returns_none_when_map_is_empty() {
        let info = create_torrent_info_with_hash("abc123def456");
        let existing: HashMap<String, TorrentListItem> = HashMap::new();

        let result = QTorrent::check_existing_torrent(&info, &existing);
        assert!(result.is_none());
    }

    #[test]
    fn returns_none_when_hash_not_found() {
        let info = create_torrent_info_with_hash("abc123def456");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert("different_hash".to_string(), create_torrent_list_item("Some Torrent"));

        let result = QTorrent::check_existing_torrent(&info, &existing);
        assert!(result.is_none());
    }

    #[test]
    fn returns_name_when_hash_matches() {
        let info = create_torrent_info_with_hash("abc123def456");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert(
            "abc123def456".to_string(),
            create_torrent_list_item("Existing Torrent Name"),
        );

        let result = QTorrent::check_existing_torrent(&info, &existing);
        assert_eq!(
            result.map(|item| &item.name),
            Some(&"Existing Torrent Name".to_string())
        );
    }

    #[test]
    fn matches_case_insensitively() {
        let info = create_torrent_info_with_hash("ABC123DEF456");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert("abc123def456".to_string(), create_torrent_list_item("Existing Torrent"));

        let result = QTorrent::check_existing_torrent(&info, &existing);
        assert_eq!(result.map(|item| &item.name), Some(&"Existing Torrent".to_string()));
    }

    #[test]
    fn finds_among_multiple_torrents() {
        let info = create_torrent_info_with_hash("target_hash_123");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert("hash_one".to_string(), create_torrent_list_item("Torrent One"));
        existing.insert(
            "target_hash_123".to_string(),
            create_torrent_list_item("Target Torrent"),
        );
        existing.insert("hash_three".to_string(), create_torrent_list_item("Torrent Three"));

        let result = QTorrent::check_existing_torrent(&info, &existing);
        assert_eq!(result.map(|item| &item.name), Some(&"Target Torrent".to_string()));
    }
}

#[cfg(test)]
mod test_add_workflow {
    use clap::Parser;
    use serde_bytes::ByteBuf;
    use tempfile::TempDir;

    use crate::config::QtorrentConfig;
    use crate::qbittorrent::test_server::{Route, route, serve};
    use crate::torrent::{Info, Torrent};

    use super::*;

    /// Directory holding one single-file torrent and an empty `downloaded` directory beside it.
    struct TorrentDirectory {
        _temp_directory: TempDir,
        root: PathBuf,
        torrent_path: PathBuf,
    }

    impl TorrentDirectory {
        fn new() -> Self {
            let temp_directory = TempDir::new().expect("should create temp directory");
            let root = temp_directory.path().join("torrents");
            std::fs::create_dir_all(root.join(utils::DOWNLOADED_DIRECTORY_NAME))
                .expect("should create downloaded directory");
            let mut torrent = Torrent::default();
            torrent.announce = Some("http://tracker.example.com/announce".to_string());
            torrent.info = Info {
                name: Some("Example.Video.mp4".to_string()),
                length: Some(1_000_000),
                piece_length: 262_144,
                pieces: ByteBuf::from(vec![0_u8; 20]),
                ..Info::default()
            };
            let torrent_path = root.join("Example.Video.torrent");
            std::fs::write(
                &torrent_path,
                serde_bencode::to_bytes(&torrent).expect("should serialize torrent"),
            )
            .expect("should write torrent file");
            Self {
                _temp_directory: temp_directory,
                root,
                torrent_path,
            }
        }

        fn moved_torrent_path(&self) -> PathBuf {
            self.root
                .join(utils::DOWNLOADED_DIRECTORY_NAME)
                .join("Example.Video.torrent")
        }

        fn info_hash(&self) -> String {
            parse_torrent(&self.torrent_path, &qtorrent(&["qtorrent"]).config)
                .expect("should parse torrent")
                .info_hash
        }
    }

    fn qtorrent(arguments: &[&str]) -> QTorrent {
        let args = QtorrentArgs::try_parse_from(arguments).expect("arguments should parse");
        QTorrent {
            config: Config::from_args_with_user_config(args, QtorrentConfig::default()),
            dot_rename: None,
        }
    }

    /// `QTorrent` for the directory, connecting to the given local port with credentials and `--yes`.
    fn connected_qtorrent(directory: &TorrentDirectory, port: u16, extra: &[&str]) -> QTorrent {
        let root = directory.root.to_string_lossy().into_owned();
        let port = port.to_string();
        let mut arguments = vec![
            "qtorrent",
            root.as_str(),
            "-H",
            "127.0.0.1",
            "-P",
            port.as_str(),
            "-u",
            "user",
            "-w",
            "password",
            "--yes",
            "--verbose",
        ];
        arguments.extend_from_slice(extra);
        qtorrent(&arguments)
    }

    fn session_routes(torrent_list: String) -> Vec<Route> {
        vec![
            route("/api/v2/auth/login", 200, "Ok."),
            route("/api/v2/auth/logout", 200, ""),
            route("/api/v2/app/version", 200, "v5.0.0"),
            route("/api/v2/app/webapiVersion", 200, "2.11.2"),
            route("/api/v2/torrents/info", 200, torrent_list),
            route("/api/v2/torrents/add", 200, "Ok."),
        ]
    }

    fn torrent_list_with(info_hash: &str) -> String {
        format!(
            r#"[{{"hash":"{info_hash}","name":"Existing Name","added_on":1,"completion_on":-1,"progress":0.0,"ratio":0.0,"save_path":"/downloads","size":1000000,"tags":""}}]"#
        )
    }

    #[tokio::test]
    async fn adds_a_new_torrent_and_moves_the_file_to_downloaded() -> Result<()> {
        let directory = TorrentDirectory::new();
        let port = serve(session_routes("[]".to_string())).await;

        connected_qtorrent(&directory, port, &[]).run().await?;

        assert!(!directory.torrent_path.exists());
        assert!(directory.moved_torrent_path().is_file());
        Ok(())
    }

    #[tokio::test]
    async fn existing_torrent_is_not_added_again_but_the_file_is_moved() -> Result<()> {
        let directory = TorrentDirectory::new();
        let mut routes = session_routes(torrent_list_with(&directory.info_hash()));
        routes.retain(|route| !route.path().starts_with("/api/v2/torrents/add"));
        let port = serve(routes).await;

        connected_qtorrent(&directory, port, &[]).run().await?;

        assert!(directory.moved_torrent_path().is_file());
        Ok(())
    }

    #[tokio::test]
    async fn failed_add_keeps_the_torrent_file_in_place() -> Result<()> {
        let directory = TorrentDirectory::new();
        let mut routes = session_routes("[]".to_string());
        routes.retain(|route| !route.path().starts_with("/api/v2/torrents/add"));
        routes.push(route("/api/v2/torrents/add", 415, ""));
        let port = serve(routes).await;

        connected_qtorrent(&directory, port, &[]).run().await?;

        assert!(directory.torrent_path.is_file());
        assert!(!directory.moved_torrent_path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn dryrun_with_connection_reports_existing_torrents_without_changes() -> Result<()> {
        let directory = TorrentDirectory::new();
        let port = serve(session_routes(torrent_list_with(&directory.info_hash()))).await;

        connected_qtorrent(&directory, port, &["--dryrun"]).run().await?;

        assert!(directory.torrent_path.is_file());
        Ok(())
    }

    #[tokio::test]
    async fn offline_and_unreachable_dryruns_still_print_the_summary() -> Result<()> {
        let directory = TorrentDirectory::new();
        let root = directory.root.to_string_lossy().into_owned();
        let unused_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("should bind a local port");
            listener.local_addr().expect("should have a local address").port()
        };

        qtorrent(&["qtorrent", &root, "--offline", "--verbose"]).run().await?;
        qtorrent(&["qtorrent", &root, "--dryrun"]).run().await?;
        connected_qtorrent(&directory, unused_port, &["--dryrun"]).run().await?;

        assert!(directory.torrent_path.is_file());
        Ok(())
    }

    #[tokio::test]
    async fn adding_without_credentials_or_torrents_fails() {
        let directory = TorrentDirectory::new();
        let root = directory.root.to_string_lossy().into_owned();
        let empty = TempDir::new().expect("should create temp directory");
        let empty_root = empty.path().to_string_lossy().into_owned();

        let credentials_error = qtorrent(&["qtorrent", &root, "--yes"])
            .run()
            .await
            .expect_err("should fail");
        let empty_error = qtorrent(&["qtorrent", &empty_root, "--yes"])
            .run()
            .await
            .expect_err("should fail");

        assert!(credentials_error.to_string().contains("credentials not configured"));
        assert!(empty_error.to_string().contains("No torrent files found"));
    }
}
