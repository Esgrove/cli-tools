//! Main add logic module for qtorrent.
//!
//! Handles the core workflow of parsing torrents and adding them to qBittorrent.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use cli_tools::date::RE_CORRECT_DATE_FORMAT;
use cli_tools::dot_rename::{DotFormat, DotRenameConfig};
use cli_tools::{print_bold, print_magenta_bold};

use crate::QtorrentArgs;
use crate::config::Config;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient};
use crate::torrent::{FileFilter, FileInfo, FilteredFiles, Torrent};

// List of known media file extensions
const KNOWN_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "mp3", "flac", "wav", "aac", "ogg",
    "wma", "m4a", "opus", "alac", "rar", "zip", "7z", "tar", "gz", "bz2", "xz", "srt", "sub", "jpg", "jpeg", "png",
    "gif", "bmp", "webp", "tiff", "tif", "pdf", "epub", "mobi",
];

/// Main handler for adding torrents to qBittorrent.
pub struct QTorrent {
    config: Config,
    dot_rename: Option<DotRenameConfig>,
}

/// Summary of files skipped due to directory matching.
#[derive(Debug, Default)]
struct SkippedDirectorySummary {
    /// Number of files in the skipped directory.
    file_count: usize,
    /// Total size of all files in the skipped directory.
    total_size: u64,
}

impl SkippedDirectorySummary {
    /// Add a file to this summary.
    const fn add_file(&mut self, size: u64) {
        self.file_count += 1;
        self.total_size += size;
    }

    /// Returns "file" or "files" based on the count.
    const fn files_word(&self) -> &'static str {
        if self.file_count == 1 { "file" } else { "files" }
    }
}

/// Information about a torrent file to be added.
struct TorrentInfo {
    /// Path to the torrent file.
    path: std::path::PathBuf,
    /// Parsed torrent data.
    torrent: Torrent,
    /// Raw torrent file bytes.
    bytes: Vec<u8>,
    /// Info hash calculated from raw bytes (lowercase hex).
    info_hash: String,
    /// Whether the original torrent has multiple files.
    original_is_multi_file: bool,
    /// Whether to treat this as multi-file after filtering (determines subdirectory creation).
    /// This is true only if more than one file will be included after filtering.
    effective_is_multi_file: bool,
    /// Custom name to rename to (None = use torrent's internal name).
    rename_to: Option<String>,
    /// Indices of files to exclude (for setting priority to 0).
    excluded_indices: Vec<usize>,
    /// For originally multi-file torrents that become effectively single-file,
    /// store the single included file's name to get the correct extension.
    single_included_file: Option<String>,
    /// Original name from torrent metadata (for file/folder renaming on disk).
    /// For single-file torrents, this is the filename.
    /// For multi-file torrents, this is the root folder name.
    original_name: Option<String>,
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
    ///
    /// If `ignore_filename_patterns` is provided and the torrent filename contains any of these
    /// strings, the filename is ignored and the internal name is used instead.
    #[allow(clippy::option_if_let_else)]
    fn suggested_name_raw(&self, ignore_filename_patterns: &[String]) -> Cow<'_, str> {
        // Try to get name from torrent filename first, unless it matches ignore patterns
        let torrent_filename = self.path.file_stem().and_then(|stem| stem.to_str()).filter(|filename| {
            // Skip filename if it contains any of the ignore patterns
            !ignore_filename_patterns
                .iter()
                .any(|pattern| filename.contains(pattern))
        });

        // Get the internal name from the torrent
        let internal_name = self.torrent.name();

        // For effective multi-file torrents (after filtering), this becomes the folder name
        if self.effective_is_multi_file {
            // Prefer torrent filename over internal name
            return if let Some(name) = torrent_filename {
                Cow::Borrowed(name)
            } else if let Some(name) = internal_name {
                Cow::Borrowed(name)
            } else {
                Cow::Borrowed("unknown")
            };
        }

        // For single-file torrents (or originally multi-file that became single after filtering),
        // preserve the file extension
        if let Some(filename) = torrent_filename {
            // For originally multi-file torrents that became single-file after filtering,
            // get the extension from the single included file
            let extension_source = if self.original_is_multi_file {
                self.single_included_file.as_deref()
            } else {
                internal_name
            };

            if let Some(source) = extension_source
                && let Some(extension) = Path::new(source).extension()
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

        // Fall back to internal name or single included file
        if let Some(ref file) = self.single_included_file {
            Cow::Borrowed(file.as_str())
        } else if let Some(name) = internal_name {
            Cow::Borrowed(name)
        } else {
            Cow::Borrowed("unknown")
        }
    }
}

