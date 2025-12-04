use std::time::Duration;

use colored::Colorize;

use crate::convert::{ProcessResult, SkipReason};

/// Statistics for the conversion run
#[derive(Debug, Default)]
pub struct RunStats {
    pub(crate) files_converted: usize,
    pub(crate) files_remuxed: usize,
    pub(crate) files_renamed: usize,
    pub(crate) files_skipped_converted: usize,
    pub(crate) files_skipped_bitrate: usize,
    pub(crate) files_skipped_duplicate: usize,
    pub(crate) files_failed: usize,
    pub(crate) total_original_size: u64,
    pub(crate) total_converted_size: u64,
    pub(crate) total_duration: Duration,
}

/// Statistics for a single file conversion
#[derive(Debug, Default, Clone, Copy)]
pub struct ConversionStats {
    original_size: u64,
    original_bitrate_kbps: u64,
    converted_size: u64,
    converted_bitrate_kbps: u64,
}

impl ConversionStats {
    /// Create conversion stats with original and converted file sizes and bitrates.
    pub(crate) const fn new(
        original_size: u64,
        original_bitrate_kbps: u64,
        converted_size: u64,
        output_bitrate_kbps: u64,
    ) -> Self {
        Self {
            original_size,
            original_bitrate_kbps,
            converted_size,
            converted_bitrate_kbps: output_bitrate_kbps,
        }
    }

    /// Calculate the size difference
    #[allow(clippy::cast_possible_wrap)]
    const fn size_difference(&self) -> i64 {
        self.converted_size as i64 - self.original_size as i64
    }

    /// Calculate the percentage change
    fn change_percentage(&self) -> f64 {
        if self.original_size == 0 || self.converted_size == 0 {
            return 0.0;
        }
        self.size_difference() as f64 / self.original_size as f64 * 100.0
    }
}

impl RunStats {
    /// Record the result of processing a file.
    pub(crate) fn add_result(&mut self, result: &ProcessResult, duration: Duration) {
        self.total_duration += duration;
        match result {
            ProcessResult::Converted { stats, .. } => {
                self.files_converted += 1;
                *self += *stats;
            }
            ProcessResult::Remuxed { .. } => {
                self.files_remuxed += 1;
            }
            ProcessResult::Renamed { .. } => {
                self.files_renamed += 1;
            }
            ProcessResult::Skipped(reason) => match reason {
                SkipReason::AlreadyConverted => self.files_skipped_converted += 1,
                SkipReason::BitrateBelowThreshold { .. } => self.files_skipped_bitrate += 1,
                SkipReason::OutputExists { .. } => self.files_skipped_duplicate += 1,
            },
            ProcessResult::Failed { .. } => {
                self.files_failed += 1;
            }
        }
    }

    /// Get the total number of skipped files.
    pub(crate) const fn total_skipped(&self) -> usize {
        self.files_skipped_converted + self.files_skipped_bitrate + self.files_skipped_duplicate
    }

    /// Calculate total space saved (negative if size increased).
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) const fn space_saved(&self) -> i64 {
        self.total_original_size as i64 - self.total_converted_size as i64
    }

    /// Print a summary of conversion statistics to stdout.
    pub(crate) fn print_summary(&self) {
        println!("{}", "\n--- Conversion Summary ---".bold().magenta());
        println!("Files converted:        {}", self.files_converted);
        println!("Files remuxed:          {}", self.files_remuxed);
        println!("Files renamed:          {}", self.files_renamed);
        println!(
            "Files failed:           {}",
            if self.files_failed > 0 {
                self.files_failed.to_string().red()
            } else {
                "0".normal()
            }
        );
        println!("Files skipped:          {}", self.total_skipped());
        if self.total_skipped() > 0 {
            println!("  - Already converted:  {}", self.files_skipped_converted);
            println!("  - Below bitrate:      {}", self.files_skipped_bitrate);
            println!("  - Duplicates:         {}", self.files_skipped_duplicate);
        }
        println!();

        if self.files_converted > 0 {
            println!(
                "Total original size:    {}",
                cli_tools::format_size(self.total_original_size)
            );
            println!(
                "Total converted size:   {}",
                cli_tools::format_size(self.total_converted_size)
            );

            if self.total_original_size > 0 {
                let saved = self.space_saved();
                let ratio = saved.abs() as f64 / self.total_original_size as f64 * 100.0;

                if saved >= 0 {
                    println!(
                        "Space saved:            {} ({:.1}%)",
                        cli_tools::format_size(saved as u64),
                        ratio
                    );
                } else {
                    println!(
                        "Space increased:        {} ({:.1}%)",
                        cli_tools::format_size((-saved) as u64),
                        ratio
                    );
                }
            }
        }

        println!(
            "Total time:             {}",
            cli_tools::format_duration(self.total_duration)
        );
    }
}

impl std::fmt::Display for ConversionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} / {:.2} Mbps -> {} / {:.2} Mbps ({:.1}%)",
            cli_tools::format_size(self.original_size),
            self.original_bitrate_kbps as f64 / 1000.0,
            cli_tools::format_size(self.converted_size),
            self.converted_bitrate_kbps as f64 / 1000.0,
            self.change_percentage(),
        )
    }
}

impl std::ops::AddAssign<ConversionStats> for RunStats {
    fn add_assign(&mut self, stats: ConversionStats) {
        self.total_original_size += stats.original_size;
        self.total_converted_size += stats.converted_size;
    }
}
