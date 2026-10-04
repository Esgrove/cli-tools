//! Video conversion orchestration.
//!
//! Discovers input files, analyzes them, and runs the processing batches from a scan or from the database.
//! The per-file ffmpeg operations live in `operations` and the subtitle sidecar handling in `subtitles`.

mod operations;
mod subtitles;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cli_tools::{print_error, print_yellow};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::VideoConvertArgs;
use crate::classification::{ClassificationRequest, probe_and_classify, should_include_file};
use crate::cli::{DatabaseMode, clean_scan_cache, clear_database, list_extensions, show_database_contents};
use crate::config::{Config, VideoConvertConfig};
use crate::database::{Database, PendingAction};
use crate::ffmpeg::probe_video_info;
use crate::helpers::{
    backup_output_path, duration_difference_ratio, format_duplicate_duration_match, has_enough_disk_space,
    paths_refer_to_same_file,
};
use crate::logger::FileLogger;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};
use crate::types::{
    AnalysisFilter, AnalysisOutput, AnalysisResult, ProcessResult, ProcessableFile, ProcessableFileSliceExt,
    ProcessingOutcome, SkipReason, SubtitleFile, VideoFile, VideoInfo, VideoInfoCache,
};

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

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
    #[allow(clippy::too_many_lines)]
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

        // Set up Ctrl+C handler for graceful abort
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

        let mut stats = RunStats::default();
        let mut outcome = ProcessingOutcome::Completed;
        let mut processed_count: usize = 0;

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
                cli_tools::count_label(candidate_files.len(), "candidate file", "candidate files")
            );
        }

        // Analyze files to determine required actions
        let mut analysis_output = self.analyze_files(candidate_files, &mut database, subtitle_matches);

        // Process renames: these files are already in a target codec but missing their codec suffix label
        if !analysis_output.renames.is_empty() {
            stats.files_renamed = self.process_renames(&analysis_output.renames);
        }

        // Update the database with files that need processing (single transaction)
        let mut pending_entries: Vec<(&Path, &str, &VideoInfo, PendingAction)> = Vec::new();
        for file in &analysis_output.remuxes {
            pending_entries.push((&file.file.path, &file.file.extension, &file.info, PendingAction::Remux));
        }
        for file in &analysis_output.subtitle_muxes {
            pending_entries.push((
                &file.file.path,
                &file.file.extension,
                &file.info,
                PendingAction::SubtitleMux,
            ));
        }
        for file in &analysis_output.conversions {
            pending_entries.push((
                &file.file.path,
                &file.file.extension,
                &file.info,
                PendingAction::Convert,
            ));
        }
        match database.batch_upsert_pending_files(&pending_entries) {
            Ok(db_added) if self.config.verbose && db_added > 0 => {
                println!("Updated {db_added} files in database");
            }
            Err(error) if self.config.verbose => {
                print_yellow!("Failed to update pending files in database: {error}");
            }
            _ => {}
        }

        // Calculate total files to process and truncate lists to respect config limit
        let remux_count = if self.config.skip_remux {
            0
        } else {
            analysis_output.remuxes.len()
        };
        let subtitle_mux_count = if self.config.skip_remux {
            0
        } else {
            analysis_output.subtitle_muxes.len()
        };
        let convert_count = if self.config.skip_convert {
            0
        } else {
            analysis_output.conversions.len()
        };
        let total_available = remux_count + subtitle_mux_count + convert_count;
        let total_limit = self.config.count.map_or(total_available, |c| total_available.min(c));

        // Truncate lists if they exceed the limit
        if let Some(count) = self.config.count
            && total_available > count
        {
            let remux_limit = remux_count.min(count);
            analysis_output.remuxes.truncate(remux_limit);
            let remaining = count.saturating_sub(remux_limit);
            let subtitle_mux_limit = subtitle_mux_count.min(remaining);
            analysis_output.subtitle_muxes.truncate(subtitle_mux_limit);
            let remaining = remaining.saturating_sub(subtitle_mux_limit);
            analysis_output.conversions.truncate(remaining);
        }

        // Process remuxes
        if !self.config.skip_remux && !analysis_output.remuxes.is_empty() {
            let (remux_stats, batch_outcome) = self.process_files_with_db_cleanup(
                analysis_output.remuxes,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::remux_to_mp4,
            );
            stats += remux_stats;
            outcome = batch_outcome;
        }

        // Process subtitle muxes
        if !self.config.skip_remux
            && !analysis_output.subtitle_muxes.is_empty()
            && outcome == ProcessingOutcome::Completed
        {
            let (subtitle_mux_stats, batch_outcome) = self.process_files_with_db_cleanup(
                analysis_output.subtitle_muxes,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::mux_subtitles,
            );
            stats += subtitle_mux_stats;
            outcome = batch_outcome;
        }

        // Process conversions
        if !self.config.skip_convert
            && !analysis_output.conversions.is_empty()
            && outcome == ProcessingOutcome::Completed
        {
            let (convert_stats, batch_outcome) = self.process_files_with_db_cleanup(
                analysis_output.conversions,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::convert_to_hevc,
            );
            stats += convert_stats;
            outcome = batch_outcome;
        }

        self.log_stats(&stats);

        if outcome != ProcessingOutcome::Completed {
            println!("\n{outcome}");
        }

        stats.print_summary();

        Ok(())
    }

    /// Gather video files based on the config settings.
    fn gather_files_to_process(&self) -> Result<Vec<VideoFile>> {
        let start = Instant::now();
        let max_depth = if self.config.recurse { usize::MAX } else { 1 };
        let mut files = Vec::new();
        let mut seen_paths = HashSet::new();

        for path in &self.config.paths {
            if path.is_file() {
                let file = VideoFile::new_with_metadata(path);
                if should_include_file(&self.config, &file) && seen_paths.insert(file.path.clone()) {
                    files.push(file);
                }
                continue;
            }

            if !path.is_dir() {
                anyhow::bail!("Input path '{}' does not exist or is not accessible", path.display());
            }

            files.extend(
                WalkDir::new(path)
                    .max_depth(max_depth)
                    .into_iter()
                    .filter_entry(|entry| !cli_tools::should_skip_entry(entry))
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| entry.file_type().is_file())
                    .map(VideoFile::from)
                    .filter(|file| should_include_file(&self.config, file))
                    .filter(|file| seen_paths.insert(file.path.clone())),
            );
        }

        files.sort_unstable();

        self.log_gathered_files(files.len(), start.elapsed());

        Ok(files)
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
                    print_error!("{}: {error}", cli_tools::path_to_string_relative(&file.file.path));
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

    /// Run video conversion from files stored in the database.
    ///
    /// This skips the scanning/analysis phase and processes files directly from the database.
    /// Supports filtering by extension, bitrate, and duration via CLI arguments.
    #[allow(clippy::too_many_lines)]
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
                    cli_tools::count_label(changed, "file changed since the scan", "files changed since the scan")
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
                cli_tools::count_label(pending_files.len(), "pending file", "pending files")
            );
        }

        // Set up Ctrl+C handler for graceful abort
        let abort_flag = Arc::new(AtomicBool::new(false));
        let abort_flag_handler = Arc::clone(&abort_flag);

        ctrlc::set_handler(move || {
            if abort_flag_handler.load(Ordering::SeqCst) {
                std::process::exit(130);
            }
            println!("\n{}", "Received Ctrl+C, finishing current file...".yellow().bold());
            abort_flag_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl+C handler");

        let mut stats = RunStats::default();
        let mut outcome = ProcessingOutcome::Completed;
        let mut processed_count: usize = 0;

        // Calculate limits
        let remux_count = if self.config.skip_remux {
            0
        } else {
            pending_files
                .iter()
                .filter(|file| file.action == PendingAction::Remux)
                .count()
        };
        let subtitle_mux_count = if self.config.skip_remux {
            0
        } else {
            pending_files
                .iter()
                .filter(|file| file.action == PendingAction::SubtitleMux)
                .count()
        };
        let convert_count = if self.config.skip_convert {
            0
        } else {
            pending_files
                .iter()
                .filter(|file| file.action == PendingAction::Convert)
                .count()
        };
        let total_available = remux_count + subtitle_mux_count + convert_count;
        let total_limit = self.config.count.map_or(total_available, |c| total_available.min(c));

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
        let mut remux_files = Vec::new();
        let mut subtitle_mux_files = Vec::new();
        let mut conversion_files = Vec::new();

        for pending_file in pending_files.into_iter().filter(|file| file.full_path.exists()) {
            let video_file = VideoFile::new_with_metadata(&pending_file.full_path);
            let subtitles = subtitle_matches.remove(&video_file.path).unwrap_or_default();
            let info = if pending_file.bit_depth >= 8 {
                pending_file.to_video_info()
            } else {
                probe_video_info(&pending_file.full_path).unwrap_or_else(|_| pending_file.to_video_info())
            };
            let processable = ProcessableFile::for_mode(video_file, info, subtitles, self.config.movie_mode);
            match pending_file.action {
                PendingAction::Convert => conversion_files.push(processable),
                PendingAction::Remux => remux_files.push(processable),
                PendingAction::SubtitleMux => subtitle_mux_files.push(processable),
            }
        }

        // Sort files
        remux_files.sort_by_order(self.config.sort);
        subtitle_mux_files.sort_by_order(self.config.sort);
        conversion_files.sort_by_order(self.config.sort);

        // Truncate lists if they exceed the limit
        if let Some(count) = self.config.count
            && total_available > count
        {
            let remux_limit = remux_files.len().min(count);
            remux_files.truncate(remux_limit);
            let remaining = count.saturating_sub(remux_limit);
            let subtitle_mux_limit = subtitle_mux_files.len().min(remaining);
            subtitle_mux_files.truncate(subtitle_mux_limit);
            let remaining = remaining.saturating_sub(subtitle_mux_limit);
            conversion_files.truncate(remaining);
        }

        // Process remuxes
        if !self.config.skip_remux && !remux_files.is_empty() {
            let (remux_stats, batch_outcome) = self.process_files_with_db_cleanup(
                remux_files,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::remux_to_mp4,
            );
            stats += remux_stats;
            outcome = batch_outcome;
        }

        // Process subtitle muxes
        if !self.config.skip_remux && !subtitle_mux_files.is_empty() && outcome == ProcessingOutcome::Completed {
            let (subtitle_mux_stats, batch_outcome) = self.process_files_with_db_cleanup(
                subtitle_mux_files,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::mux_subtitles,
            );
            stats += subtitle_mux_stats;
            outcome = batch_outcome;
        }

        // Process conversions
        if !self.config.skip_convert && !conversion_files.is_empty() && outcome == ProcessingOutcome::Completed {
            let (convert_stats, batch_outcome) = self.process_files_with_db_cleanup(
                conversion_files,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::convert_to_hevc,
            );
            stats += convert_stats;
            outcome = batch_outcome;
        }

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

    /// Analyze files in parallel to determine which need processing.
    /// Runs ffprobe on each file concurrently and filters based on video information.
    #[allow(clippy::too_many_lines)]
    fn analyze_files(
        &self,
        files: Vec<VideoFile>,
        database: &mut Database,
        mut subtitle_matches: HashMap<PathBuf, Vec<SubtitleFile>>,
    ) -> AnalysisOutput {
        let start = Instant::now();
        let total_files = files.len();

        // Extract config values needed for analysis to avoid borrowing self in parallel context
        let filter = AnalysisFilter::from(&self.config);
        let movie_mode = self.config.movie_mode;

        // Phase 1 (sequential): bulk-load the scan cache into a HashMap for O(1) lookups,
        // then split files into cache hits (classified immediately) and cache misses.
        let scan_cache: HashMap<String, VideoInfo> = database.get_all_scanned_files().unwrap_or_default();
        let mut cache_results: Vec<AnalysisResult> = Vec::new();
        let mut cache_misses: Vec<(VideoFile, Vec<SubtitleFile>)> = Vec::new();

        for file in files {
            let path_key = file.path.to_string_lossy();
            let subtitles = subtitle_matches.remove(&file.path).unwrap_or_default();
            if let Some(cached_info) = scan_cache.get(path_key.as_ref())
                && cached_info.size_bytes == file.size_bytes
                && cached_info.bit_depth >= 8
            {
                cache_results
                    .push(ClassificationRequest::new(&filter, cached_info, movie_mode, subtitles).classify(file));
            } else {
                cache_misses.push((file, subtitles));
            }
        }

        let cache_hit_count = cache_results.len();
        let cache_miss_count = cache_misses.len();
        if self.config.verbose && cache_hit_count > 0 {
            println!(
                "Scan cache: {}, {} — running ffprobe on {}",
                cli_tools::count_label(cache_hit_count, "hit", "hits"),
                cli_tools::count_label(cache_miss_count, "miss", "misses"),
                cli_tools::count_label(cache_miss_count, "file", "files")
            );
        }

        // Phase 2 (parallel): run ffprobe only on cache misses.
        let probe_results: Vec<VideoInfoCache> = if cache_misses.is_empty() {
            Vec::new()
        } else {
            let progress_bar = ProgressBar::new(cache_miss_count as u64);
            progress_bar.set_style(
                ProgressStyle::default_bar()
                    .template(PROGRESS_BAR_TEMPLATE)
                    .expect("Failed to set progress bar template")
                    .progress_chars(PROGRESS_BAR_CHARS),
            );

            let results: Vec<VideoInfoCache> = cache_misses
                .into_par_iter()
                .map(|(file, subtitles)| {
                    let result = probe_and_classify(file, subtitles, &filter, movie_mode);
                    progress_bar.inc(1);
                    result
                })
                .collect();
            progress_bar.finish_and_clear();
            results
        };

        // Phase 3: write new ffprobe results back to the scan cache in a single transaction.
        let cache_entries: Vec<(&Path, &VideoInfo)> = probe_results
            .iter()
            .filter_map(|entry| entry.info.as_ref().map(|info| (entry.path.as_path(), info)))
            .collect();
        if let Err(error) = database.batch_upsert_scanned_files(&cache_entries)
            && self.config.verbose
        {
            print_yellow!("Failed to write scan cache: {error}");
        }

        // Combine cache hits (Phase 1) with ffprobe results (Phase 2)
        let mut results = cache_results;
        results.extend(probe_results.into_iter().map(|entry| entry.result));

        // Collect files into separate vectors
        let mut conversions = Vec::new();
        let mut remuxes = Vec::new();
        let mut renames: Vec<ProcessableFile> = Vec::new();
        let mut subtitle_muxes = Vec::new();
        let mut analysis_stats = AnalysisStats::default();
        // Collect duplicate pairs for verbose output when not deleting
        let mut duplicate_pairs: Vec<(PathBuf, PathBuf)> = Vec::new();

        for result in results {
            match result {
                AnalysisResult::NeedsConversion(processable) => {
                    analysis_stats.to_convert += 1;
                    conversions.push(processable);
                }
                AnalysisResult::NeedsRemux(processable) => {
                    analysis_stats.to_remux += 1;
                    remuxes.push(processable);
                }
                AnalysisResult::NeedsRename(processable) => {
                    analysis_stats.to_rename += 1;
                    renames.push(processable);
                }
                AnalysisResult::NeedsSubtitleMux(processable) => {
                    analysis_stats.to_subtitle_mux += 1;
                    subtitle_muxes.push(processable);
                }
                AnalysisResult::Skip { file, reason } => {
                    match &reason {
                        SkipReason::AlreadyConverted => {
                            analysis_stats.skipped_converted += 1;
                        }
                        SkipReason::BitrateBelowThreshold { .. } => {
                            analysis_stats.skipped_bitrate_low += 1;
                        }
                        SkipReason::BitrateAboveThreshold { .. } => {
                            analysis_stats.skipped_bitrate_high += 1;
                        }
                        SkipReason::DurationBelowThreshold { .. } => {
                            analysis_stats.skipped_duration_short += 1;
                        }
                        SkipReason::DurationAboveThreshold { .. } => {
                            analysis_stats.skipped_duration_long += 1;
                        }
                        SkipReason::ResolutionBelowLimit { .. } => {
                            analysis_stats.skipped_resolution_low += 1;
                        }
                        SkipReason::OutputExists {
                            path: target_path,
                            source_duration,
                        } => {
                            if self.config.delete_duplicates {
                                if paths_refer_to_same_file(&file.path, target_path) {
                                    print_error!(
                                        "Refusing to delete duplicate because source and target are the same file:\n  {}",
                                        cli_tools::path_to_string_relative(&file.path)
                                    );
                                    analysis_stats.duplicate_delete_failed += 1;
                                    continue;
                                }

                                // Check target duration and delete source if within 10%
                                match probe_video_info(target_path) {
                                    Ok(target_info) => {
                                        if !target_info.is_target_codec() {
                                            print_error!(
                                                "Refusing to delete duplicate because target is not HEVC/AV1:\n  Source: {}\n  Target: {}",
                                                cli_tools::path_to_string_relative(&file.path),
                                                cli_tools::path_to_string_relative(target_path)
                                            );
                                            analysis_stats.duplicate_delete_failed += 1;
                                            continue;
                                        }

                                        let duration_ratio =
                                            duration_difference_ratio(*source_duration, target_info.duration);
                                        if duration_ratio <= 0.1 {
                                            let duration_message =
                                                format_duplicate_duration_match(*source_duration, target_info.duration);
                                            // Duration within 10%, safe to delete source
                                            if self.config.dryrun {
                                                println!(
                                                    "{} ({duration_message})",
                                                    format!(
                                                        "Would delete duplicate: {}",
                                                        cli_tools::path_to_string_relative(&file.path)
                                                    )
                                                    .yellow()
                                                );
                                                analysis_stats.duplicates_deleted += 1;
                                            } else if let Err(error) = self.delete_file(&file.path) {
                                                print_error!(
                                                    "Failed to delete duplicate {}: {error}",
                                                    cli_tools::path_to_string_relative(&file.path)
                                                );
                                                analysis_stats.duplicate_delete_failed += 1;
                                            } else {
                                                println!(
                                                    "{} ({duration_message})",
                                                    format!(
                                                        "Deleted duplicate: {}",
                                                        cli_tools::path_to_string_relative(&file.path)
                                                    )
                                                    .green()
                                                );
                                                analysis_stats.duplicates_deleted += 1;
                                            }
                                        } else {
                                            // Duration mismatch, log error
                                            print_error!(
                                                "Duration mismatch for duplicate - source: {:.1}s, target: {:.1}s ({:.3}% difference)\n  Source: {}\n  Target: {}",
                                                source_duration,
                                                target_info.duration,
                                                duration_ratio * 100.0,
                                                cli_tools::path_to_string_relative(&file.path),
                                                cli_tools::path_to_string_relative(target_path)
                                            );
                                            analysis_stats.duplicate_delete_failed += 1;
                                        }
                                    }
                                    Err(error) => {
                                        print_error!(
                                            "Failed to get duration of target file {}: {error}",
                                            cli_tools::path_to_string_relative(target_path)
                                        );
                                        analysis_stats.duplicate_delete_failed += 1;
                                    }
                                }
                            } else {
                                analysis_stats.skipped_duplicate += 1;
                                if self.config.verbose {
                                    duplicate_pairs.push((file.path.clone(), target_path.clone()));
                                }
                            }
                        }
                        SkipReason::FileMissing => {
                            analysis_stats.file_missing += 1;
                            print_yellow!(
                                "{}: File no longer exists (may have been moved or renamed)",
                                cli_tools::path_to_string_relative(&file.path)
                            );
                        }
                        SkipReason::AnalysisFailed { error } => {
                            analysis_stats.analysis_failed += 1;
                            print_error!("{}: {error}", cli_tools::path_to_string_relative(&file.path));
                        }
                    }
                    // Print skipped files (except OutputExists which is handled above, and AnalysisFailed)
                    if self.config.verbose
                        && !matches!(reason, SkipReason::AnalysisFailed { .. })
                        && !matches!(reason, SkipReason::OutputExists { .. })
                        && !matches!(reason, SkipReason::FileMissing)
                    {
                        print_yellow!("{}: {reason}", cli_tools::path_to_string_relative(&file.path));
                    }
                }
            }
        }

        // Print duplicate pairs if verbose and not deleting duplicates
        if self.config.verbose && !self.config.delete_duplicates && !duplicate_pairs.is_empty() {
            println!();
            println!("{}", "Duplicate pairs:".bold());
            for (source, target) in &duplicate_pairs {
                println!("  {}", cli_tools::path_to_string_relative(source));
                println!("  {}", cli_tools::path_to_string_relative(target));
                println!();
            }
        }

        // Sort conversions based on configured sort order
        conversions.sort_by_order(self.config.sort);
        remuxes.sort_by_order(self.config.sort);
        subtitle_muxes.sort_by_order(self.config.sort);

        self.log_analysis_stats(&analysis_stats, total_files, start.elapsed());
        analysis_stats.print_summary();

        AnalysisOutput {
            conversions,
            remuxes,
            renames,
            subtitle_muxes,
        }
    }

    /// Process all files that need renaming. Returns the number of files successfully renamed.
    fn process_renames(&self, files: &[ProcessableFile]) -> usize {
        let start = Instant::now();
        let total = files.len();
        let num_digits = total.checked_ilog10().map_or(1, |d| d as usize + 1);
        let mut renamed_count = 0;

        for (index, file) in files.iter().enumerate() {
            let file_index = format!("[{:>width$}/{total}]", index + 1, width = num_digits);

            if self.config.dryrun {
                println!("{}", format!("{file_index} [DRYRUN] Rename:").bold().purple());
                cli_tools::show_diff(
                    &cli_tools::path_to_string_relative(&file.file.path),
                    &cli_tools::path_to_string_relative(&file.output_path),
                );
                renamed_count += 1;
            } else {
                println!("{}", format!("{file_index} Rename:").bold().purple());
                cli_tools::show_diff(
                    &cli_tools::path_to_string_relative(&file.file.path),
                    &cli_tools::path_to_string_relative(&file.output_path),
                );
                if let Err(e) = std::fs::rename(&file.file.path, &file.output_path) {
                    print_error!("Failed to rename {}: {e}", file.file.path.display());
                } else {
                    renamed_count += 1;
                }
            }
        }

        self.log_renames(renamed_count, total, start.elapsed());
        renamed_count
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

    fn delete_file(&self, path: &Path) -> Result<()> {
        // Use direct delete if configured or if on a Windows network drive (trash doesn't work there)
        if self.config.delete || cli_tools::is_network_path(path) {
            println!("Deleting: {}", path.display());
            std::fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            println!("Trashing: {}", path.display());
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
    }

    fn delete_subtitle_files(&self, subtitle_files: &[SubtitleFile]) {
        for subtitle_file in subtitle_files {
            for path in subtitle_file.paths_to_delete() {
                if let Err(error) = self.delete_file(path) {
                    print_error!("Failed to delete subtitle file {}: {error}", path.display());
                }
            }
        }
    }

    fn replace_input_with_output(&self, input: &Path, output: &Path) -> Result<()> {
        let backup = backup_output_path(input);
        std::fs::rename(input, &backup).context("Failed to move original file aside before replacement")?;

        if let Err(error) = std::fs::rename(output, input) {
            if let Err(restore_error) = std::fs::rename(&backup, input) {
                anyhow::bail!(
                    "Failed to move muxed subtitle output into place: {error}, failed to restore original file: {restore_error}"
                );
            }
            return Err(error).context("Failed to move muxed subtitle output into place");
        }

        if let Err(error) = self.delete_file(&backup) {
            print_error!("Failed to delete replaced original file {}: {error}", backup.display());
        }
        Ok(())
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

#[cfg(test)]
mod test_gather_and_rename {
    use super::test_helpers::{converter, processable};
    use super::*;

    fn config_for(path: &Path, recurse: bool) -> Config {
        Config {
            paths: vec![path.to_path_buf()],
            recurse,
            extensions: vec!["mkv".to_string(), "mp4".to_string()],
            ..Config::default()
        }
    }

    #[test]
    fn gathers_matching_extensions_and_respects_recursion() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let root = directory.path().join("videos");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("Failed to create nested directory");
        std::fs::write(root.join("top.mkv"), b"video").expect("Failed to write video file");
        std::fs::write(root.join("notes.txt"), b"notes").expect("Failed to write text file");
        std::fs::write(nested.join("deep.mp4"), b"video").expect("Failed to write nested video file");

        let (_log, shallow) = converter(config_for(&root, false));
        let (_log_recursive, recursive) = converter(config_for(&root, true));

        let shallow_names: Vec<String> = shallow
            .gather_files_to_process()
            .expect("Failed to gather files")
            .into_iter()
            .map(|file| file.name)
            .collect();
        let recursive_count = recursive
            .gather_files_to_process()
            .expect("Failed to gather files")
            .len();

        assert_eq!(shallow_names, vec!["top"]);
        assert_eq!(recursive_count, 2);
    }

    #[test]
    fn gathers_multiple_files_and_directories_without_duplicates() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let first_root = directory.path().join("first");
        let second_root = directory.path().join("second");
        let nested_root = first_root.join("nested");
        std::fs::create_dir_all(&nested_root).expect("Failed to create nested directory");
        std::fs::create_dir(&second_root).expect("Failed to create second directory");
        let first_video = first_root.join("first.mkv");
        let second_video = second_root.join("second.mp4");
        let nested_video = nested_root.join("nested.mkv");
        let standalone_video = directory.path().join("standalone.mp4");
        let ignored_file = directory.path().join("notes.txt");
        for path in [
            &first_video,
            &second_video,
            &nested_video,
            &standalone_video,
            &ignored_file,
        ] {
            std::fs::write(path, b"fixture").expect("Failed to write fixture");
        }
        let first_subtitle = first_root.join("first.srt");
        let standalone_subtitle = directory.path().join("standalone.srt");
        for path in [&first_subtitle, &standalone_subtitle] {
            std::fs::write(path, b"subtitle").expect("Failed to write subtitle fixture");
        }

        let mut config = config_for(&first_root, true);
        config.paths.extend([
            second_root,
            standalone_video.clone(),
            first_video.clone(),
            nested_root,
            ignored_file,
            first_root,
        ]);
        let (_log, converter) = converter(config);
        let files = converter.gather_files_to_process().expect("Failed to gather files");
        let paths: HashSet<PathBuf> = files.iter().map(|file| file.path.clone()).collect();

        assert_eq!(files.len(), 4);
        assert_eq!(
            paths,
            HashSet::from([first_video, second_video, nested_video, standalone_video])
        );
        let subtitles = VideoConvert::gather_subtitle_files_for_video_files(&files);
        assert_eq!(subtitles.len(), 2);
        assert_eq!(
            subtitles.into_iter().map(|file| file.path).collect::<HashSet<_>>(),
            HashSet::from([first_subtitle, standalone_subtitle])
        );
    }

    #[test]
    fn multiple_directories_respect_recursion_and_filters() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let roots = [directory.path().join("first"), directory.path().join("second")];
        for root in &roots {
            let nested = root.join("nested");
            std::fs::create_dir_all(&nested).expect("Failed to create nested directory");
            for path in [
                root.join("keep.mkv"),
                root.join("skip.mp4"),
                nested.join("keep.deep.mkv"),
            ] {
                std::fs::write(path, b"video").expect("Failed to write video fixture");
            }
        }
        let mut config = config_for(&roots[0], false);
        config.paths.push(roots[1].clone());
        config.include = vec!["keep".to_string()];
        let (_log, converter) = converter(config);

        let files = converter.gather_files_to_process().expect("Failed to gather files");

        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.name == "keep"));
    }

    #[test]
    fn a_single_file_path_is_gathered_when_it_matches() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let video = directory.path().join("clip.mkv");
        let text = directory.path().join("clip.txt");
        std::fs::write(&video, b"video").expect("Failed to write video file");
        std::fs::write(&text, b"notes").expect("Failed to write text file");

        let (_log, video_converter) = converter(config_for(&video, false));
        let (_log_text, text_converter) = converter(config_for(&text, false));

        assert_eq!(
            video_converter
                .gather_files_to_process()
                .expect("Failed to gather files")
                .len(),
            1
        );
        assert_eq!(
            text_converter
                .gather_files_to_process()
                .expect("Failed to gather files"),
            [] as [VideoFile; 0]
        );
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let (_log, converter) = converter(config_for(&directory.path().join("missing"), false));

        assert!(converter.gather_files_to_process().is_err());
    }

    #[test]
    fn renames_move_files_to_their_output_names() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "hevc");
        let output = file.output_path.clone();
        let (_log, converter) = converter(Config::default());

        let renamed = converter.process_renames(&[file]);

        assert_eq!(renamed, 1);
        assert!(!path.exists());
        assert!(output.is_file());
    }

    #[test]
    fn dryrun_renames_count_without_moving_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let (_log, converter) = converter(Config {
            dryrun: true,
            ..Config::default()
        });

        let renamed = converter.process_renames(&[processable(&path, "hevc")]);

        assert_eq!(renamed, 1);
        assert!(path.is_file());
    }

    #[test]
    fn a_failed_rename_is_not_counted() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let missing = directory.path().join("Missing.mp4");
        let (_log, converter) = converter(Config::default());

        assert_eq!(converter.process_renames(&[processable(&missing, "hevc")]), 0);
    }
}
