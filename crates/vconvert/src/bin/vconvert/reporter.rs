//! Terminal output for video convert runs.
//!
//! The reporter is the `ConvertObserver` the binary passes to the converter.
//! It prints each progress event, shows the probe progress bar,
//! and forwards the events to the run log file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use cli_tools_common::{print_error, print_yellow};
use cli_tools_vconvert::{
    AnalysisStats, ConvertObserver, DeleteMethod, DiskSpaceShortage, DuplicateOutcome, FilePosition, Operation,
    ProcessResult, ProcessableFile, ProcessingOutcome, Retry, RunStats, RunSummary, SkipReason, SubtitleFile,
    SubtitleMatchProblem,
};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use crate::config::Config;
use crate::logger::FileLogger;

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Prints the progress of a run and writes it to the log file.
pub struct Reporter {
    verbose: bool,
    dry_run: bool,
    logger: FileLogger,
    /// Progress bar shown while ffprobe runs on the scan cache misses.
    progress_bar: Mutex<Option<ProgressBar>>,
    /// Sources whose output already exists, listed after the analysis in verbose mode.
    duplicate_pairs: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl Reporter {
    /// Reporter for the run configured by `config`, logging to `logger`.
    pub(crate) const fn new(config: &Config, logger: FileLogger) -> Self {
        Self {
            verbose: config.verbose,
            dry_run: config.options.dry_run,
            logger,
            progress_bar: Mutex::new(None),
            duplicate_pairs: Mutex::new(Vec::new()),
        }
    }

    /// Log the run statistics, and print why processing stopped early and the run summary.
    pub(crate) fn finish(&self, summary: &RunSummary) {
        self.logger.log_stats(&summary.stats);

        if summary.outcome != ProcessingOutcome::Completed {
            println!("\n{}", summary.outcome.to_string().bold().red());
        }

        print_run_summary(&summary.stats);
    }

    /// Print the source and output of every duplicate found during the analysis.
    fn print_duplicate_pairs(&self) {
        let duplicate_pairs = std::mem::take(&mut *self.duplicate_pairs.lock().unwrap_or_else(PoisonError::into_inner));
        if duplicate_pairs.is_empty() {
            return;
        }
        println!();
        println!("{}", "Duplicate pairs:".bold());
        for (source, target) in &duplicate_pairs {
            println!("  {}", cli_tools_common::path_to_string_relative(source));
            println!("  {}", cli_tools_common::path_to_string_relative(target));
            println!();
        }
    }
}

impl ConvertObserver for Reporter {
    fn files_gathered(&self, file_count: usize, duration: Duration) {
        self.logger.files_gathered(file_count, duration);
    }

    fn subtitle_not_matched(&self, subtitle: &Path, problem: SubtitleMatchProblem) {
        if !self.verbose {
            return;
        }
        match problem {
            SubtitleMatchProblem::NoMatchingVideo => {
                print_yellow!("No matching video found for subtitle: {}", subtitle.display());
            }
            SubtitleMatchProblem::Ambiguous => {
                print_yellow!("Ambiguous subtitle match skipped: {}", subtitle.display());
            }
        }
    }

    fn analysis_started(&self, file_count: usize) {
        if self.verbose {
            println!(
                "Found {}, analyzing...",
                cli_tools_common::count_label(file_count, "candidate file", "candidate files")
            );
        }
    }

    fn scan_cache_checked(&self, hits: usize, misses: usize) {
        if self.verbose && hits > 0 {
            println!(
                "Scan cache: {}, {} — running ffprobe on {}",
                cli_tools_common::count_label(hits, "hit", "hits"),
                cli_tools_common::count_label(misses, "miss", "misses"),
                cli_tools_common::count_label(misses, "file", "files")
            );
        }
    }

    fn probing_started(&self, file_count: usize) {
        let progress_bar = ProgressBar::new(file_count as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );
        *self.progress_bar.lock().unwrap_or_else(PoisonError::into_inner) = Some(progress_bar);
    }

    fn file_probed(&self) {
        if let Some(progress_bar) = &*self.progress_bar.lock().unwrap_or_else(PoisonError::into_inner) {
            progress_bar.inc(1);
        }
    }

    fn probing_finished(&self) {
        let progress_bar = self.progress_bar.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(progress_bar) = progress_bar {
            progress_bar.finish_and_clear();
        }
    }

