//! File logging for video conversion runs.
//!
//! Records configuration, analysis summaries, processing outcomes, and conversion statistics.
//! The logger is a `ConvertObserver`, so it receives the same progress events as the terminal output.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use cli_tools_common::print_yellow;
use cli_tools_vconvert::{
    AnalysisStats, ConversionStats, ConvertObserver, FilePosition, Operation, ProcessResult, ProcessableFile, RunStats,
    VideoInfo,
};
use jiff::ZonedDateTime;

use crate::config::Config;
use crate::reporter::format_file_position;

/// Simple file logger for conversion operations.
/// Creates a new file for each run.
/// Outputs to ~/logs/cli-tools/video_convert_<timestamp>.log
pub struct FileLogger {
    state: Mutex<LogState>,
}

/// The log file and whether writing to it has failed.
struct LogState {
    writer: Option<BufWriter<File>>,
    /// Set after the first failed write, so the warning is printed only once.
    write_failed: bool,
}

impl FileLogger {
    /// Create a no-op logger that writes nothing to disk (used for dry runs).
    pub(crate) const fn noop() -> Self {
        Self {
            state: Mutex::new(LogState {
                writer: None,
                write_failed: false,
            }),
        }
    }

    /// Create a new log file at `~/logs/cli-tools/video_convert_<timestamp>_pid<pid>.log`.
    pub(crate) fn new() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        let log_dir = home_dir.join("logs").join("cli-tools");

        if !log_dir.exists() {
            fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
        }

        let log_path = log_dir.join(format!(
            "video_convert_{}_pid{}.log",
            ZonedDateTime::now().strftime("%Y-%m-%d_%H-%M-%S"),
            std::process::id(),
        ));

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to create log file: {}", log_path.display()))?;

