//! Video conversion orchestration and media processing.
//!
//! Discovers input files, runs ffmpeg operations, manages processing batches, and validates their outputs.

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

use crate::classification::{ClassificationRequest, RE_AV1, RE_X265};
use crate::cli::{DatabaseMode, clean_scan_cache, clear_database, list_extensions, show_database_contents};
use crate::config::{Config, VideoConvertConfig};
use crate::database::{Database, PendingAction};
use crate::ffmpeg::{
    ConversionOptions, build_conversion_command, build_remux_command, build_subtitle_mux_command, probe_video_info,
    run_command_isolated, validate_mux_output,
};
use crate::helpers::{
    duration_difference_ratio, format_duplicate_duration_match, path_without_extension, paths_refer_to_same_file,
};
use crate::logger::FileLogger;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};
use crate::types::{
    AnalysisFilter, AnalysisOutput, AnalysisResult, ProcessResult, ProcessableFile, ProcessingOutcome, SkipReason,
    SubtitleFile, VideoFile, VideoInfo, VideoInfoCache, movie_subtitle_match_score,
};
use crate::{SortOrder, VideoConvertArgs};

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Minimum ratio of output duration to input duration for a conversion to be considered successful.
const MIN_DURATION_RATIO: f64 = 0.85;

/// Minimum free disk space required before converting a file, as a multiple of the
/// original file size.
const MIN_DISK_SPACE_FACTOR: u64 = 2;

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
                DatabaseMode::Clear => clear_database(),
                DatabaseMode::Show => show_database_contents(&self.config),
                DatabaseMode::ListExtensions => list_extensions(self.config.verbose),
                DatabaseMode::CleanScanCache => clean_scan_cache(self.config.verbose),
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
            let subtitle_files = self.gather_subtitle_files_to_process();
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
        let path = &self.config.path;

        if path.is_file() {
            let file = VideoFile::new_with_metadata(path);
            return if self.should_include_file(&file) {
                Ok(vec![file])
            } else {
                Ok(vec![])
            };
        }

        // Path must be a directory
        if !path.is_dir() {
            anyhow::bail!("Input path '{}' does not exist or is not accessible", path.display());
        }

        let max_depth = if self.config.recurse { usize::MAX } else { 1 };

        let mut files: Vec<VideoFile> = WalkDir::new(path)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|entry| !cli_tools::should_skip_entry(entry))
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(VideoFile::from)
            .filter(|file| self.should_include_file(file))
            .collect();

        files.sort_unstable();

        self.log_gathered_files(files.len(), start.elapsed());

        Ok(files)
    }

    /// Gather supported external subtitle sidecars for movie-mode matching.
    fn gather_subtitle_files_to_process(&self) -> Vec<SubtitleFile> {
        let path = &self.config.path;
        if path.is_file() {
            let Some(parent) = path.parent() else {
                return Vec::new();
            };
            return Self::gather_subtitle_files_from_root(parent, 1);
        }

        if !path.is_dir() {
            return Vec::new();
        }

        let max_depth = if self.config.recurse { usize::MAX } else { 1 };
        Self::gather_subtitle_files_from_root(path, max_depth)
    }

    /// Gather subtitle sidecars from the directories containing the given video files.
    fn gather_subtitle_files_for_video_files(video_files: &[VideoFile]) -> Vec<SubtitleFile> {
        let mut subtitle_files = Vec::new();
        let mut seen_directories = HashSet::new();

        for video_file in video_files {
            let Some(parent) = video_file.path.parent() else {
                continue;
            };
            if seen_directories.insert(parent.to_path_buf()) {
                subtitle_files.extend(Self::gather_subtitle_files_from_root(parent, 1));
            }
        }

        subtitle_files
    }

    /// Gather supported subtitle sidecar files below a root path.
    fn gather_subtitle_files_from_root(root: &Path, max_depth: usize) -> Vec<SubtitleFile> {
        let subtitle_paths: Vec<PathBuf> = WalkDir::new(root)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|entry| !cli_tools::should_skip_entry(entry))
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(walkdir::DirEntry::into_path)
            .filter(|path| SubtitleFile::is_supported_extension(&cli_tools::path_to_file_extension_string(path)))
            .collect();

        let idx_stems: HashSet<PathBuf> = subtitle_paths
            .iter()
            .filter(|path| cli_tools::path_to_file_extension_string(path) == "idx")
            .map(|path| path_without_extension(path))
            .collect();

        let mut subtitle_files: Vec<SubtitleFile> = subtitle_paths
            .into_iter()
            .filter_map(|path| {
                let extension = cli_tools::path_to_file_extension_string(&path);
                if extension == "sub" && idx_stems.contains(&path_without_extension(&path)) {
                    return None;
                }
                let paired_sub_path = if extension == "idx" {
                    let pair = path.with_extension("sub");
                    pair.exists().then_some(pair)
                } else {
                    None
                };
                Some(SubtitleFile::new(&path, paired_sub_path))
            })
            .collect();
        subtitle_files.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        subtitle_files
    }

    /// Match subtitle sidecars to video files in the same directory using normalized title tokens.
    fn match_subtitle_files(
        video_files: &[VideoFile],
        subtitle_files: Vec<SubtitleFile>,
        verbose: bool,
    ) -> HashMap<PathBuf, Vec<SubtitleFile>> {
        let mut matches: HashMap<PathBuf, Vec<SubtitleFile>> = HashMap::new();

        for subtitle_file in subtitle_files {
            let subtitle_stem = cli_tools::path_to_file_stem_string(&subtitle_file.path);
            let mut candidates: Vec<(usize, &VideoFile)> = video_files
                .iter()
                .filter(|video_file| video_file.path.parent() == subtitle_file.path.parent())
                .filter_map(|video_file| {
                    movie_subtitle_match_score(&video_file.name, &subtitle_stem).map(|score| (score, video_file))
                })
                .collect();

            candidates.sort_unstable_by_key(|(score, _)| std::cmp::Reverse(*score));
            let Some((best_score, best_video)) = candidates.first() else {
                if verbose {
                    print_yellow!("No matching video found for subtitle: {}", subtitle_file.path.display());
                }
                continue;
            };

            if candidates.iter().filter(|(score, _)| score == best_score).count() > 1 {
                if verbose {
                    print_yellow!("Ambiguous subtitle match skipped: {}", subtitle_file.path.display());
                }
                continue;
            }

            matches.entry(best_video.path.clone()).or_default().push(subtitle_file);
        }

        matches
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
                let _ = database.remove_pending_file(&file.file.path);
                continue;
            }

            // Ensure there is enough free disk space before starting the conversion
            if !Self::has_enough_disk_space(&file) {
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
                    // Remove from database after successful processing
                    let _ = database.remove_pending_file(&file.file.path);
                }
            }

            stats.add_result(&result, duration);
        }

        (stats, outcome)
    }

    /// Check that the output volume has enough free space for the given file.
    ///
    /// Requires at least `MIN_DISK_SPACE_FACTOR` times the original file size to be free.
    /// Prints an out-of-disk-space error and returns `false` when the space is insufficient.
    /// If the available space cannot be determined, the check passes.
    fn has_enough_disk_space(file: &ProcessableFile) -> bool {
        let original_size = file.info.size_bytes;
        let required = original_size.saturating_mul(MIN_DISK_SPACE_FACTOR);
        let Some(available) = cli_tools::available_disk_space(&file.output_path) else {
            return true;
        };

        if available < required {
            print_error!(
                "Out of disk space: converting {} needs {} free but only {} is available",
                cli_tools::path_to_string_relative(&file.file.path),
                cli_tools::format_size(required),
                cli_tools::format_size(available),
            );
            return false;
        }

        true
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
            let processable = self.processable_file(video_file, info, subtitles);
            match pending_file.action {
                PendingAction::Convert => conversion_files.push(processable),
                PendingAction::Remux => remux_files.push(processable),
                PendingAction::SubtitleMux => subtitle_mux_files.push(processable),
            }
        }

        // Sort files
        Self::sort_processable_files(&mut remux_files, self.config.sort);
        Self::sort_processable_files(&mut subtitle_mux_files, self.config.sort);
        Self::sort_processable_files(&mut conversion_files, self.config.sort);

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

    /// Remux HEVC or AV1 to MP4 container
    fn remux_to_mp4(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let codec = info.codec_suffix();

        println!(
            "{}",
            format!("{file_index} Remux: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .green()
        );
        println!("{info}");

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(output));
        }

        self.log_start(input, "remux", file_index, info, None);
        let start = Instant::now();

        let mut cmd = build_remux_command(input, output, false, codec);

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::Remuxed {};
        }

        let status = match run_command_isolated(&mut cmd) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if status.success() {
            if let Err(e) = self.delete_file(input) {
                print_error!("Failed to delete original file: {e}");
            }
            let duration = start.elapsed();
            println!(
                "{}",
                format!("✓ Remuxed in {}", cli_tools::format_duration(duration)).green()
            );
            self.log_success(output, "remux", file_index, duration, None);
            return ProcessResult::Remuxed {};
        }

        // Fallback: if audio codec is not MP4-friendly, transcode audio to AAC
        print_yellow!("Remux failed with code {status}. Retrying with AAC audio transcode...");

        // Remove failed output file if it exists
        if output.exists() {
            let _ = std::fs::remove_file(output);
        }

        let mut cmd = build_remux_command(input, output, true, codec);

        let status = match run_command_isolated(&mut cmd) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if !status.success() {
            let _ = std::fs::remove_file(output);
            let error = format!(
                "ffmpeg remux with AAC transcode failed with status: {}",
                status.code().unwrap_or(-1)
            );
            self.log_failure(input, "remux", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if let Err(e) = self.delete_file(input) {
            print_error!("Failed to delete original file: {e}");
        }

        let duration = start.elapsed();
        println!(
            "{}",
            format!("✓ Remuxed in {}", cli_tools::format_duration(duration)).green()
        );
        self.log_success(output, "remux", file_index, duration, None);
        ProcessResult::Remuxed {}
    }

    /// Process movie-mode audio and subtitle streams without converting video.
    fn mux_subtitles(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let final_output = &file.output_path;
        let command_output = if input == final_output {
            Self::temporary_output_path(final_output)
        } else {
            final_output.clone()
        };
        let info = &file.info;

        println!(
            "{}",
            format!(
                "{file_index} Process movie streams: {}",
                cli_tools::path_to_string_relative(input)
            )
            .bold()
            .cyan()
        );
        println!("{info}");

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(final_output));
            Self::print_subtitle_files(&file.subtitle_files);
        }

        self.log_start(input, "subtitle-mux", file_index, info, None);
        let start = Instant::now();

        let mut cmd = match build_subtitle_mux_command(input, &command_output, &file.subtitle_files) {
            Ok(command) => command,
            Err(error) => {
                return ProcessResult::Failed {
                    error: format!("Failed to build subtitle mux command: {error}"),
                };
            }
        };

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::SubtitlesMuxed {};
        }

        let status = match run_command_isolated(&mut cmd) {
            Ok(status) => status,
            Err(error) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {error}"),
                };
            }
        };

        if !status.success() {
            let _ = std::fs::remove_file(&command_output);
            let error = format!(
                "ffmpeg subtitle mux failed with status: {}",
                status.code().unwrap_or(-1)
            );
            self.log_failure(input, "subtitle-mux", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if let Err(error) = validate_mux_output(input, &command_output, info) {
            if let Err(delete_error) = self.delete_file(&command_output) {
                print_error!("Failed to delete invalid subtitle mux output: {delete_error}");
            }
            let error = error.to_string();
            self.log_failure(input, "subtitle-mux", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if input == final_output {
            if let Err(error) = self.replace_input_with_output(input, &command_output) {
                let error = error.to_string();
                self.log_failure(input, "subtitle-mux", file_index, &error);
                return ProcessResult::Failed { error };
            }
        } else if let Err(error) = self.delete_file(input) {
            print_error!("Failed to delete original file: {error}");
        }

        self.delete_subtitle_files(&file.subtitle_files);

        let duration = start.elapsed();
        println!(
            "{}",
            format!("✓ Processed movie streams in {}", cli_tools::format_duration(duration)).green()
        );
        self.log_success(final_output, "subtitle-mux", file_index, duration, None);
        ProcessResult::SubtitlesMuxed {}
    }

    /// Convert video to HEVC using NVENC
    #[allow(clippy::too_many_lines)]
    fn convert_to_hevc(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let extension = &file.file.extension;

        println!(
            "{}",
            format!("{file_index} Convert: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .magenta()
        );
        println!("{info}");

        let quality_level = if self.config.movie_mode {
            info.quality_level().saturating_sub(2)
        } else {
            info.quality_level()
        };

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(output));
            println!("Using quality level: {quality_level}");
            Self::print_subtitle_files(&file.subtitle_files);
        }

        self.log_start(input, "convert", file_index, info, Some(quality_level));
        let start = Instant::now();

        // Determine audio codec: copy for mp4/mkv, transcode for others
        let copy_audio = extension == "mp4" || extension == "mkv";

        let mut conversion_options = ConversionOptions::new(
            input,
            output,
            quality_level,
            copy_audio,
            self.config.movie_mode,
            &file.subtitle_files,
            info.bit_depth,
        );
        let mut ffmpeg_command = match build_conversion_command(&conversion_options) {
            Ok(command) => command,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to build ffmpeg command: {e}"),
                };
            }
        };

        if self.config.dryrun {
            println!("[DRYRUN] {ffmpeg_command:#?}");
            return ProcessResult::converted(info.size_bytes, info.bitrate_kbps, 0, 0);
        }

        // First attempt: try with CUDA filters for better performance
        let status = match run_command_isolated(&mut ffmpeg_command) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if !status.success() {
            // Clean up failed output file
            let _ = std::fs::remove_file(output);

            // Retry without CUDA filters (fallback for format compatibility issues)
            print_error!("CUDA filter failed, retrying with CPU-based filtering...");
            conversion_options = conversion_options.without_cuda_filters();
            ffmpeg_command = match build_conversion_command(&conversion_options) {
                Ok(command) => command,
                Err(e) => {
                    return ProcessResult::Failed {
                        error: format!("Failed to build retry ffmpeg command: {e}"),
                    };
                }
            };
            let status = match run_command_isolated(&mut ffmpeg_command) {
                Ok(s) => s,
                Err(e) => {
                    return ProcessResult::Failed {
                        error: format!("Failed to execute ffmpeg (retry): {e}"),
                    };
                }
            };

            if !status.success() {
                let _ = std::fs::remove_file(output);
                let error = format!("ffmpeg failed with status: {}", status.code().unwrap_or(-1));
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }
        }

        // Get output file info and validate
        let output_info = match probe_video_info(output) {
            Ok(info) => info,
            Err(e) => {
                let _ = std::fs::remove_file(output);
                let error = format!("Failed to get output info: {e}");
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }
        };

        // If output is larger than input, reconvert once with lower quality
        let output_info = if output_info.size_bytes > info.size_bytes {
            let new_quality_level = quality_level + 2;
            print_yellow!(
                "Output file ({}) is larger than input ({}), reconverting with lower quality level ({})",
                cli_tools::format_size(output_info.size_bytes),
                cli_tools::format_size(info.size_bytes),
                new_quality_level
            );
            let _ = std::fs::remove_file(output);

            conversion_options = conversion_options.with_quality_level(new_quality_level);
            ffmpeg_command = match build_conversion_command(&conversion_options) {
                Ok(command) => command,
                Err(e) => {
                    let error = format!("Failed to build reconvert ffmpeg command: {e}");
                    self.log_failure(input, "convert", file_index, &error);
                    return ProcessResult::Failed { error };
                }
            };
            let status = match run_command_isolated(&mut ffmpeg_command) {
                Ok(s) => s,
                Err(e) => {
                    let error = format!("Failed to execute ffmpeg (reconvert): {e}");
                    self.log_failure(input, "convert", file_index, &error);
                    return ProcessResult::Failed { error };
                }
            };

            if !status.success() {
                let _ = std::fs::remove_file(output);
                let error = format!(
                    "ffmpeg reconversion failed with status: {}",
                    status.code().unwrap_or(-1)
                );
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }

            match probe_video_info(output) {
                Ok(info) => info,
                Err(e) => {
                    let _ = std::fs::remove_file(output);
                    let error = format!("Failed to get reconverted video info: {e}");
                    self.log_failure(input, "convert", file_index, &error);
                    return ProcessResult::Failed { error };
                }
            }
        } else {
            output_info
        };

        // Validate output duration
        if output_info.duration < info.duration * MIN_DURATION_RATIO {
            if let Err(e) = self.delete_file(output) {
                print_error!("Failed to delete output file: {e}");
            }
            let error = format!(
                "Output duration {:.1}s is less than {:.0}% of original {:.1}s",
                output_info.duration,
                MIN_DURATION_RATIO * 100.0,
                info.duration
            );
            self.log_failure(input, "convert", file_index, &error);
            return ProcessResult::Failed { error };
        }

        if let Err(e) = self.delete_file(input) {
            print_error!("Failed to delete original file: {e}");
        }
        self.delete_subtitle_files(&file.subtitle_files);

        let conversion_stats = ConversionStats::new(
            info.size_bytes,
            info.bitrate_kbps,
            output_info.size_bytes,
            output_info.bitrate_kbps,
        );

        let duration = start.elapsed();

        println!(
            "{}",
            format!(
                "✓ Converted in {}: {conversion_stats}",
                cli_tools::format_duration(duration)
            )
            .cyan()
        );

        self.log_success(output, "convert", file_index, duration, Some(&conversion_stats));

        ProcessResult::Converted {
            stats: conversion_stats,
        }
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
                    let result = Self::probe_and_classify(file, subtitles, &filter, movie_mode);
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
        Self::sort_processable_files(&mut conversions, self.config.sort);
        Self::sort_processable_files(&mut remuxes, self.config.sort);
        Self::sort_processable_files(&mut subtitle_muxes, self.config.sort);

        self.log_analysis_stats(&analysis_stats, total_files, start.elapsed());
        analysis_stats.print_summary();

        AnalysisOutput {
            conversions,
            remuxes,
            renames,
            subtitle_muxes,
        }
    }

    /// Create a processable file using the converter's output path rules.
    fn processable_file(&self, file: VideoFile, info: VideoInfo, subtitle_files: Vec<SubtitleFile>) -> ProcessableFile {
        let output_path = file.get_output_path_for_mode_and_bit_depth(
            info.codec_suffix(),
            self.config.movie_mode,
            !subtitle_files.is_empty(),
            info.bit_depth,
        );
        ProcessableFile::new(file, info, output_path, subtitle_files)
    }

    /// Sort processable files according to the specified sort order.
    #[inline]
    fn sort_processable_files(files: &mut [ProcessableFile], sort_order: SortOrder) {
        match sort_order {
            SortOrder::Bitrate => {
                files.sort_unstable_by_key(|file| std::cmp::Reverse(file.info.bitrate_kbps));
            }
            SortOrder::Size => {
                files.sort_unstable_by_key(|file| std::cmp::Reverse(file.info.size_bytes));
            }
            SortOrder::SizeAsc => {
                files.sort_unstable_by_key(|file| file.info.size_bytes);
            }
            SortOrder::Duration => {
                files.sort_unstable_by(|a, b| {
                    b.info
                        .duration
                        .partial_cmp(&a.info.duration)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortOrder::DurationAsc => {
                files.sort_unstable_by(|a, b| {
                    a.info
                        .duration
                        .partial_cmp(&b.info.duration)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortOrder::Resolution => {
                // Sort by total pixels (width * height) descending
                files.sort_unstable_by(|a, b| {
                    let pixels_a = u64::from(a.info.width) * u64::from(a.info.height);
                    let pixels_b = u64::from(b.info.width) * u64::from(b.info.height);
                    pixels_b.cmp(&pixels_a)
                });
            }
            SortOrder::ResolutionAsc => {
                // Sort by total pixels (width * height) ascending
                files.sort_unstable_by(|a, b| {
                    let pixels_a = u64::from(a.info.width) * u64::from(a.info.height);
                    let pixels_b = u64::from(b.info.width) * u64::from(b.info.height);
                    pixels_a.cmp(&pixels_b)
                });
            }
            SortOrder::Impact => {
                // Sort by potential savings (bitrate / fps * duration) descending
                files.sort_unstable_by(|a, b| {
                    let impact_a = (a.info.bitrate_kbps as f64 / a.info.frames_per_second) * a.info.duration;
                    let impact_b = (b.info.bitrate_kbps as f64 / b.info.frames_per_second) * b.info.duration;
                    impact_b.partial_cmp(&impact_a).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortOrder::Name => {
                files.sort_unstable_by(|a, b| a.file.path.cmp(&b.file.path));
            }
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

    /// Check if a file should be converted based on extension and include/exclude patterns.
    fn should_include_file(&self, file: &VideoFile) -> bool {
        // In normal mode, skip files already marked with a target codec suffix.
        // Movie mode still analyzes them because loose subtitle sidecars may need muxing.
        if !self.config.movie_mode
            && (RE_X265.is_match(&file.name) || RE_AV1.is_match(&file.name))
            && file.extension == file.target_extension(false, false)
        {
            return false;
        }

        // Check file extension is one of the allowed extensions
        if !self.config.extensions.iter().any(|ext| ext == &file.extension) {
            return false;
        }

        // Check exclude patterns (filename must not match any)
        if self.config.exclude.iter().any(|pattern| file.name.contains(pattern)) {
            return false;
        }

        // Check include patterns (if specified, filename must match at least one)
        if !self.config.include.is_empty() {
            let matches_include = self.config.include.iter().any(|pattern| file.name.contains(pattern));
            if !matches_include {
                return false;
            }
        }

        true
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
        let backup = Self::backup_output_path(input);
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

    fn backup_output_path(output: &Path) -> PathBuf {
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        let stem = cli_tools::path_to_file_stem_string(output);
        let extension = cli_tools::path_to_file_extension_string(output);
        let backup_stem = format!("{stem}.vconvert-backup");
        let backup_filename = format!("{backup_stem}.{extension}");
        cli_tools::get_unique_path(parent, &backup_filename, &backup_stem, &extension)
    }

    fn temporary_output_path(output: &Path) -> PathBuf {
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        let stem = cli_tools::path_to_file_stem_string(output);
        let extension = cli_tools::path_to_file_extension_string(output);
        let temporary_stem = format!("{stem}.vconvert-tmp");
        let temporary_filename = format!("{temporary_stem}.{extension}");
        cli_tools::get_unique_path(parent, &temporary_filename, &temporary_stem, &extension)
    }

    fn print_subtitle_files(subtitle_files: &[SubtitleFile]) {
        if subtitle_files.is_empty() {
            return;
        }
        println!("Subtitles:");
        for subtitle_file in subtitle_files {
            println!("  - {}", cli_tools::path_to_string_relative(&subtitle_file.path));
        }
    }

    /// Run ffprobe on a file and classify the result.
    ///
    /// Returns a `VideoInfoCache` containing the analysis result, the file path, and the
    /// `VideoInfo` to write back to the scan cache (`None` only when ffprobe failed).
    fn probe_and_classify(
        file: VideoFile,
        subtitle_files: Vec<SubtitleFile>,
        filter: &AnalysisFilter,
        movie_mode: bool,
    ) -> VideoInfoCache {
        let path = file.path.clone();
        if !path.exists() {
            return VideoInfoCache {
                result: AnalysisResult::Skip {
                    file,
                    reason: SkipReason::FileMissing,
                },
                path,
                info: None,
            };
        }
        match probe_video_info(&file.path) {
            Ok(info) => {
                let result = ClassificationRequest::new(filter, &info, movie_mode, subtitle_files).classify(file);
                VideoInfoCache {
                    result,
                    path,
                    info: Some(info),
                }
            }
            Err(error) => VideoInfoCache {
                result: AnalysisResult::Skip {
                    file,
                    reason: SkipReason::AnalysisFailed {
                        error: error.to_string(),
                    },
                },
                path,
                info: None,
            },
        }
    }
}