    fn scan_cache_write_failed(&self, error: &anyhow::Error) {
        if self.verbose {
            print_yellow!("Failed to write scan cache: {error}");
        }
    }

    fn file_skipped(&self, path: &Path, reason: &SkipReason) {
        let path_display = cli_tools_common::path_to_string_relative(path);
        match reason {
            SkipReason::OutputExists { path: target, .. } => {
                if self.verbose {
                    self.duplicate_pairs
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((path.to_path_buf(), target.clone()));
                }
            }
            SkipReason::OutputConflict { .. } => print_yellow!("{path_display}: {reason}"),
            SkipReason::FileMissing => {
                print_yellow!("{path_display}: File no longer exists (may have been moved or renamed)");
            }
            SkipReason::AnalysisFailed { error } => print_error!("{path_display}: {error}"),
            _ => {
                if self.verbose {
                    print_yellow!("{path_display}: {reason}");
                }
            }
        }
    }

    fn duplicate_handled(&self, source: &Path, target: &Path, outcome: &DuplicateOutcome) {
        print_duplicate_outcome(source, target, outcome);
    }

    fn analysis_finished(&self, stats: &AnalysisStats, file_count: usize, duration: Duration) {
        self.print_duplicate_pairs();
        self.logger.analysis_finished(stats, file_count, duration);
        print_analysis_summary(stats);
    }

    fn rename_started(&self, position: FilePosition, from: &Path, to: &Path, dry_run: bool) {
        let file_index = format_file_position(position);
        let header = if dry_run {
            format!("{file_index} [DRYRUN] Rename:")
        } else {
            format!("{file_index} Rename:")
        };
        println!("{}", header.bold().purple());
        cli_tools_common::show_diff(
            &cli_tools_common::path_to_string_relative(from),
            &cli_tools_common::path_to_string_relative(to),
        );
    }

    fn rename_skipped_existing(&self, to: &Path) {
        print_yellow!("Skipping rename, file already exists: {}", to.display());
    }

    fn rename_failed(&self, from: &Path, error: &anyhow::Error) {
        print_error!("Failed to rename {}: {error}", from.display());
    }

    fn renames_finished(&self, renamed_count: usize, total_count: usize, duration: Duration) {
        self.logger.renames_finished(renamed_count, total_count, duration);
    }

    fn pending_files_saved(&self, updated_count: usize) {
        if self.verbose && updated_count > 0 {
            println!("Updated {updated_count} files in database");
        }
    }

    fn pending_files_save_failed(&self, error: &anyhow::Error) {
        if self.verbose {
            print_yellow!("Failed to update pending files in database: {error}");
        }
    }

    fn database_cleaned(&self, missing_count: usize, changed_count: usize) {
        if missing_count > 0 {
            println!(
                "{}",
                format!("Removed {missing_count} missing files from database").yellow()
            );
        }
        if changed_count > 0 {
            println!(
                "{}",
                format!(
                    "Removed {} from database, scan again to process them",
                    cli_tools_common::count_label(
                        changed_count,
                        "file changed since the scan",
                        "files changed since the scan"
                    )
                )
                .yellow()
            );
        }
    }

    fn pending_files_loaded(&self, file_count: usize) {
        if self.verbose {
            println!(
                "Processing {} from database",
                cli_tools_common::count_label(file_count, "pending file", "pending files")
            );
        }
    }

    fn pending_file_blocked(&self, path: &Path, reason: &SkipReason) {
        print_yellow!("{}: {reason}", cli_tools_common::path_to_string_relative(path));
    }

    fn database_removal_failed(&self, path: &Path, error: &anyhow::Error) {
        print_error!("Failed to remove {} from database: {error:#}", path.display());
    }

    fn file_started(
        &self,
        operation: Operation,
        position: FilePosition,
        file: &ProcessableFile,
        quality_level: Option<u8>,
    ) {
        let file_index = format_file_position(position);
        let input = cli_tools_common::path_to_string_relative(&file.file.path);
        let header = match operation {
            Operation::Remux => format!("{file_index} Remux: {input}").bold().green(),
            Operation::SubtitleMux => format!("{file_index} Process movie streams: {input}").bold().cyan(),
            Operation::Convert => format!("{file_index} Convert: {input}").bold().magenta(),
        };
        println!("{header}");
        println!("{}", file.info);

        if self.verbose {
            println!(
                "Output: {}",
                cli_tools_common::path_to_string_relative(&file.output_path)
            );
            if let Some(quality_level) = quality_level {
                println!("Using quality level: {quality_level}");
            }
            if operation != Operation::Remux {
                print_subtitle_files(&file.subtitle_files);
            }
        }

        self.logger.file_started(operation, position, file, quality_level);
    }

