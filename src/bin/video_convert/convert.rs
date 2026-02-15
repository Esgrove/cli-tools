use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cli_tools::{print_error, print_yellow};
use colored::Colorize;
use indicatif::ParallelProgressIterator;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use walkdir::WalkDir;

use crate::cli::{DatabaseMode, clear_database, list_extensions, show_database_contents};
use crate::config::{Config, VideoConvertConfig};
use crate::database::{Database, PendingAction};
use crate::logger::FileLogger;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};
use crate::types::{
    AnalysisFilter, AnalysisOutput, AnalysisResult, Codec, ProcessResult, ProcessableFile, SkipReason, VideoFile,
    VideoInfo,
};
use crate::{SortOrder, VideoConvertArgs};

pub const TARGET_EXTENSION: &str = "mp4";

/// Regex to match x265 codec identifier in filenames (case-insensitive, word boundary).
pub static RE_X265: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bx265\b").expect("Invalid x265 regex"));

/// Regex to match AV1 codec identifier in filenames (case-insensitive, word boundary).
pub static RE_AV1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bav1\b").expect("Invalid av1 regex"));

const FFMPEG_DEFAULT_ARGS: &[&str] = &["-hide_banner", "-nostdin", "-stats", "-loglevel", "info", "-y"];
const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Minimum ratio of output duration to input duration for a conversion to be considered successful.
const MIN_DURATION_RATIO: f64 = 0.85;

