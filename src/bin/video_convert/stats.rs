//! Analysis and conversion statistics for video processing.
//!
//! Collects per-file and aggregate outcomes, and implements formatted run summaries.

use std::ops::AddAssign;
use std::time::Duration;

use colored::Colorize;

use crate::types::ProcessResult;

/// Statistics from the video file analysis.
#[derive(Debug, Default)]
pub struct AnalysisStats {
    pub(crate) to_rename: usize,
    pub(crate) to_remux: usize,
    pub(crate) to_subtitle_mux: usize,
    pub(crate) to_convert: usize,
    pub(crate) skipped_converted: usize,
    pub(crate) skipped_bitrate_low: usize,
    pub(crate) skipped_bitrate_high: usize,
    pub(crate) skipped_duration_short: usize,
    pub(crate) skipped_duration_long: usize,
    pub(crate) skipped_resolution_low: usize,
    pub(crate) skipped_duplicate: usize,
    pub(crate) duplicates_deleted: usize,
    pub(crate) duplicate_delete_failed: usize,
    pub(crate) file_missing: usize,
    pub(crate) analysis_failed: usize,
}

/// Statistics for the conversion run
#[derive(Debug, Default)]
pub struct RunStats {
    pub(crate) files_renamed: usize,
    pub(crate) files_remuxed: usize,
    pub(crate) files_subtitle_muxed: usize,
    pub(crate) files_converted: usize,
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

impl AnalysisStats {
    /// Get the total number of skipped files.
    pub(crate) const fn total_skipped(&self) -> usize {
        self.skipped_converted
            + self.skipped_bitrate_low
            + self.skipped_bitrate_high
            + self.skipped_duration_short
            + self.skipped_duration_long
            + self.skipped_resolution_low
            + self.skipped_duplicate
            + self.duplicates_deleted
            + self.duplicate_delete_failed
            + self.file_missing
    }

    /// Print analysis summary.
    pub(crate) fn print_summary(&self) {
        println!("To rename:               {}", self.to_rename);
        println!("To remux:                {}", self.to_remux);
        println!("To subtitle mux:         {}", self.to_subtitle_mux);
        println!("To convert:              {}", self.to_convert);
        if self.total_skipped() > 0 {
            println!("Skipped:                 {}", self.total_skipped());
            println!(" - Already converted:    {}", self.skipped_converted);
            if self.skipped_bitrate_low > 0 {
                println!(" - Below bitrate limit:  {}", self.skipped_bitrate_low);
            }
            if self.skipped_bitrate_high > 0 {
                println!(" - Above bitrate limit:  {}", self.skipped_bitrate_high);
            }
            if self.skipped_duration_short > 0 {
                println!(" - Below duration limit: {}", self.skipped_duration_short);
            }
            if self.skipped_duration_long > 0 {
                println!(" - Above duration limit: {}", self.skipped_duration_long);
            }
            if self.skipped_resolution_low > 0 {
                println!(" - Below resolution limit: {}", self.skipped_resolution_low);
            }
            if self.skipped_duplicate > 0 {
                println!(" - Output exists:        {}", self.skipped_duplicate);
            }
            if self.duplicates_deleted > 0 {
                println!(" - Duplicates deleted:   {}", self.duplicates_deleted);
            }
            if self.duplicate_delete_failed > 0 {
                println!(
                    "{}",
                    format!(" - Delete failed:        {}", self.duplicate_delete_failed).red()
                );
            }
            if self.file_missing > 0 {
                println!(" - File missing:         {}", self.file_missing);
            }
        }
        if self.analysis_failed > 0 {
            println!("{}", format!("Analysis failed:         {}", self.analysis_failed).red());
        }
    }
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
            ProcessResult::SubtitlesMuxed { .. } => {
                self.files_subtitle_muxed += 1;
            }
            ProcessResult::Failed { .. } => {
                self.files_failed += 1;
            }
        }
    }

    /// Calculate total space saved (negative if size increased).
    #[allow(clippy::cast_possible_wrap)]
    pub(crate) const fn space_saved(&self) -> i64 {
        self.total_original_size as i64 - self.total_converted_size as i64
    }

    /// Print a summary of conversion statistics.
    pub(crate) fn print_summary(&self) {
        println!("{}", "\n--- Conversion Summary ---".bold().magenta());
        println!("Files renamed:           {}", self.files_renamed);
        println!("Files remuxed:           {}", self.files_remuxed);
        println!("Files subtitle muxed:    {}", self.files_subtitle_muxed);
        println!("Files converted:         {}", self.files_converted);
        println!(
            "Files failed:            {}",
            if self.files_failed > 0 {
                self.files_failed.to_string().red()
            } else {
                "0".normal()
            }
        );
        println!();

        if self.files_converted > 0 {
            let original_str = cli_tools::format_size(self.total_original_size);
            let converted_str = cli_tools::format_size(self.total_converted_size);

            if self.total_original_size > 0 {
                let saved = self.space_saved();
                let ratio = saved as f64 / self.total_original_size as f64 * 100.0;
                let saved_str = cli_tools::format_size(saved.unsigned_abs());

                let max_width = original_str.len().max(converted_str.len()).max(saved_str.len());

                println!("Total original size:     {original_str:>max_width$}");
                println!("Total converted size:    {converted_str:>max_width$}");

                if saved >= 0 {
                    println!("Space saved:             {saved_str:>max_width$} ({ratio:.1}%)");
                } else {
                    println!("Space increased:         {saved_str:>max_width$} ({ratio:.1}%)");
                }
            } else {
                println!("Total original size:     {original_str}");
                println!("Total converted size:    {converted_str}");
            }
        }

        println!(
            "Total time:              {}",
            cli_tools::format_duration(self.total_duration)
        );
    }
}