    fn dry_run_command(&self, command: &Command) {
        println!("[DRYRUN] {command:#?}");
    }

    fn retrying(&self, retry: Retry) {
        match retry {
            Retry::AacAudio { status } => {
                print_yellow!("Remux failed with code {status}. Retrying with AAC audio transcode...");
            }
            Retry::CpuFilters => print_error!("CUDA filter failed, retrying with CPU-based filtering..."),
            Retry::LowerQuality {
                output_size,
                input_size,
                quality_level,
            } => {
                print_yellow!(
                    "Output file ({}) is larger than input ({}), reconverting with lower quality level ({})",
                    cli_tools_common::format_size(output_size),
                    cli_tools_common::format_size(input_size),
                    quality_level
                );
            }
        }
    }

    fn file_finished(
        &self,
        operation: Operation,
        position: FilePosition,
        file: &ProcessableFile,
        result: &ProcessResult,
        duration: Duration,
    ) {
        let elapsed = cli_tools_common::format_duration(duration);
        match result {
            ProcessResult::Failed { error } => {
                print_error!(
                    "{}: {error}",
                    cli_tools_common::path_to_string_relative(&file.file.path)
                );
            }
            _ if self.dry_run => {}
            ProcessResult::Converted { stats } => {
                println!("{}", format!("✓ Converted in {elapsed}: {stats}").cyan());
            }
            ProcessResult::SubtitlesMuxed {} => {
                println!("{}", format!("✓ Processed movie streams in {elapsed}").green());
            }
            ProcessResult::Remuxed {} => {
                println!("{}", format!("✓ Remuxed in {elapsed}").green());
            }
        }

        if !self.dry_run || matches!(result, ProcessResult::Failed { .. }) {
            self.logger.file_finished(operation, position, file, result, duration);
        }
    }

    fn file_missing(&self, path: &Path) {
        print_yellow!("File no longer exists: {}", path.display());
    }

    fn file_locked(&self, path: &Path, holder_tool: &str, holder_pid: u32) {
        self.logger.file_locked(path, holder_tool, holder_pid);
        print_yellow!(
            "Skipping locked file (held by {holder_tool} pid {holder_pid}): {}",
            path.display()
        );
    }

    fn limit_reached(&self, limit: usize) {
        println!("{}", format!("\nReached file limit ({limit})").bold());
    }

    fn out_of_disk_space(&self, path: &Path, shortage: DiskSpaceShortage) {
        print_error!(
            "Out of disk space: converting {} needs {} free but only {} is available",
            cli_tools_common::path_to_string_relative(path),
            cli_tools_common::format_size(shortage.required),
            cli_tools_common::format_size(shortage.available),
        );
    }

    fn deleting(&self, path: &Path, method: DeleteMethod) {
        match method {
            DeleteMethod::Delete => println!("Deleting: {}", path.display()),
            DeleteMethod::Trash => println!("Trashing: {}", path.display()),
        }
    }

    fn delete_failed(&self, path: &Path, error: &anyhow::Error) {
        print_error!("Failed to delete {}: {error:#}", path.display());
    }

