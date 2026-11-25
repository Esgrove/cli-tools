//! Video conversion orchestration.
//!
//! Runs the processing batches from a scan or from the database.
//! Discovery, analysis, duplicate deletion, the processing plan, renames, and file disposal live in submodules,
//! the per-file ffmpeg operations in `operations`, and the subtitle sidecar handling in `subtitles`.

mod analysis;
mod discovery;
mod duplicates;
mod file_disposal;
mod operations;
mod plan;
mod renames;
mod subtitles;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use cli_tools_common::{TargetReservations, print_error, print_yellow};
use colored::Colorize;

use crate::VideoConvertArgs;
use crate::classification::database_output_conflict;
use crate::cli::{DatabaseMode, clean_scan_cache, clear_database, list_extensions, show_database_contents};
use crate::config::{Config, VideoConvertConfig};
use crate::database::{Database, PendingAction};
use crate::ffmpeg::probe_video_info;
use crate::helpers::has_enough_disk_space;
use crate::logger::FileLogger;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};
use crate::types::{ProcessResult, ProcessableFile, ProcessableFileSliceExt, ProcessingOutcome, VideoFile, VideoInfo};

use plan::ProcessingPlan;

/// Per-file operation run for every file of one processing batch.
type ProcessFunction = fn(&VideoConvert, &ProcessableFile, &str) -> ProcessResult;

/// Video converter that processes files to HEVC format using ffmpeg and NVENC.
pub struct VideoConvert {
    config: Config,
    logger: RefCell<FileLogger>,
}

impl VideoConvert {
    /// Create a new video converter from command line arguments.
    pub fn new(args: VideoConvertArgs) -> Result<Self> {
        let user_config = VideoConvertConfig::get_user_config()?;
        let config = Config::try_from_args(args, user_config)?;
        let logger = RefCell::new(FileLogger::new()?);

        Ok(Self { config, logger })
    }

    /// Run the video conversion process.
    ///
    /// This handles database modes if specified,
    /// otherwise scans for files,
    /// analyses them,
    /// updates the database,
    /// processes renames immediately,
    /// and then converts or remuxes the remaining files.
    pub fn run(self) -> Result<()> {
        self.log_init();

        // Handle database modes
        if let Some(db_mode) = self.config.database_mode {
            return match db_mode {
                DatabaseMode::Clear => Database::open_default().and_then(|db| clear_database(&db)),
                DatabaseMode::Show => Database::open_default().and_then(|db| show_database_contents(&db, &self.config)),
                DatabaseMode::ListExtensions => {
                    Database::open_default().and_then(|db| list_extensions(&db, self.config.verbose))
                }
                DatabaseMode::CleanScanCache => {
                    Database::open_default().and_then(|mut db| clean_scan_cache(&mut db, self.config.verbose))
                }
                DatabaseMode::Process => self.run_from_database(),
            };
        }

        // Open the database for tracking
        let mut database = Database::open_default()?;
        if self.config.verbose {
            println!("Database: {}", Database::path().display());
        }

        let abort_flag = Self::install_abort_handler();
        let mut stats = RunStats::default();

        // Gather candidate files
        let candidate_files = self.gather_files_to_process()?;
        if candidate_files.is_empty() {
            println!("No video files found");
            return Ok(());
        }
        let subtitle_matches = if self.config.movie_mode {
            let subtitle_files = Self::gather_subtitle_files_for_video_files(&candidate_files);
            Self::match_subtitle_files(&candidate_files, subtitle_files, self.config.verbose)
        } else {
            HashMap::new()
        };
        if self.config.verbose {
            println!(
                "Found {}, analyzing...",
                cli_tools_common::count_label(candidate_files.len(), "candidate file", "candidate files")
            );
        }

        // Analyze files to determine required actions
        let analysis_output = self.analyze_files(candidate_files, &mut database, subtitle_matches);

        // Process renames: these files are already in a target codec but missing their codec suffix label
        if !analysis_output.renames.is_empty() {
            stats.files_renamed = self.process_renames(&analysis_output.renames);
        }

        let mut plan = ProcessingPlan::new(
            analysis_output.remuxes,
            analysis_output.subtitle_muxes,
            analysis_output.conversions,
        );

        // Update the database with files that need processing (single transaction)
        match database.batch_upsert_pending_files(&plan.pending_entries()) {
            Ok(db_added) if self.config.verbose && db_added > 0 => {
                println!("Updated {db_added} files in database");
            }
            Err(error) if self.config.verbose => {
                print_yellow!("Failed to update pending files in database: {error}");
            }
            _ => {}
        }

        plan.skip(self.config.skip_remux, self.config.skip_convert);
        if let Some(count) = self.config.count {
            plan.truncate_to(count);
        }

        let (plan_stats, outcome) = self.process_plan(plan, &abort_flag, &database);
        stats += plan_stats;

        self.log_stats(&stats);

        if outcome != ProcessingOutcome::Completed {
            println!("\n{outcome}");
        }

        stats.print_summary();

        Ok(())
    }

