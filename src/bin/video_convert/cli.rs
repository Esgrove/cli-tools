//! CLI types and helper functions for video convert.

use std::str::FromStr;

use anyhow::Result;
use clap::ValueEnum;
use colored::Colorize;
use serde::Deserialize;

use crate::config::Config;
use crate::database::Database;

/// Sort order options for video files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Sort by bitrate (highest first)
    #[default]
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
}

impl SortOrder {
    /// Returns the SQL ORDER BY clause for this sort order.
    #[must_use]
    pub const fn sql_order_clause(self) -> &'static str {
        match self {
            Self::Bitrate => "bitrate_kbps DESC",
            Self::Size => "size_bytes DESC",
            Self::SizeAsc => "size_bytes ASC",
            Self::Duration => "duration DESC",
            Self::DurationAsc => "duration ASC",
            Self::Resolution => "width * height DESC",
            Self::ResolutionAsc => "width * height ASC",
            Self::Impact => "(bitrate_kbps / frames_per_second) * duration DESC",
            Self::Name => "full_path ASC",
        }
    }
}

impl FromStr for SortOrder {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "bitrate" => Ok(Self::Bitrate),
            "size" => Ok(Self::Size),
            "size_asc" | "size-asc" => Ok(Self::SizeAsc),
            "duration" => Ok(Self::Duration),
            "duration_asc" | "duration-asc" => Ok(Self::DurationAsc),
            "resolution" => Ok(Self::Resolution),
            "resolution_asc" | "resolution-asc" => Ok(Self::ResolutionAsc),
            "impact" => Ok(Self::Impact),
            "name" => Ok(Self::Name),
            _ => Err(format!("Unknown sort order: {s}")),
        }
    }
}

impl std::fmt::Display for SortOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Bitrate => "bitrate",
            Self::Size => "size",
            Self::SizeAsc => "size-asc",
            Self::Duration => "duration",
            Self::DurationAsc => "duration-asc",
            Self::Resolution => "resolution",
            Self::ResolutionAsc => "resolution-asc",
            Self::Impact => "impact",
            Self::Name => "name",
        };
        write!(f, "{name}")
    }
}

/// Clear all entries from the database.
pub fn clear_database() -> Result<()> {
    let database = Database::open_default()?;
    let cleared = database.clear()?;
    println!("{}", format!("Cleared {cleared} entries from database").green());
    Ok(())
}

/// List file extension counts in the database.
pub fn list_extensions(verbose: bool) -> Result<()> {
    let database = Database::open_default()?;
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
            .map(|e| cli_tools::format_size(e.total_size).len())
            .max()
            .unwrap_or(1);

        for ext in &ext_stats {
            let size_str = cli_tools::format_size(ext.total_size);
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
            cli_tools::format_size(total_size)
        );
    }
    Ok(())
}