impl std::fmt::Display for ConversionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} @ {:.2} Mbps -> {} @ {:.2} Mbps ({:.1}%)",
            cli_tools::format_size(self.original_size),
            self.original_bitrate_kbps as f64 / 1000.0,
            cli_tools::format_size(self.converted_size),
            self.converted_bitrate_kbps as f64 / 1000.0,
            self.change_percentage(),
        )
    }
}

impl AddAssign<ConversionStats> for RunStats {
    fn add_assign(&mut self, stats: ConversionStats) {
        self.total_original_size += stats.original_size;
        self.total_converted_size += stats.converted_size;
    }
}

impl AddAssign<Self> for RunStats {
    fn add_assign(&mut self, other: Self) {
        self.files_converted += other.files_converted;
        self.files_remuxed += other.files_remuxed;
        self.files_subtitle_muxed += other.files_subtitle_muxed;
        self.files_renamed += other.files_renamed;
        self.files_failed += other.files_failed;
        self.total_original_size += other.total_original_size;
        self.total_converted_size += other.total_converted_size;
        self.total_duration += other.total_duration;
    }
}

impl AddAssign<&Self> for RunStats {
    fn add_assign(&mut self, other: &Self) {
        self.files_converted += other.files_converted;
        self.files_remuxed += other.files_remuxed;
        self.files_subtitle_muxed += other.files_subtitle_muxed;
        self.files_renamed += other.files_renamed;
        self.files_failed += other.files_failed;
        self.total_original_size += other.total_original_size;
        self.total_converted_size += other.total_converted_size;
        self.total_duration += other.total_duration;
    }
}

#[cfg(test)]
mod conversion_stats_tests {
    use super::*;

    #[test]
    fn new_creates_stats_with_values() {
        let stats = ConversionStats::new(1_000_000, 8000, 500_000, 4000);
        assert_eq!(stats.original_size, 1_000_000);
        assert_eq!(stats.original_bitrate_kbps, 8000);
        assert_eq!(stats.converted_size, 500_000);
        assert_eq!(stats.converted_bitrate_kbps, 4000);
    }