    fn partial_output_removal_failed(&self, path: &Path, error: &std::io::Error) {
        print_error!(
            "Failed to remove partial output {}: {error}",
            cli_tools_common::path_to_string_relative(path)
        );
    }
}

/// Format a file position as a right-aligned `[index/total]` label.
pub fn format_file_position(position: FilePosition) -> String {
    let width = position.total.checked_ilog10().map_or(1, |digits| digits as usize + 1);
    format!("[{:>width$}/{}]", position.index, position.total)
}

/// Print the paths of all supplied subtitle sidecar files.
fn print_subtitle_files(subtitle_files: &[SubtitleFile]) {
    if subtitle_files.is_empty() {
        return;
    }
    println!("Subtitles:");
    for subtitle_file in subtitle_files {
        println!("  - {}", cli_tools_common::path_to_string_relative(&subtitle_file.path));
    }
}

/// Print the outcome of a duplicate deletion for the source and its existing output.
fn print_duplicate_outcome(source: &Path, target: &Path, outcome: &DuplicateOutcome) {
    let source_display = cli_tools_common::path_to_string_relative(source);
    let target_display = cli_tools_common::path_to_string_relative(target);
    match outcome {
        DuplicateOutcome::Deleted { duration_message } => {
            println!(
                "{} ({duration_message})",
                format!("Deleted duplicate: {source_display}").green()
            );
        }
        DuplicateOutcome::WouldDelete { duration_message } => {
            println!(
                "{} ({duration_message})",
                format!("Would delete duplicate: {source_display}").yellow()
            );
        }
        DuplicateOutcome::RefusedSameFile => {
            print_error!(
                "Refusing to delete duplicate because source and target are the same file:\n  {source_display}"
            );
        }
        DuplicateOutcome::RefusedNotTargetCodec => {
            print_error!(
                "Refusing to delete duplicate because target is not HEVC/AV1:\n  Source: {source_display}\n  Target: {target_display}"
            );
        }
        DuplicateOutcome::DurationMismatch {
            source_duration,
            target_duration,
            difference_ratio,
        } => {
            print_error!(
                "Duration mismatch for duplicate - source: {:.1}s, target: {:.1}s ({:.3}% difference)\n  Source: {source_display}\n  Target: {target_display}",
                source_duration,
                target_duration,
                difference_ratio * 100.0,
            );
        }
        DuplicateOutcome::ProbeFailed { error } => {
            print_error!("Failed to get duration of target file {target_display}: {error}");
        }
        DuplicateOutcome::DeleteFailed { error } => {
            print_error!("Failed to delete duplicate {source_display}: {error}");
        }
    }
}

/// Print the analysis summary.
fn print_analysis_summary(stats: &AnalysisStats) {
    println!("To rename:               {}", stats.to_rename);
    println!("To remux:                {}", stats.to_remux);
    println!("To subtitle mux:         {}", stats.to_subtitle_mux);
    println!("To convert:              {}", stats.to_convert);
    if stats.total_skipped() > 0 {
        println!("Skipped:                 {}", stats.total_skipped());
        println!(" - Already converted:    {}", stats.skipped_converted);
        if stats.skipped_bitrate_low > 0 {
            println!(" - Below bitrate limit:  {}", stats.skipped_bitrate_low);
        }
        if stats.skipped_bitrate_high > 0 {
            println!(" - Above bitrate limit:  {}", stats.skipped_bitrate_high);
        }
        if stats.skipped_duration_short > 0 {
            println!(" - Below duration limit: {}", stats.skipped_duration_short);
        }
        if stats.skipped_duration_long > 0 {
            println!(" - Above duration limit: {}", stats.skipped_duration_long);
        }
        if stats.skipped_resolution_low > 0 {
            println!(" - Below resolution limit: {}", stats.skipped_resolution_low);
        }
        if stats.skipped_duplicate > 0 {
            println!(" - Output exists:        {}", stats.skipped_duplicate);
        }
        if stats.skipped_output_conflict > 0 {
            println!(" - Output conflict:      {}", stats.skipped_output_conflict);
        }
        if stats.duplicates_deleted > 0 {
            println!(" - Duplicates deleted:   {}", stats.duplicates_deleted);
        }
        if stats.duplicate_delete_failed > 0 {
            println!(
                "{}",
                format!(" - Delete failed:        {}", stats.duplicate_delete_failed).red()
            );
        }
        if stats.file_missing > 0 {
            println!(" - File missing:         {}", stats.file_missing);
        }
    }
    if stats.analysis_failed > 0 {
        println!(
            "{}",
            format!("Analysis failed:         {}", stats.analysis_failed).red()
        );
    }
}

/// Print the summary of the processed files.
fn print_run_summary(stats: &RunStats) {
    println!("{}", "\n--- Conversion Summary ---".bold().magenta());
    println!("Files renamed:           {}", stats.files_renamed);
    println!("Files remuxed:           {}", stats.files_remuxed);
    println!("Files subtitle muxed:    {}", stats.files_subtitle_muxed);
    println!("Files converted:         {}", stats.files_converted);
    if stats.duplicates_deleted > 0 {
        println!("Duplicates deleted:      {}", stats.duplicates_deleted);
    }
    println!(
        "Files failed:            {}",
        if stats.files_failed > 0 {
            stats.files_failed.to_string().red()
        } else {
            "0".normal()
        }
    );
    println!();

    if stats.files_converted > 0 {
        let original_str = cli_tools_common::format_size(stats.total_original_size);
        let converted_str = cli_tools_common::format_size(stats.total_converted_size);

        if stats.total_original_size > 0 {
            let saved = stats.space_saved();
            let ratio = saved as f64 / stats.total_original_size as f64 * 100.0;
            let saved_str = cli_tools_common::format_size(saved.unsigned_abs());

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
        cli_tools_common::format_duration(stats.total_duration)
    );
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
            skipped_output_conflict: 16,
            duplicates_deleted: 12,
            duplicate_delete_failed: 13,
            file_missing: 14,
            analysis_failed: 15,
        };

        assert_eq!(stats.total_skipped(), 111);
        print_analysis_summary(&stats);
    }

