//! Terminal output for qtorrent add.
//!
//! Prints the torrent summaries, the dry-run overview, the configured options,
//! and the per-file listing with include and exclude status.

use std::collections::HashMap;
use std::path::Path;

use colored::Colorize;

use cli_tools::{print_bold, print_magenta_bold};

use super::QTorrent;
use crate::qbittorrent::TorrentListItem;
use crate::torrent::{FileInfo, FilteredFiles};
use crate::utils;
use crate::utils::{SkippedDirectorySummary, TorrentInfo};

impl QTorrent {
    /// Print dry-run summary of all torrents.
    pub(super) fn print_dryrun_summary(
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
    pub(super) fn print_torrent_info(&self, info: &TorrentInfo, index: usize, total: usize) {
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

    /// Print final details about the torrent before confirmation.
    pub(super) fn print_final_details(&self, info: &TorrentInfo) {
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
            println!(
                "  {} {}",
                "Include images:".dimmed(),
                if self.config.file_filter.include_images {
                    "yes".green()
                } else {
                    "no".yellow()
                }
            );
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
            if self.config.file_filter.include_images
                && let Some(min_image_file_count) = self.config.file_filter.min_image_file_count
                && min_image_file_count > 1
            {
                println!("  {} {}", "Min image count:".dimmed(), min_image_file_count);
            }
            if self.config.file_filter.include_images
                && let Some(min_image_size_kb) = self.config.file_filter.min_image_size_kb
            {
                println!("  {} {} KB", "Min image size:".dimmed(), min_image_size_kb);
            }
        }
    }

    /// Print all files sorted by size (largest first), showing include/exclude status.
    ///
    /// Files excluded due to directory matching are grouped by directory name
    /// instead of listing each file individually.
    fn print_all_files_sorted(&self, filtered: &FilteredFiles<'_>) {
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
        included_files.sort_by_key(|file| std::cmp::Reverse(file.size));

        other_excluded.sort_by_key(|file| std::cmp::Reverse(file.size));

        // Sort skipped directories by total size descending
        let mut skipped_dirs_sorted: Vec<_> = skipped_directories.into_iter().collect();
        skipped_dirs_sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.total_size));

        // Find the widest formatted size string for right-alignment
        let max_size_width = included_files
            .iter()
            .map(|file| cli_tools::format_size(file.size).len())
            .chain(
                other_excluded
                    .iter()
                    .map(|file| cli_tools::format_size(file.size).len()),
            )
            .chain(
                skipped_dirs_sorted
                    .iter()
                    .map(|(_, summary)| cli_tools::format_size(summary.total_size).len()),
            )
            .max()
            .unwrap_or(0);

        println!("  {}", "Files:".bold());

        // Print included files
        for file in included_files {
            let size_str = cli_tools::format_size(file.size);
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

            let check = "✓".green();
            println!("    {size_str:>max_size_width$}  {check} {display_path}");
        }

        // Print other excluded files (not from directory matching)
        for file in other_excluded {
            let size_str = cli_tools::format_size(file.size);
            let reason = file.exclusion_reason.as_deref().unwrap_or("excluded");
            let path = &file.path;
            let reason = reason.dimmed();
            let cross = "✗".red();
            println!("    {size_str:>max_size_width$}  {cross} {path} - {reason}");
        }

        // Print skipped directory summaries
        for (dir_name, summary) in skipped_dirs_sorted {
            let size_str = cli_tools::format_size(summary.total_size);
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
}
