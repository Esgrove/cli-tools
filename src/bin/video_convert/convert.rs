use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cli_tools::{print_error, print_yellow};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use walkdir::WalkDir;

use crate::cli::{DatabaseMode, clean_scan_cache, clear_database, list_extensions, show_database_contents};
use crate::config::{Config, VideoConvertConfig};
use crate::database::{Database, PendingAction};
use crate::logger::FileLogger;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};
use crate::types::{
    AnalysisFilter, AnalysisOutput, AnalysisResult, Codec, ProcessResult, ProcessableFile, SkipReason, VideoFile,
    VideoInfo, VideoInfoCache,
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
        let mut analysis_output = self.analyze_files(candidate_files, &mut database);

        // Process renames: these files are already in a target codec but missing their codec suffix label
        if !analysis_output.renames.is_empty() {
            stats.files_renamed = self.process_renames(&analysis_output.renames);
        }

        // Update the database with files that need processing (single transaction)
        let mut pending_entries: Vec<(&Path, &str, &VideoInfo, PendingAction)> = Vec::new();
        for file in &analysis_output.remuxes {
            pending_entries.push((&file.file.path, &file.file.extension, &file.info, PendingAction::Remux));
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
                let video_file = VideoFile::new_with_metadata(&f.full_path);
                let info = f.to_video_info();
                ProcessableFile::new(video_file, info)
            })
            .collect();

        let mut conversion_files: Vec<ProcessableFile> = conversions
            .into_iter()
            .filter(|f| f.full_path.exists())
            .map(|f| {
                let video_file = VideoFile::new_with_metadata(&f.full_path);
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
    fn analyze_files(&self, files: Vec<VideoFile>, database: &mut Database) -> AnalysisOutput {
        let start = Instant::now();
        let total_files = files.len();

        // Extract config values needed for analysis to avoid borrowing self in parallel context
        let filter = AnalysisFilter {
            min_bitrate: self.config.bitrate_limit,
            max_bitrate: self.config.max_bitrate,
            min_duration: self.config.min_duration,
            max_duration: self.config.max_duration,
            min_resolution: self.config.min_resolution,
            overwrite: self.config.overwrite,
        };

        // Phase 1 (sequential): bulk-load the scan cache into a HashMap for O(1) lookups,
        // then split files into cache hits (classified immediately) and cache misses.
        let scan_cache: HashMap<String, VideoInfo> = database.get_all_scanned_files().unwrap_or_default();
        let mut cache_results: Vec<AnalysisResult> = Vec::new();
        let mut cache_misses: Vec<VideoFile> = Vec::new();

        for file in files {
            let path_key = file.path.to_string_lossy();
            if let Some(cached_info) = scan_cache.get(path_key.as_ref())
                && cached_info.size_bytes == file.size_bytes
            {
                cache_results.push(Self::classify_video_file(file, &filter, cached_info));
            } else {
                cache_misses.push(file);
            }
        }

        let cache_hit_count = cache_results.len();
        let cache_miss_count = cache_misses.len();
        if self.config.verbose && cache_hit_count > 0 {
            println!(
                "Scan cache: {cache_hit_count} hit(s), {cache_miss_count} miss(es) — running ffprobe on {cache_miss_count} file(s)"
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
                .map(|file| {
                    let result = Self::probe_and_classify(file, &filter);
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
                        SkipReason::ResolutionBelowLimit { .. } => {
                            analysis_stats.skipped_resolution_low += 1;
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

    /// Run ffprobe on a file and classify the result.
    ///
    /// Returns a `VideoInfoCache` containing the analysis result, the file path, and the
    /// `VideoInfo` to write back to the scan cache (`None` only when ffprobe failed).
    fn probe_and_classify(file: VideoFile, filter: &AnalysisFilter) -> VideoInfoCache {
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
        match Self::get_video_info(&file.path) {
            Ok(info) => {
                let result = Self::classify_video_file(file, filter, &info);
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

    /// Classify a video file given its already-obtained `VideoInfo`.
    ///
    /// Takes `info` by reference — only clones into `ProcessableFile` on the minority
    /// paths that actually need processing (conversion, remux, rename).
    fn classify_video_file(file: VideoFile, filter: &AnalysisFilter, info: &VideoInfo) -> AnalysisResult {
        let is_target_codec = info.is_target_codec();
        let suffix = info.codec_suffix();

        // Check if already converted (target codec in MP4 with matching suffix marker)
        if is_target_codec && file.extension == TARGET_EXTENSION {
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
                return AnalysisResult::NeedsRename(ProcessableFile::new(file, info.clone()));
            }
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::AlreadyConverted,
            };
        }

        // Bitrate and duration limits only apply to conversions, not remuxes
        if !is_target_codec {
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

            // Check minimum resolution threshold
            if let Some(min_resolution) = filter.min_resolution {
                let smaller_dimension = info.width.min(info.height);
                if smaller_dimension < min_resolution {
                    return AnalysisResult::Skip {
                        file,
                        reason: SkipReason::ResolutionBelowLimit {
                            width: info.width,
                            height: info.height,
                            limit: min_resolution,
                        },
                    };
                }
            }
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

        if is_target_codec {
            AnalysisResult::NeedsRemux(ProcessableFile::new(file, info.clone()))
        } else {
            AnalysisResult::NeedsConversion(ProcessableFile::new(file, info.clone()))
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

#[cfg(test)]
mod test_classify_already_converted {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    fn hevc_info() -> VideoInfo {
        VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        }
    }

    #[test]
    fn hevc_mp4_with_x265_suffix_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mp4"), 0);
        let info = hevc_info();
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }

    #[test]
    fn hevc_mp4_with_x265_suffix_uppercase_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.X265.mp4"), 0);
        let info = hevc_info();
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }

    #[test]
    fn av1_mp4_with_av1_suffix_is_already_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.av1.mp4"), 0);
        let info = VideoInfo {
            codec: "av1".to_string(),
            bitrate_kbps: 3000,
            size_bytes: 300_000_000,
            duration: 1800.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::AlreadyConverted,
                    ..
                }
            ),
            "Expected AlreadyConverted skip, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_needs_rename {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    #[test]
    fn hevc_mp4_without_suffix_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRename(..)),
            "Expected NeedsRename, got: {result:?}"
        );
    }

    #[test]
    fn av1_mp4_without_suffix_needs_rename() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let info = VideoInfo {
            codec: "av1".to_string(),
            bitrate_kbps: 3000,
            size_bytes: 300_000_000,
            duration: 1800.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRename(..)),
            "Expected NeedsRename, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_needs_conversion {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    fn h264_info() -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        }
    }

    #[test]
    fn h264_mkv_needs_conversion() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion, got: {result:?}"
        );
    }

    #[test]
    fn h264_mp4_needs_conversion() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let info = h264_info();
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion, got: {result:?}"
        );
    }

    #[test]
    fn mpeg4_avi_needs_conversion() {
        let file = VideoFile::new(Path::new("/videos/movie.avi"), 0);
        let info = VideoInfo {
            codec: "mpeg4".to_string(),
            bitrate_kbps: 15000,
            size_bytes: 2_000_000_000,
            duration: 7200.0,
            width: 1280,
            height: 720,
            frames_per_second: 30.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_needs_remux {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, VideoFile, VideoInfo};

    fn default_filter() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    #[test]
    fn hevc_mkv_needs_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux, got: {result:?}"
        );
    }

    #[test]
    fn av1_mkv_needs_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "av1".to_string(),
            bitrate_kbps: 3000,
            size_bytes: 300_000_000,
            duration: 1800.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux, got: {result:?}"
        );
    }

    #[test]
    fn hevc_avi_needs_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.avi"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 800_000_000,
            duration: 5400.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = default_filter();

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_bitrate_filtering {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    fn h264_info_with_bitrate(bitrate_kbps: u64) -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        }
    }

    #[test]
    fn below_min_bitrate_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(5000);
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::BitrateBelowThreshold {
                        bitrate: 5000,
                        threshold: 8000
                    },
                    ..
                }
            ),
            "Expected BitrateBelowThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn above_max_bitrate_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(60000);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: Some(50000),
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::BitrateAboveThreshold {
                        bitrate: 60000,
                        threshold: 50000
                    },
                    ..
                }
            ),
            "Expected BitrateAboveThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn bitrate_at_min_threshold_is_not_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(8000);
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion at exact min threshold, got: {result:?}"
        );
    }

    #[test]
    fn bitrate_at_max_threshold_is_not_skipped() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = h264_info_with_bitrate(50000);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: Some(50000),
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion at exact max threshold, got: {result:?}"
        );
    }

    #[test]
    fn bitrate_filter_does_not_apply_to_remux() {
        // hevc in mkv needs remux — bitrate limits should not block it
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 500,
            size_bytes: 100_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: Some(50000),
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux (bitrate filter should not apply to remux), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_duration_filtering {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    fn h264_info_with_duration(duration: f64) -> VideoInfo {
        VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        }
    }

    #[test]
    fn below_min_duration_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/clip.mkv"), 0);
        let info = h264_info_with_duration(30.0);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: Some(60.0),
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::DurationBelowThreshold { .. },
                    ..
                }
            ),
            "Expected DurationBelowThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn above_max_duration_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/long.mkv"), 0);
        let info = h264_info_with_duration(14400.0);
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: Some(7200.0),
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::DurationAboveThreshold { .. },
                    ..
                }
            ),
            "Expected DurationAboveThreshold skip, got: {result:?}"
        );
    }

    #[test]
    fn duration_filter_does_not_apply_to_remux() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 10.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: Some(60.0),
            max_duration: Some(7200.0),
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux (duration filter should not apply to remux), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_resolution_filtering {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    #[test]
    fn below_min_resolution_is_skipped() {
        let file = VideoFile::new(Path::new("/videos/low_res.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(720),
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::ResolutionBelowLimit {
                        width: 640,
                        height: 480,
                        limit: 720
                    },
                    ..
                }
            ),
            "Expected ResolutionBelowLimit skip, got: {result:?}"
        );
    }

    #[test]
    fn at_min_resolution_is_not_skipped() {
        let file = VideoFile::new(Path::new("/videos/hd.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1280,
            height: 720,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(720),
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion at exact min resolution, got: {result:?}"
        );
    }

    #[test]
    fn vertical_video_uses_smaller_dimension() {
        // 1080x720 vertical — smaller dimension is 720 which is below 1080 min
        let file = VideoFile::new(Path::new("/videos/vertical.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 720,
            height: 1280,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(1080),
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::ResolutionBelowLimit { .. },
                    ..
                }
            ),
            "Expected ResolutionBelowLimit for vertical video, got: {result:?}"
        );
    }

    #[test]
    fn resolution_filter_does_not_apply_to_remux() {
        let file = VideoFile::new(Path::new("/videos/small_hevc.mkv"), 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 100_000_000,
            duration: 3600.0,
            width: 640,
            height: 480,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: Some(1080),
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRemux(..)),
            "Expected NeedsRemux (resolution filter should not apply to remux), got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_output_exists {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    fn filter_no_overwrite() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        }
    }

    fn filter_with_overwrite() -> AnalysisFilter {
        AnalysisFilter {
            min_bitrate: 0,
            max_bitrate: None,
            min_duration: None,
            max_duration: None,
            min_resolution: None,
            overwrite: true,
        }
    }

    #[test]
    fn skips_when_output_exists_no_overwrite() {
        // Use a real path whose output (*.x265.mp4) exists.
        // We create a temp dir with both source and output files.
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mkv");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };

        let result = VideoConvert::classify_video_file(file, &filter_no_overwrite(), &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::OutputExists { .. },
                    ..
                }
            ),
            "Expected OutputExists skip, got: {result:?}"
        );
    }

    #[test]
    fn converts_when_output_exists_with_overwrite() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mkv");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };

        let result = VideoConvert::classify_video_file(file, &filter_with_overwrite(), &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion with overwrite, got: {result:?}"
        );
    }

    #[test]
    fn rename_skipped_when_output_exists_no_overwrite() {
        // hevc in mp4 without suffix — rename target already exists
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mp4");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };

        let result = VideoConvert::classify_video_file(file, &filter_no_overwrite(), &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::OutputExists { .. },
                    ..
                }
            ),
            "Expected OutputExists skip for rename target, got: {result:?}"
        );
    }

    #[test]
    fn rename_proceeds_when_output_exists_with_overwrite() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mp4");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };

        let result = VideoConvert::classify_video_file(file, &filter_with_overwrite(), &info);

        assert!(
            matches!(result, AnalysisResult::NeedsRename(..)),
            "Expected NeedsRename with overwrite, got: {result:?}"
        );
    }

    #[test]
    fn remux_skipped_when_output_exists_no_overwrite() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let source = temp_dir.path().join("video.mkv");
        let output = temp_dir.path().join("video.x265.mp4");
        std::fs::write(&source, "").expect("Failed to create source");
        std::fs::write(&output, "").expect("Failed to create output");

        let file = VideoFile::new(&source, 0);
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };

        let result = VideoConvert::classify_video_file(file, &filter_no_overwrite(), &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::OutputExists { .. },
                    ..
                }
            ),
            "Expected OutputExists skip for remux, got: {result:?}"
        );
    }
}

#[cfg(test)]
mod test_classify_combined_filters {
    use super::*;
    use crate::types::{AnalysisFilter, AnalysisResult, SkipReason, VideoFile, VideoInfo};

    #[test]
    fn first_failing_filter_wins_bitrate_before_duration() {
        // Both bitrate and duration fail — bitrate check comes first
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 500,
            size_bytes: 100_000_000,
            duration: 10.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: None,
            min_duration: Some(60.0),
            max_duration: None,
            min_resolution: None,
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(
                result,
                AnalysisResult::Skip {
                    reason: SkipReason::BitrateBelowThreshold { .. },
                    ..
                }
            ),
            "Expected BitrateBelowThreshold (first filter checked), got: {result:?}"
        );
    }

    #[test]
    fn passes_all_filters_gets_converted() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 10000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: Some(50000),
            min_duration: Some(60.0),
            max_duration: Some(7200.0),
            min_resolution: Some(720),
            overwrite: false,
        };

        let result = VideoConvert::classify_video_file(file, &filter, &info);

        assert!(
            matches!(result, AnalysisResult::NeedsConversion(..)),
            "Expected NeedsConversion with all filters passing, got: {result:?}"
        );
    }
}
