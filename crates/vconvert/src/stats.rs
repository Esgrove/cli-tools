//! Analysis and conversion statistics for video processing.
//!
//! Collects per-file and aggregate outcomes of the analysis and of the processing.

use std::ops::AddAssign;
use std::time::Duration;

use crate::types::{ProcessResult, SkipReason};

/// Statistics from the video file analysis.
#[derive(Debug, Default)]
pub struct AnalysisStats {
    pub to_rename: usize,
    pub to_remux: usize,
    pub to_subtitle_mux: usize,
    pub to_convert: usize,
    pub skipped_converted: usize,
    pub skipped_bitrate_low: usize,
    pub skipped_bitrate_high: usize,
    pub skipped_duration_short: usize,
    pub skipped_duration_long: usize,
    pub skipped_resolution_low: usize,
    pub skipped_duplicate: usize,
    pub skipped_output_conflict: usize,
    pub duplicates_deleted: usize,
    pub duplicate_delete_failed: usize,
    pub file_missing: usize,
    pub analysis_failed: usize,
}

/// Statistics for the conversion run
#[derive(Debug, Default)]
pub struct RunStats {
    pub files_renamed: usize,
    pub files_remuxed: usize,
    pub files_subtitle_muxed: usize,
    pub files_converted: usize,
    /// Files successfully deleted or previewed for deletion as verified duplicates.
    pub duplicates_deleted: usize,
    /// Files that encountered a processing error, including analysis and rename failures.
    pub files_failed: usize,
    pub total_original_size: u64,
    pub total_converted_size: u64,
    pub total_duration: Duration,
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
    #[must_use]
    pub const fn total_skipped(&self) -> usize {
        self.skipped_converted
            + self.skipped_bitrate_low
            + self.skipped_bitrate_high
            + self.skipped_duration_short
            + self.skipped_duration_long
            + self.skipped_resolution_low
            + self.skipped_duplicate
            + self.skipped_output_conflict
            + self.duplicates_deleted
            + self.duplicate_delete_failed
            + self.file_missing
    }

    /// Count a skipped file under its reason.
    pub(crate) const fn count_skip(&mut self, reason: &SkipReason) {
        let counter = match reason {
            SkipReason::AlreadyConverted => &mut self.skipped_converted,
            SkipReason::BitrateBelowThreshold { .. } => &mut self.skipped_bitrate_low,
            SkipReason::BitrateAboveThreshold { .. } => &mut self.skipped_bitrate_high,
            SkipReason::DurationBelowThreshold { .. } => &mut self.skipped_duration_short,
            SkipReason::DurationAboveThreshold { .. } => &mut self.skipped_duration_long,
            SkipReason::ResolutionBelowLimit { .. } => &mut self.skipped_resolution_low,
            SkipReason::OutputExists { .. } => &mut self.skipped_duplicate,
            SkipReason::OutputConflict { .. } => &mut self.skipped_output_conflict,
            SkipReason::FileMissing => &mut self.file_missing,
            SkipReason::AnalysisFailed { .. } => &mut self.analysis_failed,
        };
        *counter += 1;
    }
}

impl RunStats {
    /// Whether any processing operation succeeded or was previewed successfully.
    #[must_use]
    pub const fn has_successes(&self) -> bool {
        self.files_renamed
            + self.files_remuxed
            + self.files_subtitle_muxed
            + self.files_converted
            + self.duplicates_deleted
            > 0
    }

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
    #[must_use]
    #[allow(clippy::cast_possible_wrap)]
    pub const fn space_saved(&self) -> i64 {
        self.total_original_size as i64 - self.total_converted_size as i64
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
        self.duplicates_deleted += other.duplicates_deleted;
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
        self.duplicates_deleted += other.duplicates_deleted;
        self.files_failed += other.files_failed;
        self.total_original_size += other.total_original_size;
        self.total_converted_size += other.total_converted_size;
        self.total_duration += other.total_duration;
    }
}

impl ConversionStats {
    /// Create conversion stats with original and converted file sizes and bitrates.
    #[must_use]
    pub const fn new(
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

impl std::fmt::Display for ConversionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} @ {:.2} Mbps -> {} @ {:.2} Mbps ({:.1}%)",
            cli_tools_common::format_size(self.original_size),
            self.original_bitrate_kbps as f64 / 1000.0,
            cli_tools_common::format_size(self.converted_size),
            self.converted_bitrate_kbps as f64 / 1000.0,
            self.change_percentage(),
        )
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
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn count_skip_counts_each_reason_once() {
        let reasons = [
            SkipReason::AlreadyConverted,
            SkipReason::BitrateBelowThreshold {
                bitrate: 1,
                threshold: 2,
            },
            SkipReason::BitrateAboveThreshold {
                bitrate: 2,
                threshold: 1,
            },
            SkipReason::DurationBelowThreshold {
                duration: 1.0,
                threshold: 2.0,
            },
            SkipReason::DurationAboveThreshold {
                duration: 2.0,
                threshold: 1.0,
            },
            SkipReason::ResolutionBelowLimit {
                width: 640,
                height: 360,
                limit: 720,
            },
            SkipReason::OutputExists {
                path: PathBuf::from("out.mp4"),
                source_duration: 1.0,
            },
            SkipReason::OutputConflict {
                path: PathBuf::from("out.mp4"),
            },
            SkipReason::FileMissing,
            SkipReason::AnalysisFailed {
                error: "probe failed".to_string(),
            },
        ];
        let mut stats = AnalysisStats::default();

        for reason in &reasons {
            stats.count_skip(reason);
        }

        assert_eq!(stats.total_skipped(), 9);
        assert_eq!(stats.analysis_failed, 1);
        assert_eq!(stats.skipped_duplicate, 1);
        assert_eq!(stats.skipped_output_conflict, 1);
    }

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
            skipped_output_conflict: 3,
            duplicates_deleted: 2,
            duplicate_delete_failed: 1,
            file_missing: 1,
            analysis_failed: 0,
        };
        assert_eq!(stats.total_skipped(), 30);
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
            duplicates_deleted: 1,
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
            duplicates_deleted: 1,
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
        assert_eq!(stats1.duplicates_deleted, 2);
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