    #[test]
    fn size_difference_positive_when_larger() {
        let stats = ConversionStats::new(500_000, 4000, 1_000_000, 8000);
        assert_eq!(stats.size_difference(), 500_000);
    }

    #[test]
    fn size_difference_negative_when_smaller() {
        let stats = ConversionStats::new(1_000_000, 8000, 500_000, 4000);
        assert_eq!(stats.size_difference(), -500_000);
    }

    #[test]
    fn change_percentage_calculates_reduction() {
        let stats = ConversionStats::new(1_000_000, 8000, 500_000, 4000);
        let percentage = stats.change_percentage();
        assert!((percentage - (-50.0)).abs() < 0.01);
    }

    #[test]
    fn change_percentage_calculates_increase() {
        let stats = ConversionStats::new(500_000, 4000, 750_000, 6000);
        let percentage = stats.change_percentage();
        assert!((percentage - 50.0).abs() < 0.01);
    }

    #[test]
    fn change_percentage_zero_when_original_zero() {
        let stats = ConversionStats::new(0, 0, 500_000, 4000);
        assert!((stats.change_percentage() - 0.0).abs() < 0.01);
    }

    #[test]
    fn change_percentage_zero_when_converted_zero() {
        let stats = ConversionStats::new(500_000, 4000, 0, 0);
        assert!((stats.change_percentage() - 0.0).abs() < 0.01);
    }

    #[test]
    fn display_formats_correctly() {
        let stats = ConversionStats::new(1_048_576, 8000, 524_288, 4000);
        let display = format!("{stats}");
        assert!(display.contains("1.00 MB"));
        assert!(display.contains("512.00 KB"));
        assert!(display.contains("8.00 Mbps"));
        assert!(display.contains("4.00 Mbps"));
        assert!(display.contains("-50.0%"));
    }
}

#[cfg(test)]
mod analysis_stats_tests {
    use super::*;

    #[test]
    fn default_values_are_zero() {
        let stats = AnalysisStats::default();
        assert_eq!(stats.to_rename, 0);
        assert_eq!(stats.to_remux, 0);
        assert_eq!(stats.to_convert, 0);
        assert_eq!(stats.total_skipped(), 0);
    }

    #[test]
    fn total_skipped_sums_all_skip_reasons() {
        let stats = AnalysisStats {
            to_rename: 0,
            to_remux: 0,
            to_subtitle_mux: 0,
            to_convert: 0,
            skipped_converted: 5,
            skipped_bitrate_low: 3,
            skipped_bitrate_high: 2,
            skipped_duration_short: 4,
            skipped_duration_long: 1,
            skipped_resolution_low: 2,
            skipped_duplicate: 6,
            duplicates_deleted: 2,
            duplicate_delete_failed: 1,
            file_missing: 1,
            analysis_failed: 0,
        };
        assert_eq!(stats.total_skipped(), 27);
    }

    #[test]
    fn total_skipped_excludes_analysis_failed() {
        let stats = AnalysisStats {
            analysis_failed: 10,
            ..Default::default()
        };
        assert_eq!(stats.total_skipped(), 0);
    }
}

#[cfg(test)]
mod run_stats_tests {
    use super::*;

    #[test]
    fn default_values_are_zero() {
        let stats = RunStats::default();
        assert_eq!(stats.files_renamed, 0);
        assert_eq!(stats.files_remuxed, 0);
        assert_eq!(stats.files_subtitle_muxed, 0);
        assert_eq!(stats.files_converted, 0);
        assert_eq!(stats.files_failed, 0);
        assert_eq!(stats.total_original_size, 0);
        assert_eq!(stats.total_converted_size, 0);
        assert_eq!(stats.total_duration, Duration::ZERO);
    }