impl QTorrent {
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
                name = Self::format_single_file_name(&dot_rename, &name);
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
                    Self::format_single_file_name(&dot_rename, internal_name)
                }
            },
        );

        Some(name)
    }

    const fn dot_formatter(&self) -> Option<DotFormat<'_>> {
        if let Some(dot_rename_config) = &self.dot_rename {
            Some(DotFormat::new(dot_rename_config))
        } else {
            None
        }
    }

    /// Format a single file name, stripping extension before formatting and restoring it after.
    fn format_single_file_name(dot_rename: &DotFormat, name: &str) -> String {
        // For single files, strip the extension before formatting and restore it after.
        // DotRename expects names without extensions.
        if let Ok((stem, extension)) = cli_tools::get_normalized_file_name_and_extension(Path::new(name)) {
            let formatted_stem = dot_rename.format_name(&stem);
            if extension.is_empty() {
                formatted_stem
            } else {
                format!("{formatted_stem}.{extension}")
            }
        } else {
            dot_rename.format_name(name)
        }
    }

    /// Create a new `TorrentAdder` from command line arguments.
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
            println!("{}", "No valid torrents to add".yellow());
            return Ok(());
        }

        // Dry-run mode: just show what would be done
        if self.config.dryrun {
            // Set suggested names on all torrents for display
            let torrents_with_names: Vec<TorrentInfo> = torrents
                .into_iter()
                .map(|mut info| {
                    info.rename_to = Some(self.clean_suggested_name(&info));
                    info
                })
                .collect();

            self.print_dryrun_summary(&torrents_with_names);
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
    ///
    /// Applies file filtering and determines whether to treat this as a multi-file torrent
    /// based on how many files will actually be included after filtering.
    fn parse_torrent(path: &Path, filter: &FileFilter<'_>) -> Result<TorrentInfo> {
        let bytes = fs::read(path).context("Failed to read torrent file")?;
        let torrent = Torrent::from_buffer(&bytes)?;

        // Calculate info hash from raw bytes (not re-serialized) for correct hash
        let info_hash = Torrent::info_hash_hex_from_bytes(&bytes)?;

        let original_is_multi_file = torrent.is_multi_file();
        let original_name = torrent.name().map(String::from);

        // Filter files and determine effective multi-file status based on included files
        let (effective_is_multi_file, excluded_indices, single_included_file, effective_original_name) =
            if original_is_multi_file && !filter.is_empty() {
                let filtered = torrent.filter_files(filter);
                let excluded: Vec<usize> = filtered.excluded.iter().map(|file| file.index).collect();
                // Treat as multi-file only if more than one file will be included
                let effective_multi = filtered.included.len() > 1;
                // If only one file remains, store its name for extension extraction
                // and use its path as the original name for renaming (since NoSubfolder is used)
                let (single_file, eff_name) = if filtered.included.len() == 1 {
                    let file_path = filtered.included[0].path.to_string();
                    (Some(file_path.clone()), Some(file_path))
                } else {
                    (None, original_name)
                };
                (effective_multi, excluded, single_file, eff_name)
            } else {
                // No filtering applied - use original multi-file status
                (original_is_multi_file, Vec::new(), None, original_name)
            };

        Ok(TorrentInfo {
            path: path.to_path_buf(),
            torrent,
            bytes,
            info_hash,
            original_is_multi_file,
            effective_is_multi_file,
            rename_to: None,
            excluded_indices,
            single_included_file,
            original_name: effective_original_name,
        })
    }

    /// Print dry-run summary of all torrents.
    fn print_dryrun_summary(&self, torrents: &[TorrentInfo]) {
        let total = torrents.len();
        print_bold!("DRYRUN {} torrents to add:", torrents.len());

        if self.config.verbose {
            self.print_options();
        }

        for (index, info) in torrents.iter().enumerate() {
            self.print_torrent_info(info, index + 1, total);
        }
    }

    /// Print information about a single torrent.
    ///
    /// The index is displayed as `[index/total]` with the index right-aligned
    /// to match the width of the total count.
    fn print_torrent_info(&self, info: &TorrentInfo, index: usize, total: usize) {
        let internal_name = info.torrent.name().unwrap_or("Unknown");
        let size = cli_tools::format_size(info.torrent.total_size());
        let width = total.to_string().chars().count();

        print_magenta_bold!(
            "\n[{index:>width$}/{total}] {}",
            cli_tools::path_to_string_relative(&info.path)
        );
        println!("  {} {}", "Internal name:".dimmed(), internal_name);
        if let Some(comment) = &info.torrent.comment
            && !comment.is_empty()
        {
            println!("  {}       {}", "Comment:".dimmed(), comment);
        }
        if self.config.verbose {
            println!("  {}     {}", "Info hash:".dimmed(), info.info_hash);
        }
        if info.original_is_multi_file {
            // Show folder name only if treating it as multi-file
            if info.effective_is_multi_file {
                println!("  {}   {}", "Folder name:".dimmed(), info.display_name().green());
            } else {
                println!("  {}     {}", "File name:".dimmed(), info.display_name().green());
            }
            self.print_multi_file_info(info);
        } else {
            println!("  {}     {}", "File name:".dimmed(), info.display_name().green());
            println!("  {}    {}", "Total size:".dimmed(), size);
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
                "  {}         {} ({} included, {} skipped)",
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
    ///
    /// Files excluded due to directory matching are grouped by directory name
    /// instead of listing each file individually.
    fn print_all_files_sorted(filtered: &FilteredFiles<'_>) {
        use std::collections::HashMap;

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

        println!("\n  {}", "Files:".bold());

        // Print included files
        for file in included_files {
            println!(
                "    {} {} ({})",
                "✓".green(),
                file.path,
                cli_tools::format_size(file.size)
            );
        }

        // Print other excluded files (not from directory matching)
        for file in other_excluded {
            let reason = file.exclusion_reason.as_deref().unwrap_or("excluded");
            println!(
                "    {} {} ({}) - {}",
                "✗".red(),
                file.path,
                cli_tools::format_size(file.size),
                reason.dimmed()
            );
        }

        // Print skipped directory summaries
        for (dir_name, summary) in skipped_dirs_sorted {
            println!(
                "    {} {}/{} ({} {}, {}) - {}",
                "✗".red(),
                dir_name,
                "...".dimmed(),
                summary.file_count,
                summary.files_word(),
                cli_tools::format_size(summary.total_size),
                format!("directory: {dir_name}").dimmed()
            );
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
    #[allow(clippy::similar_names)]
    async fn add_torrents_individually(&self, torrents: Vec<TorrentInfo>) -> Result<()> {
        // Connect to qBittorrent
        println!("{}", "Connecting to qBittorrent...".cyan());
        let mut client = QBittorrentClient::new(&self.config.host, self.config.port);

        client.login(&self.config.username, &self.config.password).await?;

        // Check connection works by getting app and api version numbers
        let app_version = client.get_app_version().await?;
        let api_version = client.get_api_version().await?;

        if self.config.verbose {
            println!("  {} {app_version}", "qBittorrent app version:".dimmed());
            println!("  {} {api_version}", "qBittorrent API version:".dimmed());
        }

        println!("{}\n", "Connected successfully".green());

        // Get list of existing torrents to check for duplicates
        let existing_torrents = client.get_torrent_list().await?;
        if self.config.verbose {
            println!(
                "  {} {}",
                "Existing torrents in qBittorrent:".dimmed(),
                existing_torrents.len()
            );
        }

        // Process each torrent individually
        let mut success_count = 0;
        let mut skipped_count = 0;
        let mut duplicate_count = 0;
        let mut renamed_count = 0;
        let mut error_count = 0;
        let total = torrents.len();

        for (index, mut info) in torrents.into_iter().enumerate() {
            println!("{}", "─".repeat(60));
            self.print_torrent_info(&info, index + 1, total);

            // Check if torrent already exists in qBittorrent
            if let Some(existing_name) = Self::check_existing_torrent(&info, &existing_torrents) {
                println!(
                    "  {} Already exists in qBittorrent as: {}",
                    "⊘".yellow(),
                    existing_name.cyan()
                );

                // Offer to rename the existing torrent
                match self.prompt_rename_existing(&info, existing_name, &client).await {
                    Ok(true) => renamed_count += 1,
                    Ok(false) => duplicate_count += 1,
                    Err(error) => {
                        cli_tools::print_error!("Failed to rename: {error}");
                        duplicate_count += 1;
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
        if renamed_count > 0 {
            println!("  {} {}", "Renamed:".cyan(), renamed_count);
        }
        if duplicate_count > 0 {
            println!("  {} {}", "Already added:".dimmed(), duplicate_count);
        }
        if skipped_count > 0 {
            println!("  {} {}", "Skipped:".yellow(), skipped_count);
        }
        if error_count > 0 {
            println!("  {} {}", "Failed:".red(), error_count);
        }

        Ok(())
    }

    /// Check if a torrent already exists in qBittorrent by comparing info hashes.
    ///
    /// Returns the existing torrent name if found, None otherwise.
    fn check_existing_torrent<'a>(
        info: &TorrentInfo,
        existing_torrents: &'a HashMap<String, String>,
    ) -> Option<&'a String> {
        let hash_lower = info.info_hash.to_lowercase();
        existing_torrents.get(&hash_lower)
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
        if let Some(ref tags) = self.config.tags {
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
                    cli_tools::print_warning!("Could not rename file/folder on disk: {error}");
                }
            }
        }

        Ok(true)
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
            "  {} [{}]",
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
        let suggested_ext = Self::extract_file_extension(&normalized_suggested);
        let internal_ext = Self::extract_file_extension(&normalized_internal);

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
                normalized_internal = Self::insert_date_before_extension(&normalized_internal, date);
            }
            (None, Some(date)) => {
                // Add date from internal to suggested (insert before extension if present)
                normalized_suggested = Self::insert_date_before_extension(&normalized_suggested, date);
            }
            _ => {}
        }

        (normalized_suggested, Some(normalized_internal))
    }

    /// Extract a file extension if it looks like a real media file extension.
    ///
    /// Only recognizes known media extensions to avoid treating names like "Show.Name" as having extension "Name".
    /// Also filters out purely numeric extensions (like `.15` from dates).
    fn extract_file_extension(name: &str) -> Option<String> {
        let ext = Path::new(name).extension()?.to_string_lossy().to_lowercase();

        // If the extension is purely numeric, it's likely part of a date, not a real extension
        if ext.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }

        if KNOWN_EXTENSIONS.contains(&ext.as_str()) {
            Some(ext)
        } else {
            None
        }
    }

    /// Insert a date before the file extension, or at the end if no extension.
    fn insert_date_before_extension(name: &str, date: &str) -> String {
        let path = Path::new(name);
        path.extension().map_or_else(
            || format!("{name}.{date}"),
            |ext| {
                let stem = path.file_stem().map_or(name, |s| s.to_str().unwrap_or(name));
                format!("{stem}.{date}.{}", ext.to_string_lossy())
            },
        )
    }

    /// Add a single torrent to qBittorrent.
    #[allow(clippy::too_many_lines)]
    async fn add_single_torrent(&self, client: &QBittorrentClient, info: TorrentInfo) -> Result<()> {
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
            tags: self.config.tags.clone(),
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
        if let Some(ref new_name) = rename_to
            && let Some(ref old_name) = original_name
            && new_name != old_name
        {
            // Retry with increasing delays - qBittorrent needs time to fully register the torrent
            let delays_ms = [250, 500, 1000];
            let mut rename_success = false;
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
                        rename_success = true;
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                    }
                }
            }

            if !rename_success {
                if let Some(error) = last_error {
                    cli_tools::print_warning!("Could not rename file/folder after retries: {error}");
                }
                println!(
                    "  {} You may need to manually rename in qBittorrent: {} → {}",
                    "⚠".yellow(),
                    old_name,
                    new_name
                );
            }
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
                    cli_tools::print_warning!("Could not set file priorities (torrent may still be loading): {error}");
                }
                println!(
                    "  {} You may need to manually skip {} file(s) in qBittorrent",
                    "⚠".yellow(),
                    excluded_indices.len()
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    mod insert_date_before_extension {
        use super::*;

        #[test]
        fn inserts_date_before_extension() {
            let result = QTorrent::insert_date_before_extension("Name.mp4", "2024.01.15");
            assert_eq!(result, "Name.2024.01.15.mp4");
        }

        #[test]
        fn appends_date_when_no_extension() {
            let result = QTorrent::insert_date_before_extension("Name", "2024.01.15");
            assert_eq!(result, "Name.2024.01.15");
        }

        #[test]
        fn handles_multiple_dots_in_name() {
            let result = QTorrent::insert_date_before_extension("Some.Name.Here.mp4", "2024.01.15");
            assert_eq!(result, "Some.Name.Here.2024.01.15.mp4");
        }
    }
}
