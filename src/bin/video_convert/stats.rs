use std::ops::AddAssign;
use std::time::Duration;

use colored::Colorize;

use crate::convert::ProcessResult;

/// Statistics from the video file analysis.
#[derive(Debug, Default)]
pub struct AnalysisStats {
    pub(crate) to_rename: usize,
    pub(crate) to_remux: usize,
    pub(crate) to_convert: usize,
    pub(crate) skipped_converted: usize,
    pub(crate) skipped_bitrate_low: usize,
    pub(crate) skipped_bitrate_high: usize,
    pub(crate) skipped_duration_short: usize,
    pub(crate) skipped_duration_long: usize,
    pub(crate) skipped_duplicate: usize,
    pub(crate) analysis_failed: usize,
}

/// Statistics for the conversion run
#[derive(Debug, Default)]
pub struct RunStats {
    pub(crate) files_renamed: usize,
    pub(crate) files_remuxed: usize,
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
            + self.skipped_duplicate
    }

    /// Print analysis summary.
    pub(crate) fn print_summary(&self) {
        println!("To rename:               {}", self.to_rename);
        println!("To remux:                {}", self.to_remux);
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
            if self.skipped_duplicate > 0 {
                println!(" - Output exists:        {}", self.skipped_duplicate);
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
        self.files_renamed += other.files_renamed;
        self.files_failed += other.files_failed;
        self.total_original_size += other.total_original_size;
        self.total_converted_size += other.total_converted_size;
        self.total_duration += other.total_duration;
    }
}