    #[test]
    fn analysis_summary_handles_no_skips_or_failures() {
        let stats = AnalysisStats::default();

        assert_eq!(stats.total_skipped(), 0);
        print_analysis_summary(&stats);
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
        print_run_summary(&saved);
        print_run_summary(&increased);
        print_run_summary(&zero_original);
        print_run_summary(&RunStats::default());
    }
}

#[cfg(test)]
mod test_format_file_position {
    use super::*;

    #[test]
    fn pads_the_index_to_the_width_of_the_total() {
        assert_eq!(format_file_position(FilePosition { index: 3, total: 120 }), "[  3/120]");
        assert_eq!(format_file_position(FilePosition { index: 1, total: 1 }), "[1/1]");
        assert_eq!(format_file_position(FilePosition { index: 0, total: 0 }), "[0/0]");
    }
}

#[cfg(test)]
mod test_reporter_events {
    use std::path::PathBuf;
    use std::process::Command;

    use cli_tools_vconvert::types::VideoFile;
    use cli_tools_vconvert::{ConversionStats, ConvertOptions, VideoInfo};

    use super::*;

    fn reporter(verbose: bool, dry_run: bool) -> (tempfile::NamedTempFile, Reporter) {
        let (log_file, logger) = FileLogger::temporary();
        let config = Config {
            options: ConvertOptions {
                dry_run,
                ..ConvertOptions::default()
            },
            verbose,
            ..Config::default()
        };
        (log_file, Reporter::new(&config, logger))
    }

    fn processable() -> ProcessableFile {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8_000,
            size_bytes: 1_024,
            duration: 60.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        ProcessableFile::for_mode(VideoFile::new(Path::new("Movie.mkv"), 1_024), info, Vec::new(), false)
    }

    fn read_log(log_file: &tempfile::NamedTempFile) -> String {
        std::fs::read_to_string(log_file.path()).unwrap()
    }

    #[test]
    fn processing_events_are_logged() {
        let (log_file, reporter) = reporter(true, false);
        let file = processable();
        let position = FilePosition { index: 1, total: 2 };
        let converted = ProcessResult::Converted {
            stats: ConversionStats::new(1_024, 8_000, 512, 4_000),
        };

        reporter.files_gathered(2, Duration::from_secs(1));
        reporter.file_started(Operation::Convert, position, &file, Some(24));
        reporter.retrying(Retry::CpuFilters);
        reporter.file_finished(Operation::Convert, position, &file, &converted, Duration::from_secs(5));
        reporter.file_finished(
            Operation::Remux,
            FilePosition { index: 2, total: 2 },
            &file,
            &ProcessResult::Failed {
                error: "ffmpeg failed".to_string(),
            },
            Duration::ZERO,
        );
        reporter.renames_finished(1, 1, Duration::ZERO);

        let contents = read_log(&log_file);
        assert!(contents.contains("GATHER FILES | 2 files found"));
        assert!(contents.contains("START   CONVERT [1/2]"));
        assert!(contents.contains("SUCCESS CONVERT [1/2]"));
        assert!(contents.contains("ERROR   REMUX [2/2]"));
        assert!(contents.contains("RENAMES COMPLETE | 1/1 files renamed"));
    }

