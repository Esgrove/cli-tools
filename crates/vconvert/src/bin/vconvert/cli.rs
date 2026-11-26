//! Command-line support for video conversion.
//!
//! Defines the sort order argument and the database modes,
//! and implements the database commands that print the database contents.

use anyhow::Result;
use clap::ValueEnum;
use cli_tools_vconvert::{Database, PendingAction, SortOrder};
use colored::Colorize;

use crate::config::Config;

/// Sort order options for video files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortOrderArgument {
    /// Sort by bitrate (highest first)
    Bitrate,
    /// Sort by file size (largest first)
    Size,
    /// Sort by file size (smallest first)
    SizeAsc,
    /// Sort by duration (longest first)
    Duration,
    /// Sort by duration (shortest first)
    DurationAsc,
    /// Sort by resolution (highest first)
    Resolution,
    /// Sort by resolution (lowest first)
    ResolutionAsc,
    /// Sort by potential savings (bitrate / fps * duration, highest first)
    Impact,
    /// Sort alphabetically by file name
    Name,
}

/// Database operation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseMode {
    /// Process files from database instead of scanning.
    Process,
    /// Clear all entries from the database.
    Clear,
    /// Show database statistics and contents.
    Show,
    /// List file extension counts in the database.
    ListExtensions,
    /// Remove stale entries from the scan cache.
    CleanScanCache,
}

impl From<SortOrderArgument> for SortOrder {
    fn from(argument: SortOrderArgument) -> Self {
        match argument {
            SortOrderArgument::Bitrate => Self::Bitrate,
            SortOrderArgument::Size => Self::Size,
            SortOrderArgument::SizeAsc => Self::SizeAsc,
            SortOrderArgument::Duration => Self::Duration,
            SortOrderArgument::DurationAsc => Self::DurationAsc,
            SortOrderArgument::Resolution => Self::Resolution,
            SortOrderArgument::ResolutionAsc => Self::ResolutionAsc,
            SortOrderArgument::Impact => Self::Impact,
            SortOrderArgument::Name => Self::Name,
        }
    }
}

/// Remove stale entries from the scan cache whose files no longer exist on disk.
pub fn clean_scan_cache(database: &mut Database, verbose: bool) -> Result<()> {
    if verbose {
        println!("{}", format!("Database: {}", Database::path().display()).bold());
    }

    let total = database.scanned_file_count()?;
    if total == 0 {
        println!("Scan cache is empty");
        return Ok(());
    }

    println!("Checking {total} cached scan entries...");
    let removed = database.remove_missing_scanned_files()?;
    if removed > 0 {
        println!(
            "{}",
            format!(
                "Removed {removed} stale entries from scan cache ({} remaining)",
                total - removed as u64
            )
            .green()
        );
    } else {
        println!("{}", "All cached entries are up to date".green());
    }
    Ok(())
}

/// Clear all entries from the database.
pub fn clear_database(database: &Database) -> Result<()> {
    let cleared = database.clear()?;
    println!("{}", format!("Cleared {cleared} entries from database").green());
    Ok(())
}

/// List file extension counts in the database.
pub fn list_extensions(database: &Database, verbose: bool) -> Result<()> {
    if verbose {
        println!("{}", format!("Database: {}", Database::path().display()).bold());
        println!();
    }

    let ext_stats = database.get_extension_stats()?;
    if ext_stats.is_empty() {
        println!("No files in database");
    } else {
        // Calculate column widths for right-alignment
        let max_count_width = ext_stats.iter().map(|e| e.count.to_string().len()).max().unwrap_or(1);
        let max_size_width = ext_stats
            .iter()
            .map(|e| cli_tools_common::format_size(e.total_size).len())
            .max()
            .unwrap_or(1);

        for ext in &ext_stats {
            let size_str = cli_tools_common::format_size(ext.total_size);
            println!(
                ".{:<4} {:>max_count_width$} files  {:>max_size_width$}",
                ext.extension, ext.count, size_str
            );
        }
        println!();
        let total_files: u64 = ext_stats.iter().map(|e| e.count).sum();
        let total_size: u64 = ext_stats.iter().map(|e| e.total_size).sum();
        println!(
            "{:<5} {:>max_count_width$} files  {:>max_size_width$}",
            "Total",
            total_files,
            cli_tools_common::format_size(total_size)
        );
    }
    Ok(())
}

