//! Main add logic module.
//!
//! Handles the core workflow of parsing torrents and adding them to qBittorrent.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use cli_tools::date::RE_CORRECT_DATE_FORMAT;
use cli_tools::dot_rename::{DotFormat, DotRenameConfig};
use cli_tools::{print_bold, print_cyan, print_magenta_bold};

use crate::QtorrentArgs;
use crate::config::Config;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient, TorrentListItem};
use crate::stats::TorrentStats;
use crate::torrent::{FileInfo, FilteredFiles, parse_torrent};
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
                println!("  {} All files excluded by filters, skipping torrent", "⊘".yellow(),);
                stats.inc_skipped();
                continue;
            }

            // Check if a torrent already exists in qBittorrent
            if let Some(existing_item) = Self::check_existing_torrent(&info, &existing_torrents) {
                println!(
                    "  {} Already exists in qBittorrent as: {}",
                    "⊘".yellow(),
                    existing_item.name.cyan()
                );

                // Offer to rename the existing torrent
                match self.prompt_rename_existing(&info, &existing_item.name, &client).await {
                    Ok(true) => stats.inc_renamed(),
                    Ok(false) => stats.inc_duplicate(),
                    Err(error) => {
                        cli_tools::print_error!("Failed to rename: {error}");
                        stats.inc_duplicate();
                    }
                }
                continue;
            }

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
                cli_tools::get_user_confirmation("Add this torrent?", true)
                    .map_err(|error| anyhow::anyhow!("Failed to get confirmation: {error}"))?
            };

            if !should_add {
                println!("{}", "Skipped.".yellow());
                stats.inc_skipped();
                continue;
            }

            match self.add_single_torrent(&client, info).await {
                Ok(size) => stats.inc_success(size),
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
                        println!("  {} Renamed on disk:", "✓".green(),);
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

            self.rename_torrent_files(client, &info_hash, &excluded_indices, &dot_rename)
                .await;
        }

        // Set file priorities to skip excluded files
        if !excluded_indices.is_empty() {
            // Wait a moment for the torrent to be fully added (if we haven't already waited for rename)
            if rename_to.is_none() || original_name.is_none() {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            let mut priority_success = false;
            let mut last_error = None;

            // Try twice with a delay between attempts
            for attempt in 0..2 {
                if attempt > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                }

                match client.set_file_priorities(&info_hash, &excluded_indices, 0).await {
                    Ok(()) => {
                        println!("  {} Set {} file(s) to skip", "✓".green(), excluded_indices.len());
                        priority_success = true;
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                    }
                }
            }

            if !priority_success {
                if let Some(error) = last_error {
                    cli_tools::print_yellow!("Could not set file priorities (torrent may still be loading): {error}");
                }
                println!(
                    "  {} You may need to manually skip {} file(s) in qBittorrent",
                    "⚠".yellow(),
                    excluded_indices.len()
                );
            }
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
            _ => input.to_string(),
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
                        cli_tools::print_error!("Failed to parse {}: {error}", path.display());
                    })
                    .ok()
            })
            .collect()
    }

    /// Print dry-run summary of all torrents.
    fn print_dryrun_summary(
        &self,
        torrents: &[TorrentInfo],
        existing_torrents: Option<&HashMap<String, TorrentListItem>>,
    ) {
        let total = torrents.len();

        // Count how many would be skipped as duplicates
        let duplicate_count = existing_torrents.map_or(0, |existing| {
            torrents
                .iter()
                .filter(|info| Self::check_existing_torrent(info, existing).is_some())
                .count()
        });

        // Count how many would be skipped because all files are excluded by filters
        let all_excluded_count = torrents.iter().filter(|info| info.all_files_excluded()).count();

        let skipped_total = duplicate_count + all_excluded_count;
        let mode_label = if self.config.offline { "OFFLINE" } else { "DRYRUN" };

        if skipped_total > 0 {
            print_bold!(
                "{mode_label} {} torrents, {} to add, {duplicate_count} skipped, excluded {all_excluded_count}:",
                torrents.len(),
                torrents.len() - skipped_total,
            );
        } else {
            print_bold!("{mode_label} {} torrents to add:", torrents.len());
        }

        if self.config.verbose {
            self.print_options();
        }

        for (index, info) in torrents.iter().enumerate() {
            self.print_torrent_info(info, index + 1, total);

            // Show if all files are excluded by filters
            if info.all_files_excluded() {
                println!("  {} All files excluded by filters, skipping torrent", "⊘".yellow());
            }

            // Show if this torrent already exists
            if let Some(existing) = existing_torrents
                && let Some(existing_item) = Self::check_existing_torrent(info, existing)
            {
                println!("  {} Already exists as: {}", "⊘".yellow(), existing_item.name.cyan());
            }
        }
    }

    /// Print information about a single torrent.
    ///
    /// The index is displayed as `[index/total]` with the index right-aligned to match the width of the total count.
    fn print_torrent_info(&self, info: &TorrentInfo, index: usize, total: usize) {
        let internal_name = info.torrent.name().unwrap_or("Unknown");
        let size = cli_tools::format_size(info.torrent.total_size());
        let width = total.to_string().chars().count();

        print_magenta_bold!(
            "\n[{index:>width$}/{total}] {}",
            cli_tools::path_to_string_relative(&info.path)
        );
        println!("  {}          {}", "Name:".dimmed(), internal_name);
        if let Some(comment) = &info.torrent.comment
            && !comment.is_empty()
        {
            println!("  {}       {}", "Comment:".dimmed(), comment);
        }
        if self.config.verbose {
            println!("  {}     {}", "Info hash:".dimmed(), info.info_hash);
        }
        if info.original_is_multi_file {
            // Show folder name if treating as multi-file or if all files were excluded
            if info.effective_is_multi_file || info.all_files_excluded() {
                println!("  {}   {}", "Folder name:".dimmed(), info.display_name().green());
            } else {
                println!("  {}     {}", "File name:".dimmed(), info.display_name().green());
            }
            self.print_multi_file_info(info);
        } else {
            println!("  {}    {}", "File name:".dimmed(), info.display_name().green());
            println!("  {}   {}", "Total size:".dimmed(), size);
        }
    }

    /// Print file information for multi-file torrents.
    fn print_multi_file_info(&self, info: &TorrentInfo) {
        let total_count = info.torrent.files().len();
        let excluded_count = info.excluded_indices.len();
        let included_count = total_count - excluded_count;
        let included_size = info.included_size;
        let total_size = info.torrent.total_size();
        let excluded_size = total_size - included_size;

        // Always show file counts
        if excluded_count > 0 {
            println!(
                "  {}         {} ({} included, {} skipped)",
                "Files:".dimmed(),
                total_count,
                format!("{included_count}").green(),
                format!("{excluded_count}").yellow()
            );
            println!(
                "  {} {} (skipping {})",
                "Download size:".dimmed(),
                cli_tools::format_size(included_size).green(),
                cli_tools::format_size(excluded_size).yellow()
            );
        } else {
            println!("  {} {}", "Files:".dimmed(), total_count);
            println!("  {} {}", "Total size:".dimmed(), cli_tools::format_size(included_size));
        }

        // In verbose mode, show all files sorted by size (largest first)
        if self.config.verbose {
            let filtered = info.torrent.filter_files(&self.config.file_filter);
            self.print_all_files_sorted(&filtered);
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
        if let Some(ref tags) = self.config.tags {
            println!("{} {}", "Tags:".bold(), tags);
        }

        println!(
            "{} {}",
            "State:".bold(),
            if self.config.paused {
                "paused".yellow()
            } else {
                "active".green()
            }
        );

        if !self.config.file_filter.is_empty() {
            println!("{}", "File filters:".bold());
            if !self.config.file_filter.skip_extensions.is_empty() {
                println!(
                    "  {} {}",
                    "Skip extensions:".dimmed(),
                    self.config.file_filter.skip_extensions.join(", ")
                );
            }
            if !self.config.file_filter.skip_directories.is_empty() {
                println!(
                    "  {} {}",
                    "Skip directories:".dimmed(),
                    self.config.file_filter.skip_directories.join(", ")
                );
            }
            if let Some(min_size_mb) = self.config.file_filter.min_size_mb {
                println!("  {} {} MB", "Min file size:".dimmed(), min_size_mb);
            }
        }
    }

    /// Print final details about the torrent before confirmation.
    fn print_final_details(&self, info: &TorrentInfo) {
        println!();
        println!("  {}", "Will add with:".bold());

        let name_label = if info.effective_is_multi_file {
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
        if let Some(ref tags) = info.tags {
            println!("    {} {}", "Tags:".dimmed(), tags);
        }
        if self.config.paused {
            println!("    {} {}", "State:".dimmed(), "paused".yellow());
        }
        let total_count = info.torrent.files().len();
        let skipped_count = info.excluded_indices.len();
        let included_count = total_count - skipped_count;
        if included_count > 1 {
            if skipped_count > 0 {
                println!(
                    "    {} {included_count} / {total_count} ({} skipped)",
                    "Files:".dimmed(),
                    format!("{skipped_count}").yellow()
                );
            } else {
                println!("    {} {total_count}", "Files:".dimmed());
            }
        }
        println!();
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
                _ => input.to_string(),
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

    /// Print all files sorted by size (largest first), showing include/exclude status.
    ///
    /// Files excluded due to directory matching are grouped by directory name
    /// instead of listing each file individually.
    fn print_all_files_sorted(&self, filtered: &FilteredFiles<'_>) {
        use crate::utils::SkippedDirectorySummary;
        use std::collections::HashMap;

        let dot_formatter = self.dot_formatter();

        // Group excluded files by directory if they were excluded due to directory matching
        let mut skipped_directories: HashMap<String, SkippedDirectorySummary> = HashMap::new();
        let mut other_excluded: Vec<&FileInfo<'_>> = Vec::new();

        for file in &filtered.excluded {
            if let Some(ref reason) = file.exclusion_reason {
                if reason.starts_with("directory: ") {
                    let dir_name = reason.trim_start_matches("directory: ").to_string();
                    skipped_directories.entry(dir_name).or_default().add_file(file.size);
                } else {
                    other_excluded.push(file);
                }
            } else {
                other_excluded.push(file);
            }
        }

        // Collect all items to display: included files, other excluded files, and directory summaries
        // Sort included and other excluded files by size descending
        let mut included_files: Vec<_> = filtered.included.iter().collect();
        included_files.sort_by(|a, b| b.size.cmp(&a.size));

        other_excluded.sort_by(|a, b| b.size.cmp(&a.size));

        // Sort skipped directories by total size descending
        let mut skipped_dirs_sorted: Vec<_> = skipped_directories.into_iter().collect();
        skipped_dirs_sorted.sort_by(|a, b| b.1.total_size.cmp(&a.1.total_size));

        // Find the widest formatted size string for right-alignment
        let all_sizes: Vec<String> = included_files
            .iter()
            .map(|file| cli_tools::format_size(file.size))
            .chain(other_excluded.iter().map(|file| cli_tools::format_size(file.size)))
            .chain(
                skipped_dirs_sorted
                    .iter()
                    .map(|(_, summary)| cli_tools::format_size(summary.total_size)),
            )
            .collect();
        let max_size_width = all_sizes.iter().map(String::len).max().unwrap_or(0);

        println!("  {}", "Files:".bold());

        // Print included files
        let mut size_index = 0;
        for file in included_files {
            // Show the final file name after dot formatting (if configured)
            let display_path = dot_formatter
                .as_ref()
                .and_then(|dot_rename| {
                    let path = Path::new(file.path.as_ref());
                    let filename = path.file_name()?.to_str()?;
                    let formatted = utils::format_single_file_name(dot_rename, filename);
                    if formatted == filename {
                        return None;
                    }
                    let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");
                    if parent.is_empty() {
                        Some(formatted)
                    } else {
                        Some(format!("{parent}/{formatted}"))
                    }
                })
                .unwrap_or_else(|| file.path.to_string());

            let size_str = &all_sizes[size_index];
            size_index += 1;
            let check = "✓".green();
            println!("    {size_str:>max_size_width$}  {check} {display_path}");
        }

        // Print other excluded files (not from directory matching)
        for file in other_excluded {
            let reason = file.exclusion_reason.as_deref().unwrap_or("excluded");
            let size_str = &all_sizes[size_index];
            size_index += 1;
            let path = &file.path;
            let reason = reason.dimmed();
            let cross = "✗".red();
            println!("    {size_str:>max_size_width$}  {cross} {path} - {reason}");
        }

        // Print skipped directory summaries
        for (dir_name, summary) in skipped_dirs_sorted {
            let size_str = &all_sizes[size_index];
            size_index += 1;
            let ellipsis = "...".dimmed();
            let file_count = summary.file_count;
            let files_word = summary.files_word();
            let cross = "✗".red();
            let reason = format!("directory: {dir_name}").dimmed();
            println!(
                "    {size_str:>max_size_width$}  {cross} {dir_name}/{ellipsis} ({file_count} {files_word}) - {reason}"
            );
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

    /// Rename individual files within a multi-file torrent using dot formatting.
    ///
    /// Queries the qBittorrent API for the actual file paths (which reflect any folder renames
    /// that have already been applied), then renames each included file with dot formatting.
    /// Retries with a fresh file list if renames fail (paths can change asynchronously after
    /// a folder rename propagates).
    async fn rename_torrent_files(
        &self,
        client: &QBittorrentClient,
        info_hash: &str,
        excluded_indices: &[usize],
        dot_rename: &DotFormat<'_>,
    ) {
        print_cyan("Renaming files...");
        let max_attempts = 3;
        let mut total_renamed: usize = 0;
        // Track indices that still need renaming
        let mut pending_indices: Option<Vec<usize>> = None;

        for attempt in 0..max_attempts {
            if attempt > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            // Fetch the current file list from qBittorrent
            let api_files = match client.get_torrent_files(info_hash).await {
                Ok(files) => files,
                Err(error) => {
                    cli_tools::print_yellow!("Could not get file list for dot-renaming: {error}");
                    return;
                }
            };

            let mut attempt_renamed: usize = 0;
            let mut failed_indices: Vec<usize> = Vec::new();

            for file in &api_files {
                if excluded_indices.contains(&file.index) {
                    continue;
                }

                // On retries, only process previously failed indices
                if let Some(ref pending) = pending_indices
                    && !pending.contains(&file.index)
                {
                    continue;
                }

                // The API returns the full path as qBittorrent sees it (including root folder)
                let old_path = &file.name;
                let path = Path::new(old_path.as_str());

                let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
                    continue;
                };

                // Apply dot formatting to the filename
                let formatted = utils::format_single_file_name(dot_rename, filename);

                if formatted == filename {
                    continue;
                }

                // Build new path by replacing only the filename portion
                let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");
                let new_path = if parent.is_empty() {
                    formatted.clone()
                } else {
                    format!("{parent}/{formatted}")
                };

                match client.rename_file(info_hash, old_path, &new_path).await {
                    Ok(()) => {
                        attempt_renamed += 1;
                        if self.config.verbose {
                            println!("    {} → {}", filename.dimmed(), formatted.green());
                        }
                    }
                    Err(error) => {
                        failed_indices.push(file.index);
                        if self.config.verbose && attempt == max_attempts - 1 {
                            cli_tools::print_yellow!("    Could not rename {filename}: {error}");
                        }
                    }
                }
            }

            total_renamed += attempt_renamed;

            if failed_indices.is_empty() {
                break;
            }

            pending_indices = Some(failed_indices);
        }

        if total_renamed > 0 {
            println!(
                "  {} Renamed {} file(s) with dot formatting",
                "✓".green(),
                total_renamed
            );
        }
        if let Some(ref pending) = pending_indices
            && !pending.is_empty()
            && !self.config.verbose
        {
            let count = pending.len();
            cli_tools::print_yellow!("  Failed to dot-rename {count} file(s) (use --verbose for details)");
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