/// Windows API constant for creating a new process group.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

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
                DatabaseMode::Process => self.run_from_database(),
            };
        }

        // Open the database for tracking
        let database = Database::open_default()?;
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
        let mut aborted = false;
        let mut processed_count: usize = 0;

        // Gather candidate files
        let candidate_files = self.gather_files_to_process()?;
        if candidate_files.is_empty() {
            println!("No video files found");
            return Ok(());
        }
        if self.config.verbose {
            println!("Found {} candidate file(s), analyzing...", candidate_files.len());
        }

        // Analyze files to determine required actions
        let mut analysis_output = self.analyze_files(candidate_files);

        // Process renames: these files are already in a target codec but missing their codec suffix label
        if !analysis_output.renames.is_empty() {
            stats.files_renamed = self.process_renames(&analysis_output.renames);
        }

        // Update the database with files that need processing
        let mut db_added = 0;
        for file in &analysis_output.remuxes {
            if database
                .upsert_pending_file(&file.file.path, &file.file.extension, &file.info, PendingAction::Remux)
                .is_ok()
            {
                db_added += 1;
            }
        }
        for file in &analysis_output.conversions {
            if database
                .upsert_pending_file(
                    &file.file.path,
                    &file.file.extension,
                    &file.info,
                    PendingAction::Convert,
                )
                .is_ok()
            {
                db_added += 1;
            }
        }
        if self.config.verbose && db_added > 0 {
            println!("Updated {db_added} files in database");
        }

        // Calculate total files to process and truncate lists to respect config limit
        let remux_count = if self.config.skip_remux {
            0
        } else {
            analysis_output.remuxes.len()
        };
        let convert_count = if self.config.skip_convert {
            0
        } else {
            analysis_output.conversions.len()
        };
        let total_available = remux_count + convert_count;
        let total_limit = self.config.count.map_or(total_available, |c| total_available.min(c));

        // Truncate lists if they exceed the limit
        if let Some(count) = self.config.count
            && total_available > count
        {
            let remux_limit = remux_count.min(count);
            analysis_output.remuxes.truncate(remux_limit);
            let remaining = count.saturating_sub(remux_limit);
            analysis_output.conversions.truncate(remaining);
        }

        // Process remuxes
        if !self.config.skip_remux && !analysis_output.remuxes.is_empty() {
            let (remux_stats, was_aborted) = self.process_files_with_db_cleanup(
                analysis_output.remuxes,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::remux_to_mp4,
            );
            stats += remux_stats;
            aborted = was_aborted;
        }

        // Process conversions
        if !self.config.skip_convert && !analysis_output.conversions.is_empty() && !aborted {
            let (convert_stats, was_aborted) = self.process_files_with_db_cleanup(
                analysis_output.conversions,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::convert_to_hevc,
            );
            stats += convert_stats;
            aborted = was_aborted;
        }

        self.log_stats(&stats);

        if aborted {
            println!("\n{}", "Aborted by user".bold().red());
        }

        stats.print_summary();

        Ok(())
    }

    /// Gather video files based on the config settings.
    fn gather_files_to_process(&self) -> Result<Vec<VideoFile>> {
        let start = Instant::now();
        let path = &self.config.path;

        if path.is_file() {
            let file = VideoFile::new(path);
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

    /// Process files and remove them from the database after successful processing.
    fn process_files_with_db_cleanup<F>(
        &self,
        files: Vec<ProcessableFile>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
        total_limit: usize,
        database: &Database,
        process_fn: F,
    ) -> (RunStats, bool)
    where
        F: Fn(&Self, &ProcessableFile, &str) -> ProcessResult,
    {
        let mut stats = RunStats::default();
        let num_digits = total_limit.checked_ilog10().map_or(1, |d| d as usize + 1);
        let mut aborted = false;

        for file in files {
            // Check abort flag before starting a new file
            if abort_flag.load(Ordering::SeqCst) {
                aborted = true;
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

            let file_index = format!("[{:>width$}/{total_limit}]", *processed_count + 1, width = num_digits);

            let start = Instant::now();
            let result = process_fn(self, &file, &file_index);
            let duration = start.elapsed();

            match &result {
                ProcessResult::Failed { error } => {
                    print_error!("{}: {error}", cli_tools::path_to_string_relative(&file.file.path));
                }
                ProcessResult::Converted { .. } | ProcessResult::Remuxed {} => {
                    *processed_count += 1;
                    // Remove from database after successful processing
                    let _ = database.remove_pending_file(&file.file.path);
                }
            }

            stats.add_result(&result, duration);
        }

        (stats, aborted)
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
            println!("Processing {} pending file(s) from database", pending_files.len());
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
        let mut aborted = false;
        let mut processed_count: usize = 0;

        // Separate files by action type
        let (remuxes, conversions): (Vec<_>, Vec<_>) = pending_files
            .into_iter()
            .partition(|f| f.action == PendingAction::Remux);

        // Calculate limits
        let remux_count = if self.config.skip_remux { 0 } else { remuxes.len() };
        let convert_count = if self.config.skip_convert { 0 } else { conversions.len() };
        let total_available = remux_count + convert_count;
        let total_limit = self.config.count.map_or(total_available, |c| total_available.min(c));

        // Convert to processable files
        let mut remux_files: Vec<ProcessableFile> = remuxes
            .into_iter()
            .filter(|f| f.full_path.exists())
            .map(|f| {
                let video_file = VideoFile::new(&f.full_path);
                let info = f.to_video_info();
                ProcessableFile::new(video_file, info)
            })
            .collect();

        let mut conversion_files: Vec<ProcessableFile> = conversions
            .into_iter()
            .filter(|f| f.full_path.exists())
            .map(|f| {
                let video_file = VideoFile::new(&f.full_path);
                let info = f.to_video_info();
                ProcessableFile::new(video_file, info)
            })
            .collect();

        // Sort files
        Self::sort_processable_files(&mut remux_files, self.config.sort);
        Self::sort_processable_files(&mut conversion_files, self.config.sort);

        // Truncate lists if they exceed the limit
        if let Some(count) = self.config.count
            && total_available > count
        {
            let remux_limit = remux_files.len().min(count);
            remux_files.truncate(remux_limit);
            let remaining = count.saturating_sub(remux_limit);
            conversion_files.truncate(remaining);
        }

        // Process remuxes
        if !self.config.skip_remux && !remux_files.is_empty() {
            let (remux_stats, was_aborted) = self.process_files_with_db_cleanup(
                remux_files,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::remux_to_mp4,
            );
            stats += remux_stats;
            aborted = was_aborted;
        }

        // Process conversions
        if !self.config.skip_convert && !conversion_files.is_empty() && !aborted {
            let (convert_stats, was_aborted) = self.process_files_with_db_cleanup(
                conversion_files,
                &abort_flag,
                &mut processed_count,
                total_limit,
                &database,
                Self::convert_to_hevc,
            );
            stats += convert_stats;
            aborted = was_aborted;
        }

        self.log_stats(&stats);

        if aborted {
            println!("\n{}", "Aborted by user".bold().red());
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

    /// Get video information using ffprobe.
    fn get_video_info(path: &Path) -> Result<VideoInfo> {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=codec_name,bit_rate,width,height,r_frame_rate:stream_tags=BPS,BPS-eng:format=bit_rate,size,duration",
                "-output_format",
                "default=nokey=0:noprint_wrappers=1",
            ])
            .arg(path)
            .output()
            .context("Failed to execute ffprobe")?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            anyhow::bail!("ffprobe failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        VideoInfo::from_ffprobe_output(&stdout, &stderr, path)
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

        let mut cmd = Self::build_remux_command(input, output, false, codec);

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::Remuxed {};
        }

        let status = match Self::run_command_isolated(&mut cmd) {
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

        let mut cmd = Self::build_remux_command(input, output, true, codec);

        let status = match Self::run_command_isolated(&mut cmd) {
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

        let quality_level = info.quality_level();

        if self.config.verbose {
            println!("Output: {}", cli_tools::path_to_string_relative(output));
            println!("Using quality level: {quality_level}");
        }

        self.log_start(input, "convert", file_index, info, Some(quality_level));
        let start = Instant::now();

        // Determine audio codec: copy for mp4/mkv, transcode for others
        let copy_audio = extension == "mp4" || extension == "mkv";

        // Track whether CUDA filters worked for potential reconversion
        let mut use_cuda_filters = true;

        let mut ffmpeg_command = Self::build_ffmpeg_command(input, output, quality_level, copy_audio, true);

        if self.config.dryrun {
            println!("[DRYRUN] {ffmpeg_command:#?}");
            return ProcessResult::converted(info.size_bytes, info.bitrate_kbps, 0, 0);
        }

        // First attempt: try with CUDA filters for better performance
        let status = match Self::run_command_isolated(&mut ffmpeg_command) {
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
            use_cuda_filters = false;
            ffmpeg_command = Self::build_ffmpeg_command(input, output, quality_level, copy_audio, false);
            let status = match Self::run_command_isolated(&mut ffmpeg_command) {
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
        let output_info = match Self::get_video_info(output) {
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

            ffmpeg_command = Self::build_ffmpeg_command(input, output, new_quality_level, copy_audio, use_cuda_filters);
            let status = match Self::run_command_isolated(&mut ffmpeg_command) {
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

            match Self::get_video_info(output) {
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
    fn analyze_files(&self, files: Vec<VideoFile>) -> AnalysisOutput {
        let start = Instant::now();
        let total_files = files.len();
        let progress_bar = ProgressBar::new(total_files as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );

        // Extract config values needed for analysis to avoid borrowing self in parallel context
        let filter = AnalysisFilter {
            min_bitrate: self.config.bitrate_limit,
            max_bitrate: self.config.max_bitrate,
            min_duration: self.config.min_duration,
            max_duration: self.config.max_duration,
            overwrite: self.config.overwrite,
        };

        let results: Vec<AnalysisResult> = files
            .into_par_iter()
            .progress_with(progress_bar)
            .map(|file| Self::analyze_video_file(file, &filter))
            .collect();

        // Collect files into separate vectors
        let mut conversions = Vec::new();
        let mut remuxes = Vec::new();
        let mut renames: Vec<ProcessableFile> = Vec::new();
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
                        SkipReason::OutputExists {
                            path: target_path,
                            source_duration,
                        } => {
                            if self.config.delete_duplicates {
                                // Check target duration and delete source if within 10%
                                match Self::get_video_info(target_path) {
                                    Ok(target_info) => {
                                        let duration_ratio = if *source_duration > 0.0 {
                                            (target_info.duration - source_duration).abs() / source_duration
                                        } else {
                                            1.0
                                        };
                                        if duration_ratio <= 0.1 {
                                            // Duration within 10%, safe to delete source
                                            if self.config.dryrun {
                                                println!(
                                                    "{} (duration match: {:.1}s vs {:.1}s)",
                                                    format!(
                                                        "Would delete duplicate: {}",
                                                        cli_tools::path_to_string_relative(&file.path)
                                                    )
                                                    .yellow(),
                                                    source_duration,
                                                    target_info.duration
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
                                                    "{} (duration match: {:.1}s vs {:.1}s)",
                                                    format!(
                                                        "Deleted duplicate: {}",
                                                        cli_tools::path_to_string_relative(&file.path)
                                                    )
                                                    .green(),
                                                    source_duration,
                                                    target_info.duration
                                                );
                                                analysis_stats.duplicates_deleted += 1;
                                            }
                                        } else {
                                            // Duration mismatch, log error
                                            print_error!(
                                                "Duration mismatch for duplicate - source: {:.1}s, target: {:.1}s ({:.1}% difference)\n  Source: {}\n  Target: {}",
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
                        SkipReason::AnalysisFailed { error } => {
                            analysis_stats.analysis_failed += 1;
                            print_error!("{}: {error}", cli_tools::path_to_string_relative(&file.path));
                        }
                    }
                    // Print skipped files (except OutputExists which is handled above, and AnalysisFailed)
                    if self.config.verbose
                        && !matches!(reason, SkipReason::AnalysisFailed { .. })
                        && !matches!(reason, SkipReason::OutputExists { .. })
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

        self.log_analysis_stats(&analysis_stats, total_files, start.elapsed());
        analysis_stats.print_summary();

        AnalysisOutput {
            conversions,
            remuxes,
            renames,
        }
    }

    /// Sort processable files according to the specified sort order.
    #[inline]
    fn sort_processable_files(files: &mut [ProcessableFile], sort_order: SortOrder) {
        match sort_order {
            SortOrder::Bitrate => {
                files.sort_unstable_by(|a, b| b.info.bitrate_kbps.cmp(&a.info.bitrate_kbps));
            }
            SortOrder::Size => {
                files.sort_unstable_by(|a, b| b.info.size_bytes.cmp(&a.info.size_bytes));
            }
            SortOrder::SizeAsc => {
                files.sort_unstable_by(|a, b| a.info.size_bytes.cmp(&b.info.size_bytes));
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

    /// Build the ffmpeg command for HEVC conversion.
    /// When `use_cuda_filters` is true, uses `hwupload_cuda` and `scale_cuda` for GPU-accelerated filtering.
    /// When false, uses CPU-based filtering which is more compatible but slightly slower.
    fn build_ffmpeg_command(
        input: &Path,
        output: &Path,
        quality_level: u8,
        copy_audio: bool,
        use_cuda_filters: bool,
    ) -> Command {
        // GPU tuning for RTX 4090 to use more VRAM and improve performance
        let extra_hw_frames = "64";
        let lookahead = "48";
        let preset = "p5"; // slow (good quality)

        let mut cmd = Command::new("ffmpeg");
        cmd.args(FFMPEG_DEFAULT_ARGS)
            .args(["-probesize", "50M", "-analyzeduration", "1M"]);

        if use_cuda_filters {
            cmd.args(["-extra_hw_frames", extra_hw_frames]);
        }

        cmd.arg("-i").arg(input);

        if use_cuda_filters {
            cmd.args(["-vf", "hwupload_cuda,scale_cuda=format=nv12"]);
        }

        cmd.args(["-c:v", "hevc_nvenc"])
            .args(["-rc:v", "vbr"])
            .args(["-cq:v", &quality_level.to_string()])
            .args(["-preset", preset])
            .args(["-b:v", "0"])
            .args(["-rc-lookahead", lookahead])
            .args(["-spatial_aq", "1", "-temporal_aq", "1"])
            .args(["-tag:v", "hvc1"]);

        if copy_audio {
            cmd.args(["-c:a", "copy"]);
        } else {
            cmd.args(["-c:a", "aac", "-b:a", "128k"]);
        }

        cmd.arg(output);
        cmd
    }

    /// Check if a file should be converted based on extension and include/exclude patterns.
    fn should_include_file(&self, file: &VideoFile) -> bool {
        // Skip files with "x265" or "av1" in the filename (already converted)
        if (RE_X265.is_match(&file.name) || RE_AV1.is_match(&file.name)) && file.extension == TARGET_EXTENSION {
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

    /// Analyze a single file to determine what action to take.
    /// This is a standalone function to allow parallel execution without borrowing `VideoConvert`.
    #[allow(clippy::too_many_lines)]
    fn analyze_video_file(file: VideoFile, filter: &AnalysisFilter) -> AnalysisResult {
        // Get video info using ffprobe
        let info = match Self::get_video_info(&file.path) {
            Ok(info) => info,
            Err(e) => {
                return AnalysisResult::Skip {
                    file,
                    reason: SkipReason::AnalysisFailed { error: e.to_string() },
                };
            }
        };

        let is_target = info.is_target_codec();
        let suffix = info.codec_suffix();

        // Check if already converted (target codec in MP4 with matching suffix marker)
        if is_target && file.extension == TARGET_EXTENSION {
            if !suffix.regex().is_match(&file.name) {
                // Needs rename to add codec suffix
                let output_path = file.get_output_path(suffix);
                if output_path.exists() && !filter.overwrite {
                    return AnalysisResult::Skip {
                        file,
                        reason: SkipReason::OutputExists {
                            path: output_path,
                            source_duration: info.duration,
                        },
                    };
                }
                return AnalysisResult::NeedsRename(ProcessableFile::new(file, info));
            }
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::AlreadyConverted,
            };
        }

        // Check minimum bitrate threshold
        if info.bitrate_kbps < filter.min_bitrate {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::BitrateBelowThreshold {
                    bitrate: info.bitrate_kbps,
                    threshold: filter.min_bitrate,
                },
            };
        }

        // Check maximum bitrate threshold
        if let Some(max_bitrate) = filter.max_bitrate
            && info.bitrate_kbps > max_bitrate
        {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::BitrateAboveThreshold {
                    bitrate: info.bitrate_kbps,
                    threshold: max_bitrate,
                },
            };
        }

        // Check minimum duration threshold
        if let Some(min_duration) = filter.min_duration
            && info.duration < min_duration
        {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::DurationBelowThreshold {
                    duration: info.duration,
                    threshold: min_duration,
                },
            };
        }

        // Check maximum duration threshold
        if let Some(max_duration) = filter.max_duration
            && info.duration > max_duration
        {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::DurationAboveThreshold {
                    duration: info.duration,
                    threshold: max_duration,
                },
            };
        }

        let output_path = file.get_output_path(suffix);

        // Check if output already exists
        if output_path.exists() && !filter.overwrite {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::OutputExists {
                    path: output_path,
                    source_duration: info.duration,
                },
            };
        }

        if is_target {
            AnalysisResult::NeedsRemux(ProcessableFile::new(file, info))
        } else {
            AnalysisResult::NeedsConversion(ProcessableFile::new(file, info))
        }
    }

    /// Build ffmpeg command for remuxing with stream copy.
    fn build_remux_command(input: &Path, output: &Path, transcode_audio: bool, codec: Codec) -> Command {
        // -map 0:v:0   -> first video stream only
        // -map 0:a?    -> all audio streams (optional, if any)
        // -map -0:t    -> drop attachments
        // -map -0:d    -> drop data streams
        // -sn          -> drop subtitles (avoids failures with non-mov_text subs)
        let mut cmd = Command::new("ffmpeg");
        cmd.args(FFMPEG_DEFAULT_ARGS).arg("-i").arg(input).args([
            "-map", "0:v:0", "-map", "0:a?", "-map", "-0:t", "-map", "-0:d", "-sn", "-c:v", "copy",
        ]);

        if transcode_audio {
            cmd.args(["-c:a", "aac", "-b:a", "128k"]);
        } else {
            cmd.args(["-c:a", "copy"]);
        }

        cmd.args(["-movflags", "+faststart"]);
        if codec == Codec::X265 {
            cmd.args(["-tag:v", "hvc1"]);
        }
        cmd.arg(output);
        cmd
    }

    /// Run a command in a new process group to prevent Ctrl+C from propagating to it.
    /// This allows the main program to handle the signal and finish the current file gracefully.
    fn run_command_isolated(cmd: &mut Command) -> std::io::Result<ExitStatus> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        #[cfg(unix)]
        {
            // Set process group to 0 to prevent SIGINT propagation
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit()).status()
    }
}