    #[test]
    fn space_saved_positive_when_smaller() {
        let stats = RunStats {
            total_original_size: 1_000_000,
            total_converted_size: 500_000,
            ..Default::default()
        };
        assert_eq!(stats.space_saved(), 500_000);
    }

    #[test]
    fn space_saved_negative_when_larger() {
        let stats = RunStats {
            total_original_size: 500_000,
            total_converted_size: 1_000_000,
            ..Default::default()
        };
        assert_eq!(stats.space_saved(), -500_000);
    }

    #[test]
    fn add_result_increments_converted() {
        let mut stats = RunStats::default();
        let result = ProcessResult::Converted {
            stats: ConversionStats::new(1_000_000, 8000, 500_000, 4000),
        };
        stats.add_result(&result, Duration::from_secs(10));

        assert_eq!(stats.files_converted, 1);
        assert_eq!(stats.total_original_size, 1_000_000);
        assert_eq!(stats.total_converted_size, 500_000);
        assert_eq!(stats.total_duration, Duration::from_secs(10));
    }

    #[test]
    fn add_result_increments_remuxed() {
        let mut stats = RunStats::default();
        let result = ProcessResult::Remuxed {};
        stats.add_result(&result, Duration::from_secs(5));

        assert_eq!(stats.files_remuxed, 1);
        assert_eq!(stats.total_duration, Duration::from_secs(5));
    }

    #[test]
    fn add_result_increments_failed() {
        let mut stats = RunStats::default();
        let result = ProcessResult::Failed {
            error: "test error".to_string(),
        };
        stats.add_result(&result, Duration::from_secs(1));

        assert_eq!(stats.files_failed, 1);
        assert_eq!(stats.total_duration, Duration::from_secs(1));
    }

    #[test]
    fn add_assign_conversion_stats() {
        let mut run_stats = RunStats::default();
        let conv_stats = ConversionStats::new(1_000_000, 8000, 500_000, 4000);
        run_stats += conv_stats;

        assert_eq!(run_stats.total_original_size, 1_000_000);
        assert_eq!(run_stats.total_converted_size, 500_000);
    }

    #[test]
    fn add_assign_run_stats() {
        let mut stats1 = RunStats {
            files_converted: 5,
            files_remuxed: 3,
            files_subtitle_muxed: 1,
            files_renamed: 2,
            files_failed: 1,
            total_original_size: 1_000_000,
            total_converted_size: 500_000,
            total_duration: Duration::from_secs(100),
        };
        let stats2 = RunStats {
            files_converted: 3,
            files_remuxed: 2,
            files_subtitle_muxed: 4,
            files_renamed: 1,
            files_failed: 0,
            total_original_size: 500_000,
            total_converted_size: 250_000,
            total_duration: Duration::from_secs(50),
        };
        stats1 += stats2;

        assert_eq!(stats1.files_converted, 8);
        assert_eq!(stats1.files_remuxed, 5);
        assert_eq!(stats1.files_subtitle_muxed, 5);
        assert_eq!(stats1.files_renamed, 3);
        assert_eq!(stats1.files_failed, 1);
        assert_eq!(stats1.total_original_size, 1_500_000);
        assert_eq!(stats1.total_converted_size, 750_000);
        assert_eq!(stats1.total_duration, Duration::from_secs(150));
    }

    #[test]
    fn add_assign_run_stats_ref() {
        let mut stats1 = RunStats {
            files_converted: 5,
            total_original_size: 1_000_000,
            ..Default::default()
        };
        let stats2 = RunStats {
            files_converted: 3,
            total_original_size: 500_000,
            ..Default::default()
        };
        stats1 += &stats2;

        assert_eq!(stats1.files_converted, 8);
        assert_eq!(stats1.total_original_size, 1_500_000);
    }
}

#[cfg(test)]
mod test_additional_run_results {
    use super::*;

