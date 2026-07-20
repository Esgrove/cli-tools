//! File logging for video conversion runs.
//!
//! Records configuration, analysis summaries, processing outcomes, and conversion statistics.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Local;

use crate::config::Config;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};
use crate::types::VideoInfo;

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
        let _ = writeln!(self.writer, "  movie_mode: {}", config.movie_mode);
        let _ = writeln!(self.writer, "  delete: {}", config.delete);
        let _ = writeln!(self.writer, "  overwrite: {}", config.overwrite);
        let _ = writeln!(self.writer, "  dryrun: {}", config.dryrun);
        if let Some(count) = config.count {
            let _ = writeln!(self.writer, "  count: {count}");
        }
        let _ = writeln!(self.writer, "  verbose: {}", config.verbose);
        let _ = self.writer.flush();
    }

    /// Log file gathering results
    pub(crate) fn log_gathered_files(&mut self, file_count: usize, duration: Duration) {
        let _ = writeln!(
            self.writer,
            "[{}] GATHER FILES | {} files found in {}",
            Self::timestamp(),
            file_count,
            cli_tools::format_duration(duration)
        );
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
    pub(crate) fn log_analysis_stats(&mut self, stats: &AnalysisStats, total_files: usize, duration: Duration) {
        let _ = writeln!(
            self.writer,
            "[{}] ANALYSE FILES | {} files in {}",
            Self::timestamp(),
            total_files,
            cli_tools::format_duration(duration)
        );
        let _ = writeln!(self.writer, "  Files to convert:      {}", stats.to_convert);
        let _ = writeln!(self.writer, "  Files to remux:        {}", stats.to_remux);
        let _ = writeln!(self.writer, "  Files to subtitle mux: {}", stats.to_subtitle_mux);
        let _ = writeln!(self.writer, "  Files to rename:       {}", stats.to_rename);
        let _ = writeln!(self.writer, "  Files skipped:         {}", stats.total_skipped());
        if stats.total_skipped() > 0 {
            let _ = writeln!(self.writer, "    - Already converted: {}", stats.skipped_converted);
            let _ = writeln!(self.writer, "    - Below bitrate:     {}", stats.skipped_bitrate_low);
            let _ = writeln!(self.writer, "    - Above bitrate:     {}", stats.skipped_bitrate_high);
            let _ = writeln!(self.writer, "    - Below duration:    {}", stats.skipped_duration_short);
            let _ = writeln!(self.writer, "    - Above duration:    {}", stats.skipped_duration_long);
            let _ = writeln!(self.writer, "    - Output exists:     {}", stats.skipped_duplicate);
        }
        if stats.analysis_failed > 0 {
            let _ = writeln!(self.writer, "  Analysis failed:       {}", stats.analysis_failed);
        }
        let _ = self.writer.flush();
    }

    /// Log rename operation statistics
    pub(crate) fn log_renames(&mut self, renamed_count: usize, total_count: usize, duration: Duration) {
        let _ = writeln!(
            self.writer,
            "[{}] RENAMES COMPLETE | {}/{} files renamed in {}",
            Self::timestamp(),
            renamed_count,
            total_count,
            cli_tools::format_duration(duration)
        );
        let _ = self.writer.flush();
    }

    /// Log final statistics
    pub(crate) fn log_stats(&mut self, stats: &RunStats) {
        let _ = writeln!(self.writer, "[{}] STATISTICS", Self::timestamp());
        let _ = writeln!(self.writer, "  Files converted: {}", stats.files_converted);
        let _ = writeln!(self.writer, "  Files remuxed:        {}", stats.files_remuxed);
        let _ = writeln!(self.writer, "  Files subtitle muxed: {}", stats.files_subtitle_muxed);
        let _ = writeln!(self.writer, "  Files failed:         {}", stats.files_failed);

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

#[cfg(test)]
mod test_file_logger {
    use super::*;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    fn create_logger() -> (NamedTempFile, FileLogger) {
        let log_file = NamedTempFile::new().expect("Failed to create temporary log file");
        let writer = BufWriter::new(log_file.reopen().expect("Failed to reopen temporary log file"));
        (log_file, FileLogger { writer })
    }

    fn read_log(log_file: &NamedTempFile, logger: FileLogger) -> String {
        drop(logger);
        fs::read_to_string(log_file.path()).expect("Failed to read temporary log file")
    }

    fn video_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8_500,
            size_bytes: 1_048_576,
            duration: 120.0,
            width: 1920,
            height: 1080,
            frames_per_second: 23.976,
            bit_depth: 10,
            warning: None,
        }
    }

    #[test]
    fn logs_complete_initial_configuration() {
        let (log_file, mut logger) = create_logger();
        let config = Config {
            bitrate_limit: 6_000,
            convert_all: true,
            convert_other: true,
            count: Some(12),
            delete: true,
            dryrun: true,
            exclude: vec!["sample".to_string()],
            extensions: vec!["mp4".to_string(), "mkv".to_string()],
            include: vec!["movie".to_string()],
            movie_mode: true,
            overwrite: true,
            path: PathBuf::from("videos"),
            recurse: true,
            verbose: true,
            ..Default::default()
        };

        logger.log_init(&config);
        let contents = read_log(&log_file, logger);

        assert!(contents.contains("INIT \"videos\""));
        assert!(contents.contains("bitrate_limit: 6000"));
        assert!(contents.contains("convert_all: true"));
        assert!(contents.contains("convert_other: true"));
        assert!(contents.contains("include: [\"movie\"]"));
        assert!(contents.contains("exclude: [\"sample\"]"));
        assert!(contents.contains("extensions: [\"mp4\", \"mkv\"]"));
        assert!(contents.contains("recurse: true"));
        assert!(contents.contains("movie_mode: true"));
        assert!(contents.contains("delete: true"));
        assert!(contents.contains("overwrite: true"));
        assert!(contents.contains("dryrun: true"));
        assert!(contents.contains("count: 12"));
        assert!(contents.contains("verbose: true"));
    }

    #[test]
    fn logs_processing_events_with_and_without_optional_details() {
        let (log_file, mut logger) = create_logger();
        let info = video_info();
        let conversion_stats = ConversionStats::new(1_048_576, 8_500, 524_288, 4_000);

        logger.log_gathered_files(3, Duration::from_millis(1_500));
        logger.log_start(Path::new("movie.mkv"), "convert", "1/2", &info, Some(23));
        logger.log_start(Path::new("bonus.mkv"), "remux", "2/2", &info, None);
        logger.log_success(
            Path::new("movie.mkv"),
            "convert",
            "1/2",
            Duration::from_secs(65),
            Some(&conversion_stats),
        );
        logger.log_success(Path::new("bonus.mkv"), "remux", "2/2", Duration::from_secs(2), None);
        logger.log_failure(Path::new("broken.mkv"), "convert", "3/3", "invalid stream");
        logger.log_renames(2, 3, Duration::from_secs(1));

        let contents = read_log(&log_file, logger);
        assert!(contents.contains("GATHER FILES | 3 files found"));
        assert!(contents.contains("START   CONVERT 1/2 - \"movie.mkv\""));
        assert!(contents.contains("h264 1920x1080 8.50 Mbps 24 FPS | Level: 23"));

        let remux_start = contents
            .lines()
            .find(|line| line.contains("START   REMUX"))
            .expect("Expected remux start entry");
        assert!(!remux_start.contains("Level:"));

        assert!(contents.contains("SUCCESS CONVERT 1/2 - \"movie.mkv\""));
        assert!(contents.contains("1.00 MB @ 8.50 Mbps -> 512.00 KB @ 4.00 Mbps (-50.0%)"));

        let remux_success = contents
            .lines()
            .find(|line| line.contains("SUCCESS REMUX"))
            .expect("Expected remux success entry");
        assert!(!remux_success.contains("->"));

        assert!(contents.contains("ERROR   CONVERT 3/3 - \"broken.mkv\" | invalid stream"));
        assert!(contents.contains("RENAMES COMPLETE | 2/3 files renamed"));
    }

    #[test]
    fn logs_analysis_and_run_statistic_branches() {
        let (log_file, mut logger) = create_logger();
        let analysis_stats = AnalysisStats {
            to_convert: 4,
            to_remux: 3,
            to_subtitle_mux: 2,
            to_rename: 1,
            skipped_converted: 1,
            skipped_bitrate_low: 2,
            skipped_bitrate_high: 3,
            skipped_duration_short: 4,
            skipped_duration_long: 5,
            skipped_duplicate: 6,
            analysis_failed: 7,
            ..Default::default()
        };

        logger.log_analysis_stats(&analysis_stats, 38, Duration::from_secs(3));
        logger.log_analysis_stats(&AnalysisStats::default(), 0, Duration::ZERO);
        logger.log_stats(&RunStats::default());
        logger.log_stats(&RunStats {
            files_converted: 2,
            files_remuxed: 1,
            files_subtitle_muxed: 1,
            files_failed: 1,
            total_original_size: 2_097_152,
            total_converted_size: 1_048_576,
            total_duration: Duration::from_secs(90),
            ..Default::default()
        });
        logger.log_stats(&RunStats {
            files_converted: 1,
            total_original_size: 524_288,
            total_converted_size: 1_048_576,
            total_duration: Duration::from_secs(4),
            ..Default::default()
        });

        let contents = read_log(&log_file, logger);
        assert!(contents.contains("ANALYSE FILES | 38 files"));
        assert!(contents.contains("Files to convert:      4"));
        assert!(contents.contains("Files to remux:        3"));
        assert!(contents.contains("Files to subtitle mux: 2"));
        assert!(contents.contains("Files skipped:         21"));
        assert!(contents.contains("Analysis failed:       7"));
        assert_eq!(contents.matches("STATISTICS").count(), 3);
        assert!(contents.contains("Files converted: 2"));
        assert!(contents.contains("Files remuxed:        1"));
        assert!(contents.contains("Files subtitle muxed: 1"));
        assert!(contents.contains("Files failed:         1"));
        assert!(contents.contains("Space saved: 1.00 MB"));
        assert!(contents.contains("Space increased: 512.00 KB"));
        assert_eq!(contents.matches(" END").count(), 3);
    }
}
