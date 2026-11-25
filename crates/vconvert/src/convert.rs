//! Video conversion orchestration.
//!
//! Runs the processing batches from a scan or from the database, reporting every step to a `ConvertObserver`.
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use cli_tools_common::TargetReservations;

use crate::classification::database_output_conflict;
use crate::database::{Database, PendingAction};
use crate::ffmpeg::probe_video_info;
use crate::helpers::disk_space_shortage;
use crate::observer::{ConvertObserver, FilePosition, Operation};
use crate::options::ConvertOptions;
use crate::stats::RunStats;
use crate::types::{ProcessResult, ProcessableFile, ProcessableFileSliceExt, ProcessingOutcome, VideoFile};

pub use duplicates::DuplicateOutcome;
use plan::ProcessingPlan;

/// Per-file operation run for every file of one processing batch.
type ProcessFunction<'a> = fn(&VideoConvert<'a>, &ProcessableFile, FilePosition) -> ProcessResult;

/// Video converter that processes files to HEVC format using ffmpeg and NVENC.
pub struct VideoConvert<'a> {
    options: ConvertOptions,
    observer: &'a dyn ConvertObserver,
}

/// Statistics and outcome of a run that processed files.
#[derive(Debug)]
pub struct RunSummary {
    /// Counts and sizes of the processed files.
    pub stats: RunStats,
    /// Whether every planned file was processed, or why processing stopped early.
    pub outcome: ProcessingOutcome,
}

/// One batch of files that share the same operation.
struct Batch<'a> {
    operation: Operation,
    files: Vec<ProcessableFile>,
    process: ProcessFunction<'a>,
}

impl<'a> VideoConvert<'a> {
    /// Create a converter for the options, reporting progress to the observer.
    #[must_use]
    pub fn new(options: ConvertOptions, observer: &'a dyn ConvertObserver) -> Self {
        Self { options, observer }
    }

    /// The options of this converter.
    #[must_use]
    pub const fn options(&self) -> &ConvertOptions {
        &self.options
    }

    /// Scan the input paths, analyse the files, and process them.
    ///
    /// Renames happen right after the analysis.
    /// The files still to process are stored in the database,
    /// then remuxed, subtitle muxed, and converted in that order.
    /// Setting the abort flag stops the run after the current file.
    /// Returns `None` when no video files were found.
    ///
    /// # Errors
    /// Returns an error if an input path does not exist.
    pub fn run(&self, database: &mut Database, abort_flag: &AtomicBool) -> Result<Option<RunSummary>> {
        let mut stats = RunStats::default();

        let candidate_files = self.gather_files_to_process()?;
        if candidate_files.is_empty() {
            return Ok(None);
        }
        let subtitle_matches = if self.options.movie_mode {
            let subtitle_files = Self::gather_subtitle_files_for_video_files(&candidate_files);
            self.match_subtitle_files(&candidate_files, subtitle_files)
        } else {
            HashMap::new()
        };
        self.observer.analysis_started(candidate_files.len());

        let analysis_output = self.analyze_files(candidate_files, database, subtitle_matches);

        // These files are already in a target codec but missing their codec suffix label
        if !analysis_output.renames.is_empty() {
            stats.files_renamed = self.process_renames(&analysis_output.renames);
        }

        let mut plan = ProcessingPlan::new(
            analysis_output.remuxes,
            analysis_output.subtitle_muxes,
            analysis_output.conversions,
        );

        // Store every file that needs processing in a single transaction
        match database.batch_upsert_pending_files(&plan.pending_entries()) {
            Ok(updated_count) => self.observer.pending_files_saved(updated_count),
            Err(error) => self.observer.pending_files_save_failed(&error),
        }

        plan.skip(self.options.skip_remux, self.options.skip_convert);
        if let Some(count) = self.options.count {
            plan.truncate_to(count);
        }

        let (plan_stats, outcome) = self.process_plan(plan, abort_flag, database);
        stats += plan_stats;

        Ok(Some(RunSummary { stats, outcome }))
    }

    /// Process the pending files stored in the database by earlier scans.
    ///
    /// Rows of files that are missing or changed since the scan are removed first.
    /// The remaining files matching the options are processed without scanning or analysing them again.
    /// Setting the abort flag stops the run after the current file.
    /// Returns `None` when no pending file matches the options.
    ///
    /// # Errors
    /// Returns an error if the database cannot be read or updated.
    pub fn run_from_database(&self, database: &mut Database, abort_flag: &AtomicBool) -> Result<Option<RunSummary>> {
        let missing_count = database.remove_missing_files()?;
        let changed_count = database.remove_changed_files()?;
        self.observer.database_cleaned(missing_count, changed_count);

        let pending_files = database.get_pending_files(&self.options.pending_file_filter())?;
        if pending_files.is_empty() {
            return Ok(None);
        }
        self.observer.pending_files_loaded(pending_files.len());

        let video_files: Vec<VideoFile> = pending_files
            .iter()
            .filter(|file| file.full_path.exists())
            .map(|file| VideoFile::new_with_metadata(&file.full_path))
            .collect();
        let mut subtitle_matches = if self.options.movie_mode {
            let subtitle_files = Self::gather_subtitle_files_for_video_files(&video_files);
            self.match_subtitle_files(&video_files, subtitle_files)
        } else {
            HashMap::new()
        };

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
            let processable = ProcessableFile::for_mode(video_file, info, subtitles, self.options.movie_mode);
            // The output may have appeared since the scan, so the scan time checks are repeated here.
            // A blocked row is dropped from the database and a new scan classifies the file again.
            if let Some(reason) = database_output_conflict(&processable, self.options.overwrite, &mut reservations) {
                self.observer.pending_file_blocked(&processable.file.path, &reason);
                if let Err(error) = database.remove_pending_file(&pending_file.full_path) {
                    self.observer.database_removal_failed(&pending_file.full_path, &error);
                }
                continue;
            }
            match pending_file.action {
                PendingAction::Convert => plan.conversions.push(processable),
                PendingAction::Remux => plan.remuxes.push(processable),
                PendingAction::SubtitleMux => plan.subtitle_muxes.push(processable),
            }
        }