/// Show database statistics and contents.
///
/// # Errors
/// Returns an error if the database cannot be opened or queried.
pub fn show_database_contents(database: &Database, config: &Config) -> Result<()> {
    if config.verbose {
        println!("{}", format!("Database: {}", Database::path().display()).bold());
        println!();
    }

    let stats = database.get_stats()?;
    println!("{stats}");

    if stats.total_files > 0 {
        // Show extension statistics
        let ext_stats = database.get_extension_stats()?;
        if !ext_stats.is_empty() {
            println!();
            println!("{}", "File extensions:".bold());

            // Calculate column widths for right-alignment
            let max_count_width = ext_stats.iter().map(|e| e.count.to_string().len()).max().unwrap_or(1);

            for ext in &ext_stats {
                println!(
                    "  .{}: {:>max_count_width$} files ({})",
                    ext.extension,
                    ext.count,
                    cli_tools_common::format_size(ext.total_size)
                );
            }
        }

        println!();
        println!("{}", "Pending files:".bold());

        let files = database.get_pending_files(&config.options.pending_file_filter())?;
        let total_matching = files.len();
        let display_count = config
            .display_limit
            .map_or(total_matching, |limit| total_matching.min(limit));

        for file in files.iter().take(display_count) {
            // Use fixed widths for consistent alignment:
            // size: 9 chars (e.g. "19.20 GB "), bitrate:
            // 10 chars (e.g. "15.0 Mbps "), duration: 10 chars (e.g. "3h 01m 18s")
            let size_str = cli_tools_common::format_size(file.size_bytes);
            let bitrate_str = format!("{:.1} Mbps", file.bitrate_kbps as f64 / 1000.0);
            let duration_str = cli_tools_common::format_duration_seconds(file.duration);
            let action_str = match file.action {
                PendingAction::Convert => "CONVERT".yellow(),
                PendingAction::Remux => "REMUX".cyan(),
                PendingAction::SubtitleMux => "SUBS".purple(),
            };
            println!(
                "  [{action_str}] {:<5} {:>10} {:>10} {:>10} - {}",
                file.extension,
                size_str,
                bitrate_str,
                duration_str,
                file.full_path.display()
            );
        }

        if files.is_empty() {
            println!("  (no files match the current filter)");
        } else {
            println!();
            let remaining = total_matching - display_count;
            if remaining > 0 {
                println!("Showing {display_count} of {total_matching} matching files ({remaining} more)...");
            } else {
                println!("Showing {total_matching} of {} files", stats.total_files);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod sort_order_argument_tests {
    use super::*;

    #[test]
    fn every_argument_maps_to_the_sort_order_of_the_same_name() {
        for argument in SortOrderArgument::value_variants() {
            let name = argument
                .to_possible_value()
                .expect("every sort order has a value name")
                .get_name()
                .to_string();

            assert_eq!(SortOrder::from(*argument).to_string(), name);
        }
    }
}

#[cfg(test)]
mod database_mode_tests {
    use super::*;

    #[test]
    fn database_mode_equality() {
        assert_eq!(DatabaseMode::Process, DatabaseMode::Process);
        assert_eq!(DatabaseMode::Clear, DatabaseMode::Clear);
        assert_eq!(DatabaseMode::Show, DatabaseMode::Show);
        assert_eq!(DatabaseMode::ListExtensions, DatabaseMode::ListExtensions);
        assert_eq!(DatabaseMode::CleanScanCache, DatabaseMode::CleanScanCache);
        assert_ne!(DatabaseMode::Process, DatabaseMode::Clear);
        assert_ne!(DatabaseMode::CleanScanCache, DatabaseMode::Clear);
    }

    #[test]
    fn database_mode_debug() {
        let mode = DatabaseMode::Process;
        let debug = format!("{mode:?}");
        assert!(debug.contains("Process"));
    }
}

#[cfg(test)]
mod test_database_reporting {
    use std::path::PathBuf;

    use cli_tools_vconvert::{ConvertOptions, VideoInfo};

    use super::*;

    /// Video info for a file of the given size and bitrate.
    fn video_info(size_bytes: u64, bitrate_kbps: u64) -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps,
            size_bytes,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    /// Database holding one pending file per given extension.
    fn database_with(extensions: &[&str]) -> Database {
        let database = Database::open_in_memory().expect("the database should open");
        for (index, extension) in extensions.iter().enumerate() {
            let path = PathBuf::from(format!("/videos/movie{index}.{extension}"));
            database
                .upsert_pending_file(
                    &path,
                    extension,
                    &video_info(1_000_000 * (index as u64 + 1), 8000),
                    PendingAction::Convert,
                )
                .expect("the file should be recorded");
        }
        database
    }

    #[test]
    fn listing_extensions_of_an_empty_database_reports_nothing_found() {
        let database = Database::open_in_memory().expect("the database should open");

        list_extensions(&database, true).expect("listing should succeed");
    }

    #[test]
    fn show_database_contents_does_not_panic_on_negative_duration() {
        let database = Database::open_in_memory().expect("the database should open");
        let mut info = video_info(1_000_000, 8000);
        info.duration = -10.0;
        database
            .upsert_pending_file(
                std::path::Path::new("/videos/bad.mkv"),
                "mkv",
                &info,
                PendingAction::Convert,
            )
            .expect("insert");
        let config = Config {
            options: ConvertOptions::default(),
            convert_all: false,
            convert_other: false,
            display_limit: None,
            database_mode: Some(DatabaseMode::Show),
            verbose: false,
        };
        show_database_contents(&database, &config).expect("show-db should handle negative duration safely");
    }

    #[test]
    fn listing_extensions_reports_every_extension_and_the_total() {
        let database = database_with(&["mp4", "mkv", "mkv"]);

        list_extensions(&database, false).expect("listing should succeed");
        list_extensions(&database, true).expect("listing should succeed");

        let stats = database.get_extension_stats().expect("stats should be available");
        assert_eq!(stats.len(), 2, "each extension should be counted once: {stats:?}");
        let total: u64 = stats.iter().map(|entry| entry.count).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn showing_an_empty_database_stops_after_the_statistics() {
        let database = Database::open_in_memory().expect("the database should open");

        show_database_contents(&database, &Config::default()).expect("showing should succeed");
    }

    #[test]
    fn showing_the_database_lists_the_pending_files() {
        let database = database_with(&["mp4", "mkv"]);

        show_database_contents(
            &database,
            &Config {
                verbose: true,
                ..Config::default()
            },
        )
        .expect("showing should succeed");
    }

    #[test]
    fn the_display_limit_reports_how_many_files_are_left() {
        let database = database_with(&["mp4", "mkv", "avi"]);

        show_database_contents(
            &database,
            &Config {
                display_limit: Some(1),
                ..Config::default()
            },
        )
        .expect("showing should succeed");
    }

    #[test]
    fn a_filter_that_matches_nothing_is_reported() {
        let database = database_with(&["mp4"]);

        show_database_contents(
            &database,
            &Config {
                options: ConvertOptions {
                    extensions: vec!["webm".to_string()],
                    ..ConvertOptions::default()
                },
                ..Config::default()
            },
        )
        .expect("showing should succeed");
    }

    #[test]
    fn every_pending_action_is_rendered() {
        let database = Database::open_in_memory().expect("the database should open");
        for (index, action) in [PendingAction::Convert, PendingAction::Remux, PendingAction::SubtitleMux]
            .into_iter()
            .enumerate()
        {
            database
                .upsert_pending_file(
                    &PathBuf::from(format!("/videos/movie{index}.mkv")),
                    "mkv",
                    &video_info(1_000_000, 8000),
                    action,
                )
                .expect("the file should be recorded");
        }

        show_database_contents(&database, &Config::default()).expect("showing should succeed");
    }

    #[test]
    fn clearing_removes_every_entry() {
        let database = database_with(&["mp4", "mkv"]);

        clear_database(&database).expect("clearing should succeed");

        let stats = database.get_stats().expect("stats should be available");
        assert_eq!(stats.total_files, 0);
    }

    #[test]
    fn cleaning_an_empty_scan_cache_reports_it_is_empty() {
        let mut database = Database::open_in_memory().expect("the database should open");

        clean_scan_cache(&mut database, true).expect("cleaning should succeed");

        assert_eq!(database.scanned_file_count().expect("count should be available"), 0);
    }

    #[test]
    fn cleaning_drops_the_entries_whose_files_are_gone() {
        let mut database = Database::open_in_memory().expect("the database should open");
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let present = directory.path().join("present.mp4");
        std::fs::write(&present, b"video").expect("file should be written");
        database
            .upsert_scanned_file(&present, &video_info(1_000_000, 8000))
            .expect("the file should be recorded");
        database
            .upsert_scanned_file(&directory.path().join("missing.mp4"), &video_info(1_000_000, 8000))
            .expect("the file should be recorded");

        assert_eq!(database.scanned_file_count().expect("count should be available"), 2);

        clean_scan_cache(&mut database, false).expect("cleaning should succeed");

        assert_eq!(
            database.scanned_file_count().expect("count should be available"),
            1,
            "only the missing file should be dropped"
        );
    }

    #[test]
    fn cleaning_reports_when_every_entry_is_still_valid() {
        let mut database = Database::open_in_memory().expect("the database should open");
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let present = directory.path().join("present.mp4");
        std::fs::write(&present, b"video").expect("file should be written");
        database
            .upsert_scanned_file(&present, &video_info(1_000_000, 8000))
            .expect("the file should be recorded");

        clean_scan_cache(&mut database, false).expect("cleaning should succeed");

        assert_eq!(database.scanned_file_count().expect("count should be available"), 1);
    }
}