/// Show database statistics and contents.
///
/// # Errors
/// Returns an error if the database cannot be opened or queried.
pub fn show_database_contents(config: &Config) -> Result<()> {
    let database = Database::open_default()?;
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
                    cli_tools::format_size(ext.total_size)
                );
            }
        }

        println!();
        println!("{}", "Pending files:".bold());

        let files = database.get_pending_files(&config.db_filter)?;
        let total_matching = files.len();
        let display_count = config
            .display_limit
            .map_or(total_matching, |limit| total_matching.min(limit));

        for file in files.iter().take(display_count) {
            // Use fixed widths for consistent alignment:
            // size: 9 chars (e.g. "19.20 GB "), bitrate: 10 chars (e.g. "15.0 Mbps "), duration: 10 chars (e.g. "3h 01m 18s")
            let size_str = cli_tools::format_size(file.size_bytes);
            let bitrate_str = format!("{:.1} Mbps", file.bitrate_kbps as f64 / 1000.0);
            let duration_str = cli_tools::format_duration(std::time::Duration::from_secs_f64(file.duration));
            let action_str = match file.action {
                crate::database::PendingAction::Convert => "CONVERT".yellow(),
                crate::database::PendingAction::Remux => "REMUX".cyan(),
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
mod sort_order_tests {
    use super::*;

    fn parse(s: &str) -> Result<SortOrder, String> {
        <SortOrder as std::str::FromStr>::from_str(s)
    }

    #[test]
    fn from_str_parses_bitrate() {
        assert_eq!(parse("bitrate").unwrap(), SortOrder::Bitrate);
        assert_eq!(parse("BITRATE").unwrap(), SortOrder::Bitrate);
    }

    #[test]
    fn from_str_parses_size() {
        assert_eq!(parse("size").unwrap(), SortOrder::Size);
        assert_eq!(parse("size_asc").unwrap(), SortOrder::SizeAsc);
        assert_eq!(parse("size-asc").unwrap(), SortOrder::SizeAsc);
    }

    #[test]
    fn from_str_parses_duration() {
        assert_eq!(parse("duration").unwrap(), SortOrder::Duration);
        assert_eq!(parse("duration_asc").unwrap(), SortOrder::DurationAsc);
        assert_eq!(parse("duration-asc").unwrap(), SortOrder::DurationAsc);
    }

    #[test]
    fn from_str_parses_resolution() {
        assert_eq!(parse("resolution").unwrap(), SortOrder::Resolution);
        assert_eq!(parse("resolution_asc").unwrap(), SortOrder::ResolutionAsc);
        assert_eq!(parse("resolution-asc").unwrap(), SortOrder::ResolutionAsc);
    }

    #[test]
    fn from_str_parses_impact() {
        assert_eq!(parse("impact").unwrap(), SortOrder::Impact);
    }

    #[test]
    fn from_str_parses_name() {
        assert_eq!(parse("name").unwrap(), SortOrder::Name);
    }

    #[test]
    fn from_str_returns_error_for_unknown() {
        let result = parse("unknown");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown sort order"));
    }

    #[test]
    fn display_formats_correctly() {
        assert_eq!(format!("{}", SortOrder::Bitrate), "bitrate");
        assert_eq!(format!("{}", SortOrder::Size), "size");
        assert_eq!(format!("{}", SortOrder::SizeAsc), "size-asc");
        assert_eq!(format!("{}", SortOrder::Duration), "duration");
        assert_eq!(format!("{}", SortOrder::DurationAsc), "duration-asc");
        assert_eq!(format!("{}", SortOrder::Resolution), "resolution");
        assert_eq!(format!("{}", SortOrder::ResolutionAsc), "resolution-asc");
        assert_eq!(format!("{}", SortOrder::Impact), "impact");
        assert_eq!(format!("{}", SortOrder::Name), "name");
    }

    #[test]
    fn sql_order_clause_returns_valid_sql() {
        assert_eq!(SortOrder::Bitrate.sql_order_clause(), "bitrate_kbps DESC");
        assert_eq!(SortOrder::Size.sql_order_clause(), "size_bytes DESC");
        assert_eq!(SortOrder::SizeAsc.sql_order_clause(), "size_bytes ASC");
        assert_eq!(SortOrder::Duration.sql_order_clause(), "duration DESC");
        assert_eq!(SortOrder::DurationAsc.sql_order_clause(), "duration ASC");
        assert_eq!(SortOrder::Resolution.sql_order_clause(), "width * height DESC");
        assert_eq!(SortOrder::ResolutionAsc.sql_order_clause(), "width * height ASC");
        assert_eq!(
            SortOrder::Impact.sql_order_clause(),
            "(bitrate_kbps / frames_per_second) * duration DESC"
        );
        assert_eq!(SortOrder::Name.sql_order_clause(), "full_path ASC");
    }

    #[test]
    fn default_is_bitrate() {
        assert_eq!(SortOrder::default(), SortOrder::Bitrate);
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
        assert_ne!(DatabaseMode::Process, DatabaseMode::Clear);
    }

    #[test]
    fn database_mode_debug() {
        let mode = DatabaseMode::Process;
        let debug = format!("{mode:?}");
        assert!(debug.contains("Process"));
    }
}