        plan.remuxes.sort_by_order(self.options.sort);
        plan.subtitle_muxes.sort_by_order(self.options.sort);
        plan.conversions.sort_by_order(self.options.sort);

        plan.skip(self.options.skip_remux, self.options.skip_convert);
        if let Some(count) = self.options.count {
            plan.truncate_to(count);
        }

        let (stats, outcome) = self.process_plan(plan, abort_flag, database);
        Ok(Some(RunSummary { stats, outcome }))
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

        let batches = [
            Batch {
                operation: Operation::Remux,
                files: plan.remuxes,
                process: Self::remux_to_mp4,
            },
            Batch {
                operation: Operation::SubtitleMux,
                files: plan.subtitle_muxes,
                process: Self::mux_subtitles,
            },
            Batch {
                operation: Operation::Convert,
                files: plan.conversions,
                process: Self::convert_to_hevc,
            },
        ];
        for batch in batches {
            if batch.files.is_empty() || outcome != ProcessingOutcome::Completed {
                continue;
            }
            let (batch_stats, batch_outcome) =
                self.process_files_with_db_cleanup(batch, abort_flag, &mut processed_count, total_limit, database);
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
    /// so the run statistics can still be reported.
    fn process_files_with_db_cleanup(
        &self,
        batch: Batch<'a>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
        total_limit: usize,
        database: &Database,
    ) -> (RunStats, ProcessingOutcome) {
        let mut stats = RunStats::default();
        let mut outcome = ProcessingOutcome::Completed;

        for file in batch.files {
            // Check abort flag before starting a new file
            if abort_flag.load(Ordering::SeqCst) {
                outcome = ProcessingOutcome::Aborted;
                break;
            }

            if *processed_count >= total_limit {
                self.observer.limit_reached(total_limit);
                break;
            }

            if !file.file.path.exists() {
                self.observer.file_missing(&file.file.path);
                if let Err(error) = database.remove_pending_file(&file.file.path) {
                    self.observer.database_removal_failed(&file.file.path, &error);
                }
                continue;
            }

            if let Some(shortage) = disk_space_shortage(&file) {
                self.observer.out_of_disk_space(&file.file.path, shortage);
                outcome = ProcessingOutcome::OutOfDiskSpace;
                break;
            }

            let position = FilePosition {
                index: *processed_count + 1,
                total: total_limit,
            };

            let start = Instant::now();
            let result = (batch.process)(self, &file, position);
            let duration = start.elapsed();

            if !matches!(result, ProcessResult::Failed { .. }) {
                *processed_count += 1;
                if let Err(error) = database.remove_pending_file(&file.file.path) {
                    self.observer.database_removal_failed(&file.file.path, &error);
                }
            }
            self.observer
                .file_finished(batch.operation, position, &file, &result, duration);

            stats.add_result(&result, duration);
        }

        (stats, outcome)
    }
}

/// Builders shared by the converter tests in this module and its submodules.
#[cfg(test)]
mod test_helpers {
    use std::path::Path;

    use super::VideoConvert;
    use crate::observer::ConvertObserver;
    use crate::options::ConvertOptions;
    use crate::types::{ProcessableFile, VideoFile, VideoInfo};

    /// Observer that ignores every event.
    struct SilentObserver;

    impl ConvertObserver for SilentObserver {}

    static SILENT_OBSERVER: SilentObserver = SilentObserver;

    /// Converter with the given options that reports nothing.
    pub fn converter(options: ConvertOptions) -> VideoConvert<'static> {
        VideoConvert::new(options, &SILENT_OBSERVER)
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
    use std::path::{Path, PathBuf};

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

    fn batch(files: Vec<ProcessableFile>, process: ProcessFunction<'static>) -> Batch<'static> {
        Batch {
            operation: Operation::Convert,
            files,
            process,
        }
    }

    fn pending_paths(database: &Database) -> Vec<PathBuf> {
        database
            .get_pending_files(&crate::database::PendingFileFilter::default())
            .expect("Failed to query pending files")
            .into_iter()
            .map(|file| file.full_path)
            .collect()
    }

    fn succeed(_converter: &VideoConvert<'static>, _file: &ProcessableFile, _position: FilePosition) -> ProcessResult {
        ProcessResult::converted(1_024, 8_000, 512, 4_000)
    }

    fn fail(_converter: &VideoConvert<'static>, _file: &ProcessableFile, _position: FilePosition) -> ProcessResult {
        ProcessResult::Failed {
            error: "ffmpeg failed".to_string(),
        }
    }

    #[test]
    fn successful_files_are_counted_and_removed_from_the_database() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv", "second.mkv"]);
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &database,
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
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, fail),
            &AtomicBool::new(false),
            &mut processed_count,
            1,
            &database,
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
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, _) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            1,
            &database,
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
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &database,
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
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(true),
            &mut processed_count,
            1,
            &database,
        );

        assert_eq!(outcome, ProcessingOutcome::Aborted);
        assert_eq!(stats.files_converted, 0);
        assert_eq!(pending_paths(&database).len(), 1);
    }
}