    /// Run video conversion from files stored in the database.
    ///
    /// This skips the scanning/analysis phase and processes files directly from the database.
    /// Supports filtering by extension, bitrate, and duration via CLI arguments.
    pub fn run_from_database(&self) -> Result<()> {
        self.log_init();

        let mut database = Database::open_default()?;

        // Remove files that no longer exist
        let removed = database.remove_missing_files()?;
        if removed > 0 {
            println!("{}", format!("Removed {removed} missing files from database").yellow());
        }

        let changed = database.remove_changed_files()?;
        if changed > 0 {
            println!(
                "{}",
                format!(
                    "Removed {} from database, scan again to process them",
                    cli_tools_common::count_label(
                        changed,
                        "file changed since the scan",
                        "files changed since the scan"
                    )
                )
                .yellow()
            );
        }

        // Build filter from config
        let filter = self.config.db_filter.clone();

        // Get pending files from database with filters applied
        let pending_files = database.get_pending_files(&filter)?;
        if pending_files.is_empty() {
            println!("No pending files in database matching filters");
            return Ok(());
        }

        if self.config.verbose {
            println!(
                "Processing {} from database",
                cli_tools_common::count_label(pending_files.len(), "pending file", "pending files")
            );
        }

        let abort_flag = Self::install_abort_handler();

        let video_files: Vec<VideoFile> = pending_files
            .iter()
            .filter(|file| file.full_path.exists())
            .map(|file| VideoFile::new_with_metadata(&file.full_path))
            .collect();
        let mut subtitle_matches = if self.config.movie_mode {
            let subtitle_files = Self::gather_subtitle_files_for_video_files(&video_files);
            Self::match_subtitle_files(&video_files, subtitle_files, self.config.verbose)
        } else {
            HashMap::new()
        };

        // Convert to processable files
        let mut plan = ProcessingPlan::default();
        let mut reservations = TargetReservations::new();

        for pending_file in pending_files.into_iter().filter(|file| file.full_path.exists()) {
            let video_file = VideoFile::new_with_metadata(&pending_file.full_path);
            let subtitles = subtitle_matches.remove(&video_file.path).unwrap_or_default();
            let info = if pending_file.bit_depth >= 8 {
                pending_file.to_video_info()
            } else {
                probe_video_info(&pending_file.full_path).unwrap_or_else(|_| pending_file.to_video_info())
            };
            let processable = ProcessableFile::for_mode(video_file, info, subtitles, self.config.movie_mode);
            // The output may have appeared since the scan, so the scan time checks are repeated here.
            // A blocked row is dropped from the database and a new scan classifies the file again.
            if let Some(reason) = database_output_conflict(&processable, self.config.overwrite, &mut reservations) {
                print_yellow!(
                    "{}: {reason}",
                    cli_tools_common::path_to_string_relative(&processable.file.path)
                );
                if let Err(error) = database.remove_pending_file(&pending_file.full_path) {
                    print_error!(
                        "Failed to remove {} from database: {error}",
                        pending_file.full_path.display()
                    );
                }
                continue;
            }
            match pending_file.action {
                PendingAction::Convert => plan.conversions.push(processable),
                PendingAction::Remux => plan.remuxes.push(processable),
                PendingAction::SubtitleMux => plan.subtitle_muxes.push(processable),
            }
        }

        // Sort files
        plan.remuxes.sort_by_order(self.config.sort);
        plan.subtitle_muxes.sort_by_order(self.config.sort);
        plan.conversions.sort_by_order(self.config.sort);

        plan.skip(self.config.skip_remux, self.config.skip_convert);
        if let Some(count) = self.config.count {
            plan.truncate_to(count);
        }

        let (stats, outcome) = self.process_plan(plan, &abort_flag, &database);

        self.log_stats(&stats);

        if outcome != ProcessingOutcome::Completed {
            println!("\n{outcome}");
        }

        stats.print_summary();

        // Show remaining database stats
        let db_stats = database.get_stats()?;
        if db_stats.total_files > 0 {
            println!("\n{}", "Remaining in database:".bold());
            println!("{db_stats}");
        }

        Ok(())
    }

