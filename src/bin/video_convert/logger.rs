use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Local;

use crate::config::Config;
use crate::convert::VideoInfo;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};

/// Simple file logger for conversion operations.
/// Creates a new file for each run.
/// Outputs to ~/logs/cli-tools/video_convert_<timestamp>.log
pub struct FileLogger {
    writer: BufWriter<File>,
}

impl FileLogger {
    /// Create a new log file to ~/logs/cli-tools/video_convert_<timestamp>.log
    pub(crate) fn new() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        let log_dir = home_dir.join("logs").join("cli-tools");

        // Create log directory if it doesn't exist
        if !log_dir.exists() {
            fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
        }

        let log_path = log_dir.join(format!(
            "video_convert_{}.log",
            Local::now().format("%Y-%m-%d_%H-%M-%S")
        ));

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to create log file: {}", log_path.display()))?;

        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    fn timestamp() -> String {
        Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Log when starting the program
    pub(crate) fn log_init(&mut self, config: &Config) {
        let _ = writeln!(
            self.writer,
            "[{}] INIT \"{}\"",
            Self::timestamp(),
            config.path.display()
        );
        let _ = writeln!(self.writer, "  bitrate_limit: {}", config.bitrate_limit);
        let _ = writeln!(self.writer, "  convert_all: {}", config.convert_all);
        let _ = writeln!(self.writer, "  convert_other: {}", config.convert_other);
        if !config.include.is_empty() {
            let _ = writeln!(self.writer, "  include: {:?}", config.include);
        }
        if !config.exclude.is_empty() {
            let _ = writeln!(self.writer, "  exclude: {:?}", config.exclude);
        }
        let _ = writeln!(self.writer, "  extensions: {:?}", config.extensions);
        let _ = writeln!(self.writer, "  recurse: {}", config.recurse);
        let _ = writeln!(self.writer, "  delete: {}", config.delete);
        let _ = writeln!(self.writer, "  overwrite: {}", config.overwrite);
        let _ = writeln!(self.writer, "  dryrun: {}", config.dryrun);
        let _ = writeln!(self.writer, "  number: {}", config.number);
        let _ = writeln!(self.writer, "  verbose: {}", config.verbose);
        let _ = self.writer.flush();
    }

    /// Log when starting a conversion or remux operation
    pub(crate) fn log_start(
        &mut self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        info: &VideoInfo,
        quality_level: Option<u8>,
    ) {
        let _ = writeln!(
            self.writer,
            "[{}] START   {} {} - \"{}\" | {} {}x{} {:.2} Mbps {:.0} FPS{}",
            Self::timestamp(),
            operation.to_uppercase(),
            file_index,
            file_path.display(),
            info.codec,
            info.width,
            info.height,
            info.bitrate_kbps as f64 / 1000.0,
            info.frames_per_second,
            quality_level.map_or_else(String::new, |q| format!(" | Level: {q}"))
        );
        let _ = self.writer.flush();
    }

    /// Log when a conversion or remux finishes successfully
    pub(crate) fn log_success(
        &mut self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        duration: Duration,
        stats: Option<&ConversionStats>,
    ) {
        let _ = writeln!(
            self.writer,
            "[{}] SUCCESS {} {} - \"{}\" | Time: {}{}",
            Self::timestamp(),
            operation.to_uppercase(),
            file_index,
            file_path.display(),
            cli_tools::format_duration(duration),
            stats.map_or(String::new(), |s| format!(" | {s}"))
        );
        let _ = self.writer.flush();
    }

    /// Log when a conversion or remux fails
    pub(crate) fn log_failure(&mut self, file_path: &Path, operation: &str, file_index: &str, error: &str) {
        let _ = writeln!(
            self.writer,
            "[{}] ERROR   {} {} - \"{}\" | {}",
            Self::timestamp(),
            operation.to_uppercase(),
            file_index,
            file_path.display(),
            error
        );
        let _ = self.writer.flush();
    }

    /// Log analysis phase statistics
    pub(crate) fn log_analysis_stats(&mut self, stats: &AnalysisStats) {
        let _ = writeln!(self.writer, "[{}] ANALYSIS COMPLETE", Self::timestamp());
        let _ = writeln!(self.writer, "  Files to convert:      {}", stats.to_convert);
        let _ = writeln!(self.writer, "  Files to remux:        {}", stats.to_remux);
        let _ = writeln!(self.writer, "  Files to rename:       {}", stats.to_rename);
        let _ = writeln!(self.writer, "  Files skipped:         {}", stats.total_skipped());
        if stats.total_skipped() > 0 {
            let _ = writeln!(self.writer, "    - Already converted: {}", stats.skipped_converted);
            let _ = writeln!(self.writer, "    - Below bitrate:     {}", stats.skipped_bitrate);
            let _ = writeln!(self.writer, "    - Output exists:     {}", stats.skipped_duplicate);
        }
        if stats.analysis_failed > 0 {
            let _ = writeln!(self.writer, "  Analysis failed:       {}", stats.analysis_failed);
        }
        let _ = self.writer.flush();
    }

    /// Log final statistics
    pub(crate) fn log_stats(&mut self, stats: &RunStats) {
        let _ = writeln!(self.writer, "[{}] STATISTICS", Self::timestamp());
        let _ = writeln!(self.writer, "  Files converted: {}", stats.files_converted);
        let _ = writeln!(self.writer, "  Files remuxed:   {}", stats.files_remuxed);
        let _ = writeln!(self.writer, "  Files failed:    {}", stats.files_failed);

        if stats.files_converted > 0 {
            let _ = writeln!(
                self.writer,
                "  Total original size:  {}",
                cli_tools::format_size(stats.total_original_size)
            );
            let _ = writeln!(
                self.writer,
                "  Total converted size: {}",
                cli_tools::format_size(stats.total_converted_size)
            );

            let saved = stats.space_saved();
            if saved >= 0 {
                let _ = writeln!(self.writer, "  Space saved: {}", cli_tools::format_size(saved as u64));
            } else {
                let _ = writeln!(
                    self.writer,
                    "  Space increased: {}",
                    cli_tools::format_size((-saved) as u64)
                );
            }
        }

        let _ = writeln!(
            self.writer,
            "  Total time: {}",
            cli_tools::format_duration(stats.total_duration)
        );
        let _ = writeln!(self.writer, "[{}] END", Self::timestamp());
        let _ = self.writer.flush();
    }
}
