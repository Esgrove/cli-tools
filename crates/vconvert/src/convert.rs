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
use cli_tools_db::locks::{AcquireOutcome, ActiveLockStore, DEFAULT_LOCK_TTL_SECS};

use crate::classification::database_output_conflict_with_mode;
use crate::database::{Database, PendingAction};
use crate::ffmpeg::{probe_video_info, register_lock_owner, unregister_lock_owner};
use crate::helpers::disk_space_shortage;
use crate::observer::{ConvertObserver, FilePosition, Operation};
use crate::options::ConvertOptions;
use crate::stats::RunStats;
use crate::types::{ProcessResult, ProcessableFile, ProcessableFileSliceExt, ProcessingOutcome, VideoFile, VideoInfo};

pub use duplicates::DuplicateOutcome;
use plan::ProcessingPlan;

/// Per-file operation run for every file of one processing batch.
type ProcessFunction<'a> =
    fn(&VideoConvert<'a>, &ProcessableFile, FilePosition, &AtomicBool) -> (ProcessResult, Option<VideoInfo>);

/// Guard that registers a lock store owner for forced-abort cleanup and unregisters it on drop.
struct RunLockScope {
    store: ActiveLockStore,
}

impl RunLockScope {
    /// Open an [`ActiveLockStore`] for `database` when it is backed by a file on disk and not a dry run.
    fn open(database: &Database, dry_run: bool) -> Option<Self> {
        if dry_run {
            return None;
        }
        let db_path = database.db_path()?.to_path_buf();
        let store = ActiveLockStore::open_at(db_path.clone(), "vconvert").ok()?;
        register_lock_owner(db_path, store.owner_id().to_string());
        Some(Self { store })
    }
}

impl Drop for RunLockScope {
    fn drop(&mut self) {
        unregister_lock_owner(self.store.owner_id());
        let _ = self.store.release_all_owned();
    }
}

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
        if abort_flag.load(Ordering::SeqCst) {
            return Ok(Some(RunSummary {
                stats: RunStats::default(),
                outcome: ProcessingOutcome::Aborted,
            }));
        }

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

        let lock_scope = RunLockScope::open(database, self.options.dry_run);
        let lock_store = lock_scope.as_ref().map(|scope| &scope.store);

        let (analysis_output, mut stats) = self.analyze_files(candidate_files, database, subtitle_matches, abort_flag);
        if !analysis_output.renames.is_empty() && !abort_flag.load(Ordering::SeqCst) {
            let (renamed, failed) = self.process_renames(&analysis_output.renames, abort_flag, database, lock_store);
            stats.files_renamed = renamed;
            stats.files_failed += failed;
        }
        if abort_flag.load(Ordering::SeqCst) {
            return Ok(Some(RunSummary {
                stats,
                outcome: ProcessingOutcome::Aborted,
            }));
        }

        let mut plan = ProcessingPlan::new(
            analysis_output.remuxes,
            analysis_output.subtitle_muxes,
            analysis_output.conversions,
        );

        // Store every file that needs processing in a single transaction unless this is a dry run.
        if !self.options.dry_run {
            match database.batch_upsert_pending_files_with_mode(&plan.pending_entries(), self.options.movie_mode) {
                Ok(updated_count) => self.observer.pending_files_saved(updated_count),
                Err(error) => self.observer.pending_files_save_failed(&error),
            }
        }

        plan.skip(self.options.skip_remux, self.options.skip_convert);
        if let Some(count) = self.options.count {
            plan.truncate_to(count);
        }

        let (plan_stats, outcome) = self.process_plan(plan, abort_flag, database, lock_store, false);
        stats += plan_stats;

        Ok(Some(RunSummary { stats, outcome }))
    }

    /// Process the pending files stored in the database by earlier scans.
    ///
    /// Rows of files that are missing or changed since the scan are removed first.
    /// The remaining files matching the options are processed without scanning or analysing them again.
    /// Concurrent `--from-db` runs claim rows atomically and hold `active_locks` so they never convert the same file.
    ///
    /// # Errors
    /// Returns an error if the database cannot be read or updated.
    pub fn run_from_database(&self, database: &mut Database, abort_flag: &AtomicBool) -> Result<Option<RunSummary>> {
        if abort_flag.load(Ordering::SeqCst) {
            return Ok(Some(RunSummary {
                stats: RunStats::default(),
                outcome: ProcessingOutcome::Aborted,
            }));
        }
        let lock_scope = RunLockScope::open(database, self.options.dry_run);
        let lock_store = lock_scope.as_ref().map(|scope| &scope.store);

        if !self.options.dry_run {
            let missing_count = database.remove_missing_files()?;
            let changed_count = database.remove_changed_files()?;
            self.observer.database_cleaned(missing_count, changed_count);
        }

        // Defer count truncation until after action skips and output-conflict checks so skipped rows do not starve --count.
        let mut filter = self.options.pending_file_filter();
        filter.limit = None;
        let mut pending_files = database.get_pending_files(&filter)?;
        // Apply --skip-remux / --skip-convert before output-conflict checks so skipped rows never reserve outputs.
        pending_files.retain(|file| {
            let skipped = match file.action {
                PendingAction::Remux | PendingAction::SubtitleMux => self.options.skip_remux,
                PendingAction::Convert => self.options.skip_convert,
            };
            !skipped && file.full_path.is_file() && !file.is_changed_on_disk()
        });
        if pending_files.is_empty() {
            return Ok(None);
        }
        self.observer.pending_files_loaded(pending_files.len());

        let video_files: Vec<VideoFile> = pending_files
            .iter()
            .filter(|file| {
                file.action == PendingAction::SubtitleMux
                    || (file.action == PendingAction::Convert && self.options.movie_mode)
            })
            .map(|file| VideoFile::new_with_metadata(&file.full_path))
            .collect();
        let subtitle_files = Self::gather_subtitle_files_for_video_files(&video_files);
        let mut subtitle_matches = self.match_subtitle_files(&video_files, subtitle_files);

        let mut plan = ProcessingPlan::default();
        let mut reservations = TargetReservations::new();

        for pending_file in pending_files.into_iter().filter(|file| file.full_path.exists()) {
            if abort_flag.load(Ordering::SeqCst) {
                return Ok(Some(RunSummary {
                    stats: RunStats::default(),
                    outcome: ProcessingOutcome::Aborted,
                }));
            }
            let is_subtitle_mux = pending_file.action == PendingAction::SubtitleMux;
            let effective_movie_mode = self.options.movie_mode || is_subtitle_mux;
            let video_file = VideoFile::new_with_metadata(&pending_file.full_path);
            let subtitles = subtitle_matches.remove(&video_file.path).unwrap_or_default();
            let info = if pending_file.bit_depth >= 8 {
                pending_file.to_video_info()
            } else {
                probe_video_info(&pending_file.full_path).unwrap_or_else(|_| pending_file.to_video_info())
            };
            let processable = ProcessableFile::for_mode(video_file, info, subtitles, effective_movie_mode);
            // The output may have appeared since the scan, so the scan time checks are repeated here.
            // A blocked row is dropped from the database and a new scan classifies the file again.
            if let Some(reason) = database_output_conflict_with_mode(
                &processable,
                self.options.overwrite,
                is_subtitle_mux,
                &mut reservations,
            ) {
                self.observer.pending_file_blocked(&processable.file.path, &reason);
                if !self.options.dry_run
                    && let Err(error) = database.remove_pending_file(&pending_file.full_path)
                {
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

        if let Some(count) = self.options.count {
            plan.truncate_to(count);
        }

        let (stats, outcome) = self.process_plan(plan, abort_flag, database, lock_store, true);
        Ok(Some(RunSummary { stats, outcome }))
    }

    /// Process the remuxes, subtitle muxes, and conversions of a plan in that order.
    fn process_plan(
        &self,
        plan: ProcessingPlan,
        abort_flag: &AtomicBool,
        database: &mut Database,
        lock_store: Option<&ActiveLockStore>,
        claim_pending_rows: bool,
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
            let (batch_stats, batch_outcome) = self.process_files_with_db_cleanup(
                batch,
                abort_flag,
                &mut processed_count,
                total_limit,
                database,
                lock_store,
                claim_pending_rows,
            );
            stats += batch_stats;
            outcome = batch_outcome;
        }

        (stats, outcome)
    }

    /// Process files, coordinating via `active_locks` and `pending_files` claims, and update `database`.
    #[allow(clippy::too_many_arguments)]
    fn process_files_with_db_cleanup(
        &self,
        batch: Batch<'a>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
        total_limit: usize,
        database: &mut Database,
        lock_store: Option<&ActiveLockStore>,
        claim_pending_rows: bool,
    ) -> (RunStats, ProcessingOutcome) {
        let mut stats = RunStats::default();
        let mut outcome = ProcessingOutcome::Completed;

        for file in batch.files {
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
                if !self.options.dry_run
                    && let Err(error) = database.remove_pending_file(&file.file.path)
                {
                    self.observer.database_removal_failed(&file.file.path, &error);
                }
                continue;
            }

            // Acquire cross-tool active locks on input and output paths before starting work.
            let _lock_guard = if let Some(store) = lock_store {
                match store.try_acquire(
                    &[file.file.path.clone(), file.output_path.clone()],
                    batch.operation.as_str(),
                ) {
                    Ok(AcquireOutcome::Acquired(guard)) => Some(guard),
                    Ok(AcquireOutcome::AlreadyLocked(holder)) => {
                        self.observer.file_locked(&file.file.path, &holder.tool, holder.pid);
                        continue;
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

            let claimed_owner = if claim_pending_rows && !self.options.dry_run {
                let owner_id = lock_store.map_or("vconvert-in-memory", ActiveLockStore::owner_id);
                match database.claim_pending_file(&file.file.path, owner_id, DEFAULT_LOCK_TTL_SECS) {
                    Ok(true) => Some(owner_id.to_string()),
                    Ok(false) => {
                        self.observer.file_locked(&file.file.path, "vconvert", 0);
                        continue;
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

            if let Some(shortage) = disk_space_shortage(&file) {
                if let Some(ref owner_id) = claimed_owner {
                    let _ = database.release_pending_claim(&file.file.path, owner_id);
                }
                self.observer.out_of_disk_space(&file.file.path, shortage);
                outcome = ProcessingOutcome::OutOfDiskSpace;
                break;
            }

            let position = FilePosition {
                index: *processed_count + 1,
                total: total_limit,
            };

            let start = Instant::now();
            let (result, output_info) = (batch.process)(self, &file, position, abort_flag);
            let duration = start.elapsed();

            if !matches!(result, ProcessResult::Failed { .. }) {
                *processed_count += 1;
                if !self.options.dry_run {
                    if let Err(error) = database.remove_pending_file(&file.file.path) {
                        self.observer.database_removal_failed(&file.file.path, &error);
                    }
                    if let Some(ref output_info) = output_info {
                        let _ = database.upsert_scanned_file(&file.output_path, output_info);
                    }
                    if !cli_tools_common::paths_refer_to_same_file(&file.file.path, &file.output_path) {
                        let _ = database.remove_cached_path(&file.file.path);
                    }
                }
            } else if let Some(ref owner_id) = claimed_owner {
                let _ = database.release_pending_claim(&file.file.path, owner_id);
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

    fn succeed(
        _converter: &VideoConvert<'static>,
        _file: &ProcessableFile,
        _position: FilePosition,
        _abort_flag: &AtomicBool,
    ) -> (ProcessResult, Option<VideoInfo>) {
        (ProcessResult::converted(1_024, 8_000, 512, 4_000), None)
    }

    fn fail(
        _converter: &VideoConvert<'static>,
        _file: &ProcessableFile,
        _position: FilePosition,
        _abort_flag: &AtomicBool,
    ) -> (ProcessResult, Option<VideoInfo>) {
        (
            ProcessResult::Failed {
                error: "ffmpeg failed".to_string(),
            },
            None,
        )
    }

    #[test]
    fn successful_files_are_counted_and_removed_from_the_database() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv", "second.mkv"]);
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &mut database,
            None,
            false,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(processed_count, 2);
        assert_eq!(stats.files_converted, 2);
        assert_eq!(pending_paths(&database), [] as [PathBuf; 0]);
    }

    #[test]
    fn failed_files_stay_in_the_database() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["broken.mkv"]);
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, fail),
            &AtomicBool::new(false),
            &mut processed_count,
            1,
            &mut database,
            None,
            false,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(processed_count, 0);
        assert_eq!(stats.files_failed, 1);
        assert_eq!(pending_paths(&database), vec![directory.path().join("broken.mkv")]);
    }

    #[test]
    fn missing_files_are_dropped_from_the_database_without_processing() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["gone.mkv"]);
        std::fs::remove_file(directory.path().join("gone.mkv")).expect("Failed to remove video file");
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, _) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            1,
            &mut database,
            None,
            false,
        );

        assert_eq!(processed_count, 0);
        assert_eq!(stats.files_converted, 0);
        assert_eq!(pending_paths(&database), [] as [PathBuf; 0]);
    }

    #[test]
    fn processing_stops_at_the_file_limit() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv", "second.mkv", "third.mkv"]);
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &mut database,
            None,
            false,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(stats.files_converted, 2);
        assert_eq!(pending_paths(&database).len(), 1);
    }

    #[test]
    fn abort_flag_stops_before_the_next_file() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("Failed to open in-memory database");
        let files = pending_files(directory.path(), &database, &["first.mkv"]);
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(true),
            &mut processed_count,
            1,
            &mut database,
            None,
            false,
        );

        assert_eq!(outcome, ProcessingOutcome::Aborted);
        assert_eq!(stats.files_converted, 0);
        assert_eq!(pending_paths(&database).len(), 1);
    }

    #[test]
    fn skips_file_locked_by_another_concurrent_worker() {
        let directory = tempfile::tempdir().expect("temp dir");
        let db_path = directory.path().join("cli-tools.db");
        let mut database = Database::open_at(&db_path).expect("open db");
        let files = pending_files(directory.path(), &database, &["busy.mkv", "free.mkv"]);

        let other_worker = ActiveLockStore::open_at(db_path.clone(), "vconvert").expect("lock store");
        let _guard = other_worker
            .try_acquire(std::slice::from_ref(&files[0].file.path), "convert")
            .expect("lock busy file");

        let this_worker = ActiveLockStore::open_at(db_path, "vconvert").expect("lock store");
        let converter = converter(ConvertOptions::default());
        let mut processed_count = 0;

        let (stats, outcome) = converter.process_files_with_db_cleanup(
            batch(files, succeed),
            &AtomicBool::new(false),
            &mut processed_count,
            2,
            &mut database,
            Some(&this_worker),
            true,
        );

        assert_eq!(outcome, ProcessingOutcome::Completed);
        assert_eq!(stats.files_converted, 1);
        assert_eq!(pending_paths(&database), vec![directory.path().join("busy.mkv")]);
    }

    #[test]
    fn a_preaborted_scan_keeps_duplicates_and_unlabeled_sources() {
        let directory = tempfile::tempdir().unwrap();
        let mut database = Database::open_in_memory().unwrap();
        let unlabeled = directory.path().join("unlabeled.mp4");
        let source = directory.path().join("duplicate.mkv");
        let output = directory.path().join("duplicate.x265.mp4");
        for (path, codec) in [(&unlabeled, "hevc"), (&source, "h264"), (&output, "hevc")] {
            std::fs::write(path, vec![0_u8; 1_024]).unwrap();
            database
                .batch_upsert_scanned_files(&[(path.as_path(), &video_info(codec))])
                .unwrap();
        }
        let options = ConvertOptions {
            paths: vec![unlabeled.clone(), source.clone()],
            extensions: vec!["mp4".to_owned(), "mkv".to_owned()],
            delete_duplicates: true,
            delete: true,
            ..ConvertOptions::default()
        };

        let summary = converter(options)
            .run(&mut database, &AtomicBool::new(true))
            .unwrap()
            .unwrap();

        assert!(unlabeled.is_file(), "a cancelled scan renamed a source");
        assert!(source.is_file(), "a cancelled scan deleted a duplicate");
        assert_eq!(summary.outcome, ProcessingOutcome::Aborted);
    }

    #[test]
    fn cancellation_between_renames_keeps_remaining_files() {
        struct AbortOnSecondRename<'a>(&'a AtomicBool);

        impl ConvertObserver for AbortOnSecondRename<'_> {
            fn rename_started(&self, position: FilePosition, _: &Path, _: &Path, _: bool) {
                if position.index == 2 {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let mut database = Database::open_in_memory().unwrap();
        let paths = [directory.path().join("first.mp4"), directory.path().join("second.mp4")];
        for path in &paths {
            std::fs::write(path, vec![0_u8; 1_024]).unwrap();
            database
                .batch_upsert_scanned_files(&[(path.as_path(), &video_info("hevc"))])
                .unwrap();
        }
        let abort = AtomicBool::new(false);
        let observer = AbortOnSecondRename(&abort);
        let options = ConvertOptions {
            paths: paths.to_vec(),
            extensions: vec!["mp4".to_owned()],
            ..ConvertOptions::default()
        };
        let summary = VideoConvert::new(options, &observer)
            .run(&mut database, &abort)
            .unwrap()
            .unwrap();

        assert_eq!(paths.iter().filter(|path| path.is_file()).count(), 1);
        assert_eq!(summary.stats.files_renamed, 1);
        assert_eq!(summary.outcome, ProcessingOutcome::Aborted);
    }

    #[test]
    fn dryrun_run_from_database_keeps_all_pending_rows_unchanged() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut database = Database::open_in_memory().expect("open db");
        let _files = pending_files(directory.path(), &database, &["first.mkv", "gone.mkv"]);
        std::fs::remove_file(directory.path().join("gone.mkv")).expect("remove gone");

        let options = ConvertOptions {
            dry_run: true,
            ..ConvertOptions::default()
        };
        let video_convert = converter(options);
        let summary = video_convert
            .run_from_database(&mut database, &AtomicBool::new(false))
            .expect("dryrun from db")
            .expect("should return summary");

        assert_eq!(summary.stats.files_converted, 1);
        assert_eq!(
            pending_paths(&database).len(),
            2,
            "dry-run must not remove pending rows from the database"
        );
    }

    #[test]
    fn preview_ignores_changed_rows_without_removing_them() {
        let directory = tempfile::tempdir().unwrap();
        let mut database = Database::open_in_memory().unwrap();
        let files = pending_files(directory.path(), &database, &["stale.mkv", "current.mkv"]);
        std::fs::write(&files[0].file.path, b"a replacement with different bytes").unwrap();

        let preview = converter(ConvertOptions {
            dry_run: true,
            ..ConvertOptions::default()
        });
        let summary = preview
            .run_from_database(&mut database, &AtomicBool::new(false))
            .unwrap()
            .unwrap();

        assert_eq!(summary.stats.files_converted, 1);
        assert_eq!(pending_paths(&database).len(), 2);
    }

    #[test]
    fn from_db_respects_min_resolution_include_exclude_and_skip_before_count() {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut database = Database::open_in_memory().expect("open db");

        let remux_path = directory.path().join("01_remux.mkv");
        let small_path = directory.path().join("02_small.mkv");
        let excluded_path = directory.path().join("03_sample_keep.mkv");
        let target_path = directory.path().join("04_keep_1080p.mkv");
        for path in [&remux_path, &small_path, &excluded_path, &target_path] {
            std::fs::write(path, b"video").expect("write");
        }

        let mut small_info = video_info("h264");
        small_info.width = 640;
        small_info.height = 480;
        let large_info = video_info("h264");
        let remux_info = video_info("hevc");

        database
            .upsert_pending_file(&remux_path, "mkv", &remux_info, PendingAction::Remux)
            .expect("insert remux");
        database
            .upsert_pending_file(&small_path, "mkv", &small_info, PendingAction::Convert)
            .expect("insert small");
        database
            .upsert_pending_file(&excluded_path, "mkv", &large_info, PendingAction::Convert)
            .expect("insert excluded");
        database
            .upsert_pending_file(&target_path, "mkv", &large_info, PendingAction::Convert)
            .expect("insert target");

        let options = ConvertOptions {
            dry_run: true,
            skip_remux: true,
            min_resolution: Some(1080),
            include: vec!["keep".to_string()],
            exclude: vec!["sample".to_string()],
            count: Some(1),
            sort: crate::options::SortOrder::Name,
            ..ConvertOptions::default()
        };
        let summary = converter(options)
            .run_from_database(&mut database, &AtomicBool::new(false))
            .expect("run from db")
            .expect("summary");

        assert_eq!(summary.stats.files_remuxed, 0);
        assert_eq!(
            summary.stats.files_converted, 1,
            "count=1 with skip_remux, min_resolution, include, and exclude should still convert 04_keep_1080p.mkv"
        );
    }

    #[test]
    fn from_db_allows_in_place_subtitle_mux_rows() {
        let directory = tempfile::tempdir().expect("temp dir");
        let movie_path = directory.path().join("Movie.x265.mkv");
        let srt_path = directory.path().join("Movie.x265.srt");
        std::fs::write(&srt_path, "1\n00:00:00,000 --> 00:00:00,500\nHello\n").expect("write srt");

        if cli_tools_common::command_exists("ffmpeg") {
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=64x64:d=1",
                    "-c:v",
                    "libx265",
                    "-tag:v",
                    "hvc1",
                    "-y",
                ])
                .arg(&movie_path)
                .status()
                .expect("generate mkv");
            assert!(status.success(), "ffmpeg should generate tiny sample mkv");
        } else {
            std::fs::write(&movie_path, b"video").expect("write dummy");
        }

        let processable_in_place = ProcessableFile::for_mode(
            VideoFile::new_with_metadata(&movie_path),
            video_info("hevc"),
            vec![crate::types::SubtitleFile::new(&srt_path, None)],
            true,
        );
        let mut reservations = TargetReservations::new();
        assert!(
            database_output_conflict_with_mode(&processable_in_place, false, true, &mut reservations).is_none(),
            "in-place SubtitleMux must not be flagged as an output conflict"
        );

        if cli_tools_common::command_exists("ffmpeg") {
            let mut database = Database::open_in_memory().expect("open db");
            database
                .upsert_pending_file(&movie_path, "mkv", &video_info("hevc"), PendingAction::SubtitleMux)
                .expect("insert subtitle mux");

            let options = ConvertOptions {
                dry_run: true,
                movie_mode: true,
                ..ConvertOptions::default()
            };
            let summary = converter(options)
                .run_from_database(&mut database, &AtomicBool::new(false))
                .expect("run from db")
                .expect("summary");

            assert_eq!(
                summary.stats.files_subtitle_muxed, 1,
                "in-place SubtitleMux row must proceed in run_from_database"
            );
        }
    }
}