    /// Install a Ctrl+C handler that asks the run to stop after the current file.
    ///
    /// A second Ctrl+C exits immediately.
    fn install_abort_handler() -> Arc<AtomicBool> {
        let abort_flag = Arc::new(AtomicBool::new(false));
        let abort_flag_handler = Arc::clone(&abort_flag);

        ctrlc::set_handler(move || {
            if abort_flag_handler.load(Ordering::SeqCst) {
                // Second Ctrl+C - force exit
                std::process::exit(130);
            }
            println!("\n{}", "Received Ctrl+C, finishing current file...".yellow().bold());
            abort_flag_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl+C handler");

        abort_flag
    }

    /// Process the remuxes, subtitle muxes, and conversions of a plan in that order.
    ///
    /// A batch only starts when the previous one completed,
    /// so an abort or a full disk stops the rest of the plan.
    fn process_plan(
        &self,
        plan: ProcessingPlan,
        abort_flag: &AtomicBool,
        database: &Database,
    ) -> (RunStats, ProcessingOutcome) {
        let total_limit = plan.total();
        let mut stats = RunStats::default();
        let mut outcome = ProcessingOutcome::Completed;
        let mut processed_count: usize = 0;

        let batches: [(Vec<ProcessableFile>, ProcessFunction); 3] = [
            (plan.remuxes, Self::remux_to_mp4),
            (plan.subtitle_muxes, Self::mux_subtitles),
            (plan.conversions, Self::convert_to_hevc),
        ];
        for (files, process_fn) in batches {
            if files.is_empty() || outcome != ProcessingOutcome::Completed {
                continue;
            }
            let (batch_stats, batch_outcome) = self.process_files_with_db_cleanup(
                files,
                abort_flag,
                &mut processed_count,
                total_limit,
                database,
                process_fn,
            );
            stats += batch_stats;
            outcome = batch_outcome;
        }

        (stats, outcome)
    }

    /// Process files and remove them from the database after successful processing.
    ///
    /// Before each file the free disk space at the output location is verified:
    /// at least `MIN_DISK_SPACE_FACTOR` times the original file size must be available,
    /// otherwise processing stops gracefully with an out-of-disk-space outcome
    /// so the run statistics can still be printed.
    fn process_files_with_db_cleanup<F>(
        &self,
        files: Vec<ProcessableFile>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
        total_limit: usize,
        database: &Database,
        process_fn: F,
    ) -> (RunStats, ProcessingOutcome)
    where
        F: Fn(&Self, &ProcessableFile, &str) -> ProcessResult,
    {
        let mut stats = RunStats::default();
        let num_digits = total_limit.checked_ilog10().map_or(1, |d| d as usize + 1);
        let mut outcome = ProcessingOutcome::Completed;

        for file in files {
            // Check abort flag before starting a new file
            if abort_flag.load(Ordering::SeqCst) {
                outcome = ProcessingOutcome::Aborted;
                break;
            }

            if *processed_count >= total_limit {
                println!("{}", format!("\nReached file limit ({total_limit})").bold());
                break;
            }

            // Check if file still exists
            if !file.file.path.exists() {
                print_yellow!("File no longer exists: {}", file.file.path.display());
                if let Err(error) = database.remove_pending_file(&file.file.path) {
                    print_error!("{error:#}");
                }
                continue;
            }

            // Ensure there is enough free disk space before starting the conversion
            if !has_enough_disk_space(&file) {
                outcome = ProcessingOutcome::OutOfDiskSpace;
                break;
            }

            let file_index = format!("[{:>width$}/{total_limit}]", *processed_count + 1, width = num_digits);

            let start = Instant::now();
            let result = process_fn(self, &file, &file_index);
            let duration = start.elapsed();

            match &result {
                ProcessResult::Failed { error } => {
                    print_error!(
                        "{}: {error}",
                        cli_tools_common::path_to_string_relative(&file.file.path)
                    );
                }
                ProcessResult::Converted { .. } | ProcessResult::Remuxed {} | ProcessResult::SubtitlesMuxed {} => {
                    *processed_count += 1;
                    if let Err(error) = database.remove_pending_file(&file.file.path) {
                        print_error!("{error:#}");
                    }
                }
            }

            stats.add_result(&result, duration);
        }

        (stats, outcome)
    }

    #[inline]
    fn log_init(&self) {
        self.logger.borrow_mut().log_init(&self.config);
    }

    #[inline]
    fn log_gathered_files(&self, file_count: usize, duration: Duration) {
        self.logger.borrow_mut().log_gathered_files(file_count, duration);
    }

    #[inline]
    fn log_analysis_stats(&self, stats: &AnalysisStats, total_files: usize, duration: Duration) {
        self.logger
            .borrow_mut()
            .log_analysis_stats(stats, total_files, duration);
    }

    #[inline]
    fn log_renames(&self, renamed_count: usize, total_count: usize, duration: Duration) {
        self.logger
            .borrow_mut()
            .log_renames(renamed_count, total_count, duration);
    }

    #[inline]
    fn log_start(
        &self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        info: &VideoInfo,
        quality_level: Option<u8>,
    ) {
        self.logger
            .borrow_mut()
            .log_start(file_path, operation, file_index, info, quality_level);
    }

    #[inline]
    fn log_success(
        &self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        duration: Duration,
        stats: Option<&ConversionStats>,
    ) {
        self.logger
            .borrow_mut()
            .log_success(file_path, operation, file_index, duration, stats);
    }

    #[inline]
    fn log_failure(&self, file_path: &Path, operation: &str, file_index: &str, error: &str) {
        self.logger
            .borrow_mut()
            .log_failure(file_path, operation, file_index, error);
    }

    #[inline]
    fn log_stats(&self, stats: &RunStats) {
        self.logger.borrow_mut().log_stats(stats);
    }
}

/// Builders shared by the converter tests in this module and its submodules.
#[cfg(test)]
mod test_helpers {
    use std::cell::RefCell;
    use std::path::Path;

    use tempfile::NamedTempFile;

    use super::VideoConvert;
    use crate::config::Config;
    use crate::logger::FileLogger;
    use crate::types::{ProcessableFile, VideoFile, VideoInfo};

    /// Converter with the given config, logging to a temporary file kept alive by the returned handle.
    pub fn converter(config: Config) -> (NamedTempFile, VideoConvert) {
        let (log_file, logger) = FileLogger::temporary();
        (
            log_file,
            VideoConvert {
                config,
                logger: RefCell::new(logger),
            },
        )
    }

    /// Video info for a small 1080p file with the given codec.
    pub fn video_info(codec: &str) -> VideoInfo {
        VideoInfo {
            codec: codec.to_string(),
            bitrate_kbps: 8_000,
            size_bytes: 1_024,
            duration: 60.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        }
    }

    /// Processable file for the path, with its output path resolved for normal mode.
    pub fn processable(path: &Path, codec: &str) -> ProcessableFile {
        ProcessableFile::for_mode(VideoFile::new(path, 1_024), video_info(codec), Vec::new(), false)
    }
}

#[cfg(test)]
mod test_process_files_with_db_cleanup {
    use std::path::PathBuf;

    use super::test_helpers::{converter, processable, video_info};
    use super::*;

    /// Files written into the directory, each also stored as a pending database entry.
    fn pending_files(directory: &Path, database: &Database, names: &[&str]) -> Vec<ProcessableFile> {
        names
            .iter()
            .map(|name| {
                let path = directory.join(name);
                std::fs::write(&path, b"video").expect("Failed to write video file");
                database
                    .upsert_pending_file(&path, "mkv", &video_info("h264"), PendingAction::Convert)
                    .expect("Failed to insert pending file");
                processable(&path, "h264")
            })
            .collect()
    }

    fn pending_paths(database: &Database) -> Vec<PathBuf> {
        database
            .get_pending_files(&crate::database::PendingFileFilter::default())
            .expect("Failed to query pending files")
            .into_iter()
            .map(|file| file.full_path)
            .collect()
    }

    fn succeed(_converter: &VideoConvert, _file: &ProcessableFile, _index: &str) -> ProcessResult {
        ProcessResult::converted(1_024, 8_000, 512, 4_000)
    }

    fn fail(_converter: &VideoConvert, _file: &ProcessableFile, _index: &str) -> ProcessResult {
        ProcessResult::Failed {
            error: "ffmpeg failed".to_string(),
        }
    }

    #[test]
    fn successful_files_are_counted_and_removed_from_the_database() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv", "second.mkv"]);
        let (_log, converter) = converter(Config::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            files,
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &database,
            succeed,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(processed_count, 2);
        assert_eq!(stats.files_converted, 2);
        assert_eq!(pending_paths(&database), [] as [PathBuf; 0]);
    }

    #[test]
    fn failed_files_stay_in_the_database() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["broken.mkv"]);
        let (_log, converter) = converter(Config::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            files,
            &AtomicBool::new(false),
            &mut processed_count,
            1,
            &database,
            fail,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(processed_count, 0);
        assert_eq!(stats.files_failed, 1);
        assert_eq!(pending_paths(&database), vec![directory.path().join("broken.mkv")]);
    }

    #[test]
    fn missing_files_are_dropped_from_the_database_without_processing() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["gone.mkv"]);
        std::fs::remove_file(directory.path().join("gone.mkv")).expect("Failed to remove video file");
        let (_log, converter) = converter(Config::default());
        let mut processed_count = 0;

        let (stats, _) = converter.process_files_with_db_cleanup(
            files,
            &AtomicBool::new(false),
            &mut processed_count,
            1,
            &database,
            succeed,
        );

        assert_eq!(processed_count, 0);
        assert_eq!(stats.files_converted, 0);
        assert_eq!(pending_paths(&database), [] as [PathBuf; 0]);
    }

    #[test]
    fn processing_stops_at_the_file_limit() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv", "second.mkv", "third.mkv"]);
        let (_log, converter) = converter(Config::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            files,
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &database,
            succeed,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(stats.files_converted, 2);
        assert_eq!(pending_paths(&database).len(), 1);
    }

    #[test]
    fn abort_flag_stops_before_the_next_file() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv"]);
        let (_log, converter) = converter(Config::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            files,
            &AtomicBool::new(true),
            &mut processed_count,
            1,
            &database,
            succeed,
        );

        assert_eq!(outcome, ProcessingOutcome::Aborted);
        assert_eq!(stats.files_converted, 0);
        assert_eq!(pending_paths(&database).len(), 1);
    }
}