    #[test]
    fn dry_run_successes_are_not_logged() {
        let (log_file, reporter) = reporter(false, true);
        let file = processable();
        let position = FilePosition { index: 1, total: 1 };

        reporter.file_started(Operation::Remux, position, &file, None);
        reporter.dry_run_command(&Command::new("ffmpeg"));
        reporter.file_finished(
            Operation::Remux,
            position,
            &file,
            &ProcessResult::Remuxed {},
            Duration::ZERO,
        );

        let contents = read_log(&log_file);
        assert!(contents.contains("START   REMUX"));
        assert!(!contents.contains("SUCCESS"));
    }

    #[test]
    fn duplicate_pairs_are_collected_in_verbose_mode_and_cleared_after_the_analysis() {
        let (_log_file, reporter) = reporter(true, false);
        let reason = SkipReason::OutputExists {
            path: PathBuf::from("Movie.x265.mp4"),
            source_duration: 60.0,
        };

        reporter.file_skipped(Path::new("Movie.mkv"), &reason);
        assert_eq!(reporter.duplicate_pairs.lock().unwrap().len(), 1);

        reporter.analysis_finished(&AnalysisStats::default(), 1, Duration::ZERO);
        assert!(reporter.duplicate_pairs.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_pairs_are_not_collected_without_verbose() {
        let (_log_file, reporter) = reporter(false, false);
        let reason = SkipReason::OutputExists {
            path: PathBuf::from("Movie.x265.mp4"),
            source_duration: 60.0,
        };

        reporter.file_skipped(Path::new("Movie.mkv"), &reason);

        assert!(reporter.duplicate_pairs.lock().unwrap().is_empty());
    }

    #[test]
    fn the_progress_bar_exists_only_while_probing() {
        let (_log_file, reporter) = reporter(false, false);

        reporter.probing_started(2);
        reporter.file_probed();
        assert!(reporter.progress_bar.lock().unwrap().is_some());

        reporter.probing_finished();
        assert!(reporter.progress_bar.lock().unwrap().is_none());
    }

    #[test]
    fn every_other_event_prints_without_failing() {
        let (_log_file, reporter) = reporter(true, false);
        let source = Path::new("Movie.mkv");
        let target = Path::new("Movie.x265.mp4");
        let error = anyhow::anyhow!("failure");

        reporter.subtitle_not_matched(Path::new("Movie.srt"), SubtitleMatchProblem::NoMatchingVideo);
        reporter.subtitle_not_matched(Path::new("Movie.srt"), SubtitleMatchProblem::Ambiguous);
        reporter.analysis_started(3);
        reporter.scan_cache_checked(1, 2);
        reporter.scan_cache_write_failed(&error);
        reporter.file_skipped(source, &SkipReason::FileMissing);
        reporter.file_skipped(source, &SkipReason::AlreadyConverted);
        reporter.file_skipped(
            source,
            &SkipReason::AnalysisFailed {
                error: "probe failed".to_string(),
            },
        );
        reporter.duplicate_handled(source, target, &DuplicateOutcome::RefusedSameFile);
        reporter.duplicate_handled(
            source,
            target,
            &DuplicateOutcome::DurationMismatch {
                source_duration: 60.0,
                target_duration: 30.0,
                difference_ratio: 0.5,
            },
        );
        reporter.rename_started(FilePosition { index: 1, total: 1 }, source, target, true);
        reporter.rename_skipped_existing(target);
        reporter.rename_failed(source, &error);
        reporter.pending_files_saved(2);
        reporter.pending_files_save_failed(&error);
        reporter.database_cleaned(1, 2);
        reporter.pending_files_loaded(2);
        reporter.pending_file_blocked(source, &SkipReason::FileMissing);
        reporter.database_removal_failed(source, &error);
        reporter.file_missing(source);
        reporter.limit_reached(2);
        reporter.out_of_disk_space(
            source,
            DiskSpaceShortage {
                required: 2_048,
                available: 1_024,
            },
        );
        reporter.deleting(source, DeleteMethod::Trash);
        reporter.deleting(source, DeleteMethod::Delete);
        reporter.delete_failed(source, &error);
        reporter.partial_output_removal_failed(target, &std::io::Error::other("busy"));
        reporter.finish(&RunSummary {
            stats: RunStats::default(),
            outcome: ProcessingOutcome::Aborted,
        });
    }
}