        Ok(Self::from_writer(BufWriter::new(file)))
    }

    /// Logger writing to a temporary file, which lives as long as the returned handle.
    #[cfg(test)]
    pub(crate) fn temporary() -> (tempfile::NamedTempFile, Self) {
        let log_file = tempfile::NamedTempFile::new().expect("Failed to create temporary log file");
        let writer = BufWriter::new(log_file.reopen().expect("Failed to reopen temporary log file"));
        (log_file, Self::from_writer(writer))
    }

    /// Logger writing to the given writer.
    const fn from_writer(writer: BufWriter<File>) -> Self {
        Self {
            state: Mutex::new(LogState {
                writer: Some(writer),
                write_failed: false,
            }),
        }
    }

    /// Log when starting the program
    pub(crate) fn log_init(&self, config: &Config) {
        self.write_entry(|writer| {
            write!(writer, "[{}] INIT", Self::timestamp())?;
            let options = &config.options;
            for path in &options.paths {
                write!(writer, " \"{}\"", path.display())?;
            }
            writeln!(writer)?;
            writeln!(writer, "  min_bitrate: {}", options.min_bitrate)?;
            writeln!(writer, "  convert_all: {}", config.convert_all)?;
            writeln!(writer, "  convert_other: {}", config.convert_other)?;
            if !options.include.is_empty() {
                writeln!(writer, "  include: {:?}", options.include)?;
            }
            if !options.exclude.is_empty() {
                writeln!(writer, "  exclude: {:?}", options.exclude)?;
            }
            writeln!(writer, "  extensions: {:?}", options.extensions)?;
            writeln!(writer, "  recurse: {}", options.recurse)?;
            writeln!(writer, "  movie_mode: {}", options.movie_mode)?;
            writeln!(writer, "  delete: {}", options.delete)?;
            writeln!(writer, "  overwrite: {}", options.overwrite)?;
            writeln!(writer, "  dryrun: {}", options.dry_run)?;
            if let Some(count) = options.count {
                writeln!(writer, "  count: {count}")?;
            }
            writeln!(writer, "  verbose: {}", config.verbose)
        });
    }

    /// Log file gathering results
    pub(crate) fn log_gathered_files(&self, file_count: usize, duration: Duration) {
        self.write_entry(|writer| {
            writeln!(
                writer,
                "[{}] GATHER FILES | {} files found in {}",
                Self::timestamp(),
                file_count,
                cli_tools_common::format_duration(duration)
            )
        });
    }

    /// Log when starting a conversion or remux operation
    pub(crate) fn log_start(
        &self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        info: &VideoInfo,
        quality_level: Option<u8>,
    ) {
        self.write_entry(|writer| {
            writeln!(
                writer,
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
            )
        });
    }

    /// Log when a conversion or remux finishes successfully
    pub(crate) fn log_success(
        &self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        duration: Duration,
        stats: Option<&ConversionStats>,
    ) {
        self.write_entry(|writer| {
            writeln!(
                writer,
                "[{}] SUCCESS {} {} - \"{}\" | Time: {}{}",
                Self::timestamp(),
                operation.to_uppercase(),
                file_index,
                file_path.display(),
                cli_tools_common::format_duration(duration),
                stats.map_or(String::new(), |s| format!(" | {s}"))
            )
        });
    }

    /// Log when a conversion or remux fails
    pub(crate) fn log_failure(&self, file_path: &Path, operation: &str, file_index: &str, error: &str) {
        self.write_entry(|writer| {
            writeln!(
                writer,
                "[{}] ERROR   {} {} - \"{}\" | {}",
                Self::timestamp(),
                operation.to_uppercase(),
                file_index,
                file_path.display(),
                error
            )
        });
    }

    /// Log analysis phase statistics
    pub(crate) fn log_analysis_stats(&self, stats: &AnalysisStats, total_files: usize, duration: Duration) {
        self.write_entry(|writer| {
            writeln!(
                writer,
                "[{}] ANALYSE FILES | {} files in {}",
                Self::timestamp(),
                total_files,
                cli_tools_common::format_duration(duration)
            )?;
            writeln!(writer, "  Files to convert:      {}", stats.to_convert)?;
            writeln!(writer, "  Files to remux:        {}", stats.to_remux)?;
            writeln!(writer, "  Files to subtitle mux: {}", stats.to_subtitle_mux)?;
            writeln!(writer, "  Files to rename:       {}", stats.to_rename)?;
            writeln!(writer, "  Files skipped:         {}", stats.total_skipped())?;
            if stats.total_skipped() > 0 {
                writeln!(writer, "    - Already converted: {}", stats.skipped_converted)?;
                writeln!(writer, "    - Below bitrate:     {}", stats.skipped_bitrate_low)?;
                writeln!(writer, "    - Above bitrate:     {}", stats.skipped_bitrate_high)?;
                writeln!(writer, "    - Below duration:    {}", stats.skipped_duration_short)?;
                writeln!(writer, "    - Above duration:    {}", stats.skipped_duration_long)?;
                writeln!(writer, "    - Output exists:     {}", stats.skipped_duplicate)?;
            }
            if stats.analysis_failed > 0 {
                writeln!(writer, "  Analysis failed:       {}", stats.analysis_failed)?;
            }
            Ok(())
        });
    }

    /// Log rename operation statistics
    pub(crate) fn log_renames(&self, renamed_count: usize, total_count: usize, duration: Duration) {
        self.write_entry(|writer| {
            writeln!(
                writer,
                "[{}] RENAMES COMPLETE | {}/{} files renamed in {}",
                Self::timestamp(),
                renamed_count,
                total_count,
                cli_tools_common::format_duration(duration)
            )
        });
    }

    /// Log final statistics
    pub(crate) fn log_stats(&self, stats: &RunStats) {
        self.write_entry(|writer| {
            writeln!(writer, "[{}] STATISTICS", Self::timestamp())?;
            writeln!(writer, "  Files renamed:        {}", stats.files_renamed)?;
            writeln!(writer, "  Files converted: {}", stats.files_converted)?;
            writeln!(writer, "  Files remuxed:        {}", stats.files_remuxed)?;
            writeln!(writer, "  Files subtitle muxed: {}", stats.files_subtitle_muxed)?;
            if stats.duplicates_deleted > 0 {
                writeln!(writer, "  Duplicates deleted:   {}", stats.duplicates_deleted)?;
            }
            writeln!(writer, "  Files failed:         {}", stats.files_failed)?;

            if stats.files_converted > 0 {
                writeln!(
                    writer,
                    "  Total original size:  {}",
                    cli_tools_common::format_size(stats.total_original_size)
                )?;
                writeln!(
                    writer,
                    "  Total converted size: {}",
                    cli_tools_common::format_size(stats.total_converted_size)
                )?;

                let saved = stats.space_saved();
                if saved >= 0 {
                    writeln!(writer, "  Space saved: {}", cli_tools_common::format_size(saved as u64))?;
                } else {
                    writeln!(
                        writer,
                        "  Space increased: {}",
                        cli_tools_common::format_size((-saved) as u64)
                    )?;
                }
            }

            writeln!(
                writer,
                "  Total time: {}",
                cli_tools_common::format_duration(stats.total_duration)
            )?;
            writeln!(writer, "[{}] END", Self::timestamp())
        });
    }

    /// Format the current local time for log entries.
    fn timestamp() -> String {
        ZonedDateTime::now().strftime("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Write one log entry and flush it, warning once if the log file cannot be written.
    fn write_entry(&self, write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(ref mut writer) = state.writer else {
            return;
        };
        let result = write(writer).and_then(|()| writer.flush());
        let first_failure = result.is_err() && !state.write_failed;
        if first_failure {
            state.write_failed = true;
        }
        drop(state);
        if let Err(error) = result
            && first_failure
        {
            print_yellow!("Failed to write to the log file: {error}");
        }
    }

    /// Whether a write to the log file has failed.
    #[cfg(test)]
    fn write_failed(&self) -> bool {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).write_failed
    }
}