    #[test]
    fn add_result_increments_subtitle_muxed() {
        let mut stats = RunStats::default();

        stats.add_result(&ProcessResult::SubtitlesMuxed {}, Duration::from_millis(250));

        assert_eq!(stats.files_subtitle_muxed, 1);
        assert_eq!(stats.total_duration, Duration::from_millis(250));
    }

    #[test]
    fn add_result_accumulates_mixed_outcomes_and_durations() {
        let mut stats = RunStats::default();
        let converted = ProcessResult::Converted {
            stats: ConversionStats::new(2_000, 8_000, 1_000, 4_000),
        };
        let remuxed = ProcessResult::Remuxed {};
        let subtitle_muxed = ProcessResult::SubtitlesMuxed {};
        let failed = ProcessResult::Failed {
            error: "invalid stream".to_string(),
        };

        stats.add_result(&converted, Duration::from_secs(4));
        stats.add_result(&remuxed, Duration::from_secs(3));
        stats.add_result(&subtitle_muxed, Duration::from_secs(2));
        stats.add_result(&failed, Duration::from_secs(1));

        assert_eq!(stats.files_converted, 1);
        assert_eq!(stats.files_remuxed, 1);
        assert_eq!(stats.files_subtitle_muxed, 1);
        assert_eq!(stats.files_failed, 1);
        assert_eq!(stats.total_original_size, 2_000);
        assert_eq!(stats.total_converted_size, 1_000);
        assert_eq!(stats.total_duration, Duration::from_secs(10));
    }
}

#[cfg(test)]
mod test_summary_branches {
    use super::*;

    #[test]
    fn analysis_summary_handles_every_reported_category() {
        let stats = AnalysisStats {
            to_rename: 1,
            to_remux: 2,
            to_subtitle_mux: 3,
            to_convert: 4,
            skipped_converted: 5,
            skipped_bitrate_low: 6,
            skipped_bitrate_high: 7,
            skipped_duration_short: 8,
            skipped_duration_long: 9,
            skipped_resolution_low: 10,
            skipped_duplicate: 11,
            duplicates_deleted: 12,
            duplicate_delete_failed: 13,
            file_missing: 14,
            analysis_failed: 15,
        };

        assert_eq!(stats.total_skipped(), 95);
        stats.print_summary();
    }

    #[test]
    fn analysis_summary_handles_no_skips_or_failures() {
        let stats = AnalysisStats::default();

        assert_eq!(stats.total_skipped(), 0);
        stats.print_summary();
    }

    #[test]
    fn run_summary_handles_saved_increased_and_zero_original_sizes() {
        let saved = RunStats {
            files_converted: 1,
            total_original_size: 2_097_152,
            total_converted_size: 1_048_576,
            ..Default::default()
        };
        let increased = RunStats {
            files_converted: 1,
            files_failed: 1,
            total_original_size: 524_288,
            total_converted_size: 1_048_576,
            ..Default::default()
        };
        let zero_original = RunStats {
            files_converted: 1,
            total_converted_size: 1_024,
            ..Default::default()
        };

        assert_eq!(saved.space_saved(), 1_048_576);
        assert_eq!(increased.space_saved(), -524_288);
        assert_eq!(zero_original.space_saved(), -1_024);
        saved.print_summary();
        increased.print_summary();
        zero_original.print_summary();
        RunStats::default().print_summary();
    }
}

#[cfg(test)]
mod test_additional_conversion_display {
    use super::*;

    #[test]
    fn display_formats_size_increase() {
        let stats = ConversionStats::new(524_288, 4_000, 1_048_576, 8_000);

        assert_eq!(
            stats.to_string(),
            "512.00 KB @ 4.00 Mbps -> 1.00 MB @ 8.00 Mbps (100.0%)"
        );
    }

    #[test]
    fn display_formats_zero_sized_conversion() {
        let stats = ConversionStats::new(0, 0, 0, 0);

        assert_eq!(stats.to_string(), "0 B @ 0.00 Mbps -> 0 B @ 0.00 Mbps (0.0%)");
    }
}