impl ConvertObserver for FileLogger {
    fn files_gathered(&self, file_count: usize, duration: Duration) {
        self.log_gathered_files(file_count, duration);
    }

    fn analysis_finished(&self, stats: &AnalysisStats, file_count: usize, duration: Duration) {
        self.log_analysis_stats(stats, file_count, duration);
    }

    fn renames_finished(&self, renamed_count: usize, total_count: usize, duration: Duration) {
        self.log_renames(renamed_count, total_count, duration);
    }

    fn file_started(
        &self,
        operation: Operation,
        position: FilePosition,
        file: &ProcessableFile,
        quality_level: Option<u8>,
    ) {
        self.log_start(
            &file.file.path,
            operation.as_str(),
            &format_file_position(position),
            &file.info,
            quality_level,
        );
    }

    fn file_locked(&self, path: &Path, holder_tool: &str, holder_pid: u32) {
        self.write_entry(|writer| {
            writeln!(
                writer,
                "[{}] SKIPPED LOCKED ({holder_tool} pid {holder_pid}) | {}",
                Self::timestamp(),
                path.display()
            )
        });
    }

    fn file_finished(
        &self,
        operation: Operation,
        position: FilePosition,
        file: &ProcessableFile,
        result: &ProcessResult,
        duration: Duration,
    ) {
        let file_index = format_file_position(position);
        match result {
            ProcessResult::Failed { error } => {
                self.log_failure(&file.file.path, operation.as_str(), &file_index, error);
            }
            ProcessResult::Converted { stats } => {
                self.log_success(
                    &file.output_path,
                    operation.as_str(),
                    &file_index,
                    duration,
                    Some(stats),
                );
            }
            ProcessResult::Remuxed {} | ProcessResult::SubtitlesMuxed {} => {
                self.log_success(&file.output_path, operation.as_str(), &file_index, duration, None);
            }
        }
    }
}

#[cfg(test)]
mod test_file_logger {
    use std::path::PathBuf;

    use cli_tools_vconvert::{ConvertOptions, MinimumBitrate};
    use tempfile::NamedTempFile;

    use super::*;

    fn create_logger() -> (NamedTempFile, FileLogger) {
        FileLogger::temporary()
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
        let (log_file, logger) = create_logger();
        let config = Config {
            options: ConvertOptions {
                count: Some(12),
                delete: true,
                dry_run: true,
                exclude: vec!["sample".to_string()],
                extensions: vec!["mp4".to_string(), "mkv".to_string()],
                include: vec!["movie".to_string()],
                min_bitrate: MinimumBitrate::Fixed(6_000),
                movie_mode: true,
                overwrite: true,
                paths: vec![PathBuf::from("videos")],
                recurse: true,
                ..ConvertOptions::default()
            },
            convert_all: true,
            convert_other: true,
            verbose: true,
            ..Default::default()
        };

        logger.log_init(&config);
        let contents = read_log(&log_file, logger);

        assert!(contents.contains("INIT \"videos\""));
        assert!(contents.contains("min_bitrate: 6000 kbps"));
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
        let (log_file, logger) = create_logger();
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
        let (log_file, logger) = create_logger();
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

    #[test]
    fn write_failure_is_remembered_and_logging_continues() {
        let log_file = NamedTempFile::new().expect("Failed to create temporary log file");
        let read_only = File::open(log_file.path()).expect("Failed to open temporary log file read-only");
        let logger = FileLogger::from_writer(BufWriter::new(read_only));

        logger.log_gathered_files(1, Duration::ZERO);
        assert!(logger.write_failed());

        logger.log_renames(1, 1, Duration::ZERO);
        assert!(logger.write_failed());
    }

    #[test]
    fn successful_writes_leave_failure_flag_unset() {
        let (log_file, logger) = create_logger();

        logger.log_gathered_files(1, Duration::ZERO);

        assert!(!logger.write_failed());
        assert!(read_log(&log_file, logger).contains("GATHER FILES"));
    }
}
