use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cli_tools::{print_error, print_warning};
use colored::Colorize;
use indicatif::ParallelProgressIterator;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::VideoConvertArgs;
use crate::config::{Config, VideoConvertConfig};
use crate::logger::FileLogger;
use crate::stats::{AnalysisStats, ConversionStats, RunStats};

const TARGET_EXTENSION: &str = "mp4";
const FFMPEG_DEFAULT_ARGS: &[&str] = &["-hide_banner", "-nostdin", "-stats", "-loglevel", "info", "-y"];
const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Minimum ratio of output duration to input duration for a conversion to be considered successful.
const MIN_DURATION_RATIO: f64 = 0.85;

/// Video converter that processes files to HEVC format using ffmpeg and NVENC.
pub struct VideoConvert {
    config: Config,
    logger: RefCell<FileLogger>,
}

/// A video file with its path and parsed name components.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct VideoFile {
    path: PathBuf,
    name: String,
    extension: String,
}

/// Information about a video file from ffprobe
#[derive(Debug)]
pub struct VideoInfo {
    /// Video codec name (e.g., "hevc", "h264")
    pub(crate) codec: String,
    /// Video bitrate in kbps
    pub(crate) bitrate_kbps: u64,
    /// File size in bytes
    pub(crate) size_bytes: u64,
    /// Duration in seconds
    pub(crate) duration: f64,
    /// Video width in pixels
    pub(crate) width: u32,
    /// Video height in pixels
    pub(crate) height: u32,
    /// Framerate in frames per second
    pub(crate) frames_per_second: f64,
    /// Warning message from ffprobe stderr (if any)
    pub(crate) warning: Option<String>,
}

/// Reasons why a file was skipped
#[derive(Debug)]
pub enum SkipReason {
    /// File is already HEVC in MP4 container
    AlreadyConverted,
    /// File bitrate is below the threshold
    BitrateBelowThreshold { bitrate: u64, threshold: u64 },
    /// Output file already exists
    OutputExists { path: PathBuf },
    /// Failed to get video info
    AnalysisFailed { error: String },
}

/// Result of processing a single file
#[derive(Debug)]
pub enum ProcessResult {
    /// File was converted successfully
    Converted { stats: ConversionStats },
    /// File was remuxed (already HEVC, just changed container to MP4)
    Remuxed {},
    /// Failed to process file
    Failed { error: String },
}

impl ProcessResult {
    /// A successful conversion result with size statistics.
    const fn converted(
        original_size: u64,
        original_bitrate_kbps: u64,
        converted_size: u64,
        output_bitrate_kbps: u64,
    ) -> Self {
        Self::Converted {
            stats: ConversionStats::new(
                original_size,
                original_bitrate_kbps,
                converted_size,
                output_bitrate_kbps,
            ),
        }
    }
}

impl VideoFile {
    /// Create a new `VideoFile` from a path, extracting name and extension.
    fn new(path: &Path) -> Self {
        let path = path.to_owned();
        let name = cli_tools::path_to_file_stem_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }

    /// Get the output path for the converted file.
    fn output_path(&self) -> PathBuf {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let new_name = format!("{}.x265.mp4", self.name);
        parent.join(new_name)
    }
}

impl VideoInfo {
    /// Determine quality level based on resolution and bitrate.
    /// Quality level 1 to 51, lower is better quality and bigger file size.
    fn quality_level(&self) -> u8 {
        let is_4k = self.width.max(self.height) >= 2160;
        let bitrate_mbps = self.bitrate_kbps as f64 / 1000.0;

        if is_4k {
            if bitrate_mbps > 26.0 {
                30
            } else if bitrate_mbps > 18.0 {
                31
            } else if bitrate_mbps > 10.0 {
                32
            } else {
                33
            }
        } else if bitrate_mbps > 16.0 {
            28
        } else if bitrate_mbps > 12.0 {
            29
        } else if bitrate_mbps > 6.0 {
            30
        } else {
            31
        }
    }
}

impl std::fmt::Display for VideoInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Codec:      {}", self.codec)?;
        writeln!(f, "Size:       {}", cli_tools::format_size(self.size_bytes))?;
        writeln!(
            f,
            "Bitrate:    {:.2} Mbps @ {:.0} FPS",
            self.bitrate_kbps as f64 / 1000.0,
            self.frames_per_second
        )?;
        writeln!(f, "Duration:   {}", cli_tools::format_duration_seconds(self.duration))?;
        write!(f, "Resolution: {}x{}", self.width, self.height)?;
        if let Some(warning) = &self.warning {
            write!(f, "\nWarning:    {warning}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for VideoFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyConverted => write!(f, "Already HEVC in MP4 container"),
            Self::BitrateBelowThreshold { bitrate, threshold } => {
                write!(f, "Bitrate {bitrate} kbps is below threshold {threshold} kbps")
            }
            Self::OutputExists { path } => {
                write!(f, "Output file already exists: \"{}\"", path.display())
            }
            Self::AnalysisFailed { error } => {
                write!(f, "Failed to analyze: {error}")
            }
        }
    }
}

impl From<walkdir::DirEntry> for VideoFile {
    fn from(entry: walkdir::DirEntry) -> Self {
        Self::new(entry.path())
    }
}

/// Result of analyzing a video file to determine what action to take.
#[derive(Debug)]
enum AnalysisResult {
    /// File needs to be converted to HEVC
    NeedsConversion {
        file: VideoFile,
        info: VideoInfo,
        output_path: PathBuf,
    },
    /// File is already HEVC but needs remuxing to MP4
    NeedsRemux {
        file: VideoFile,
        info: VideoInfo,
        output_path: PathBuf,
    },
    /// File should be renamed to add .x265 suffix
    NeedsRename { file: VideoFile },
    /// File should be skipped
    Skip { file: VideoFile, reason: SkipReason },
}

/// A video file with its analyzed info, ready for processing.
#[derive(Debug)]
struct ProcessableFile {
    file: VideoFile,
    info: VideoInfo,
    output_path: PathBuf,
}

/// Output from the analysis phase.
struct AnalysisOutput {
    /// Files that need full conversion (non-HEVC to HEVC).
    conversions: Vec<ProcessableFile>,
    /// Files that need remuxing (HEVC but wrong container).
    remuxes: Vec<ProcessableFile>,
    /// Files that need to be renamed (HEVC MP4 without .x265 suffix).
    renames: Vec<VideoFile>,
}

impl VideoConvert {
    /// Create a new video converter from command line arguments.
    pub fn new(args: VideoConvertArgs) -> Result<Self> {
        let user_config = VideoConvertConfig::get_user_config();
        let config = Config::try_from_args(args, user_config)?;
        let logger = RefCell::new(FileLogger::new()?);

        Ok(Self { config, logger })
    }

    /// Run the video conversion process.
    pub fn run(&self) -> Result<()> {
        self.log_init();

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
        let analysis_output = self.analyze_files(candidate_files);

        // Process renames: these files are already in HEVC format but missing "x265" label
        if !analysis_output.renames.is_empty() {
            self.process_renames(&analysis_output.renames);
        }

        // Process remuxes
        if !self.config.skip_remux && !analysis_output.remuxes.is_empty() {
            let (remux_stats, was_aborted) =
                self.process_remuxes(analysis_output.remuxes, &abort_flag, &mut processed_count);
            stats.merge(&remux_stats);
            aborted = was_aborted;
        }

        // Process conversions
        if !self.config.skip_convert && !analysis_output.conversions.is_empty() {
            let (convert_stats, was_aborted) =
                self.process_conversions(analysis_output.conversions, &abort_flag, &mut processed_count);
            stats.merge(&convert_stats);
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

        let max_depth = if self.config.recursive { usize::MAX } else { 1 };

        let mut files: Vec<VideoFile> = WalkDir::new(path)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|entry| !cli_tools::is_hidden(entry))
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(VideoFile::from)
            .filter(|file| self.should_include_file(file))
            .collect();

        files.sort_unstable();

        Ok(files)
    }

    /// Generic file processing loop.
    fn process_files<F>(
        &self,
        files: Vec<ProcessableFile>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
        process_fn: F,
    ) -> (RunStats, bool)
    where
        F: Fn(&Self, &ProcessableFile, &str) -> ProcessResult,
    {
        let mut stats = RunStats::default();
        let limit = self.config.number;
        let num_digits = limit.to_string().chars().count();
        let mut aborted = false;

        for file in files {
            // Check abort flag before starting a new file
            if abort_flag.load(Ordering::SeqCst) {
                aborted = true;
                break;
            }

            if *processed_count >= limit {
                println!("Reached file limit ({limit})");
                break;
            }

            let file_index = format!("[{:>width$}/{limit}]", *processed_count + 1, width = num_digits);

            let start = Instant::now();
            let result = process_fn(self, &file, &file_index);
            let duration = start.elapsed();

            if let ProcessResult::Failed { ref error } = result {
                print_error!("{}: {error}", cli_tools::path_to_string_relative(&file.file.path));
            } else {
                *processed_count += 1;
            }

            stats.add_result(&result, duration);
        }

        (stats, aborted)
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

        // Parse key=value pairs from output
        // Example output:
        // ```
        //  codec_name=h264
        //  bit_rate=7345573
        //  duration=2425.237007
        //  size=2292495805
        //  bit_rate=7562133
        //  r_frame_rate=30/1
        // ```
        let mut codec = String::new();
        let mut bitrate_bps: Option<u64> = None;
        let mut size_bytes: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;
        let mut frames_per_second: Option<f64> = None;

        for line in stdout.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "codec_name" => codec = value.to_lowercase(),
                    "bit_rate" | "BPS" | "BPS-eng" => {
                        if bitrate_bps.is_none()
                            && let Ok(bps) = value.parse::<u64>()
                            && bps > 0
                        {
                            bitrate_bps = Some(bps);
                        }
                    }
                    "size" => {
                        if let Ok(size) = value.parse::<u64>() {
                            size_bytes = Some(size);
                        }
                    }
                    "duration" => {
                        if let Ok(seconds) = value.parse::<f64>() {
                            duration = Some(seconds);
                        }
                    }
                    "width" => {
                        if let Ok(w) = value.parse::<u32>() {
                            width = Some(w);
                        }
                    }
                    "height" => {
                        if let Ok(h) = value.parse::<u32>() {
                            height = Some(h);
                        }
                    }
                    "r_frame_rate" => {
                        // Parse fractional framerate like "30/1" or "30000/1001"
                        if let Some((num, den)) = value.split_once('/')
                            && let (Ok(n), Ok(d)) = (num.parse::<f64>(), den.parse::<f64>())
                            && d > 0.0
                        {
                            frames_per_second = Some(n / d);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Fall back to file metadata for size if not in ffprobe output
        let size_bytes = size_bytes.unwrap_or_else(|| fs::metadata(path).map(|m| m.len()).unwrap_or(0));

        // Validate required fields
        if codec.is_empty() {
            anyhow::bail!("failed to detect video codec");
        }
        let Some(bitrate_bps) = bitrate_bps else {
            anyhow::bail!("failed to detect bitrate");
        };
        let Some(duration) = duration else {
            anyhow::bail!("failed to detect duration");
        };
        let Some(width) = width else {
            anyhow::bail!("failed to detect video width");
        };
        let Some(height) = height else {
            anyhow::bail!("failed to detect video height");
        };
        let Some(frames_per_second) = frames_per_second else {
            anyhow::bail!("failed to detect framerate");
        };

        let warning = if stderr.is_empty() {
            None
        } else {
            Some(stderr.trim().to_string())
        };

        Ok(VideoInfo {
            codec,
            bitrate_kbps: bitrate_bps / 1000,
            size_bytes,
            duration,
            width,
            height,
            frames_per_second,
            warning,
        })
    }

    /// Remux HEVC video to MP4 container
    fn remux_to_mp4(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;

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

        // Try pure copy and drop unsupported streams
        // -map 0:v:0   -> first video stream only
        // -map 0:a?    -> all audio streams (optional, if any)
        // -map -0:t    -> drop attachments
        // -map -0:d    -> drop data streams
        // -sn          -> drop subtitles (avoids failures with non-mov_text subs)
        let mut cmd = Command::new("ffmpeg");
        cmd.args(FFMPEG_DEFAULT_ARGS)
            .arg("-i")
            .arg(input)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-map",
                "-0:t",
                "-map",
                "-0:d",
                "-sn",
                "-c:v",
                "copy",
                "-c:a",
                "copy",
                "-movflags",
                "+faststart",
                "-tag:v",
                "hvc1",
            ])
            .arg(output);

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
        print_warning!("Remux failed with code {status}. Retrying with AAC audio transcode...");

        // Remove failed output file if it exists
        if output.exists() {
            let _ = fs::remove_file(output);
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(FFMPEG_DEFAULT_ARGS)
            .arg("-i")
            .arg(input)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-map",
                "-0:t",
                "-map",
                "-0:d",
                "-sn",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-movflags",
                "+faststart",
                "-tag:v",
                "hvc1",
            ])
            .arg(output);

        let status = match Self::run_command_isolated(&mut cmd) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if !status.success() {
            let _ = fs::remove_file(output);
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
    fn convert_to_hevc(&self, file: &ProcessableFile, file_index: &str) -> ProcessResult {
        let input = &file.file.path;
        let output = &file.output_path;
        let info = &file.info;
        let extension = &file.file.extension;

        println!(
            "{}",
            format!("{file_index} Converting: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .magenta()
        );
        println!("{info}");

        let quality_level = info.quality_level();

        if self.config.verbose {
            println!("Converting: {}", cli_tools::path_to_string_relative(output));
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
            let _ = fs::remove_file(output);

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
                let _ = fs::remove_file(output);
                let error = format!("ffmpeg failed with status: {}", status.code().unwrap_or(-1));
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }
        }

        // Get output file info and validate
        let output_info = match Self::get_video_info(output) {
            Ok(info) => info,
            Err(e) => {
                let error = format!("Failed to get output info: {e}");
                self.log_failure(input, "convert", file_index, &error);
                return ProcessResult::Failed { error };
            }
        };

        // If output is larger than input, reconvert once with lower quality
        let output_info = if output_info.size_bytes > info.size_bytes {
            let new_quality_level = quality_level + 2;
            print_warning!(
                "Output file ({}) is larger than input ({}), reconverting with lower quality level ({})",
                cli_tools::format_size(output_info.size_bytes),
                cli_tools::format_size(info.size_bytes),
                new_quality_level
            );
            let _ = fs::remove_file(output);

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
                let _ = fs::remove_file(output);
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
                    let _ = fs::remove_file(output);
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

        let stats = ConversionStats::new(
            info.size_bytes,
            info.bitrate_kbps,
            output_info.size_bytes,
            output_info.bitrate_kbps,
        );

        let duration = start.elapsed();

        println!(
            "{}",
            format!("✓ Converted in {}: {stats}", cli_tools::format_duration(duration)).cyan()
        );

        self.log_success(output, "convert", file_index, duration, Some(&stats));

        ProcessResult::Converted { stats }
    }

    /// Analyze files in parallel to determine which need processing.
    /// Runs ffprobe on each file concurrently and filters based on video information.
    fn analyze_files(&self, files: Vec<VideoFile>) -> AnalysisOutput {
        let progress_bar = ProgressBar::new(files.len() as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );

        // Extract config values needed for analysis to avoid borrowing self in parallel context
        let bitrate_limit = self.config.bitrate_limit;
        let overwrite = self.config.overwrite;

        let results: Vec<AnalysisResult> = files
            .into_par_iter()
            .progress_with(progress_bar)
            .map(|file| Self::analyze_video_file(file, bitrate_limit, overwrite))
            .collect();

        // Collect files into separate vectors
        let mut conversions = Vec::new();
        let mut remuxes = Vec::new();
        let mut renames = Vec::new();
        let mut analysis_stats = AnalysisStats::default();

        for result in results {
            match result {
                AnalysisResult::NeedsConversion {
                    file,
                    info,
                    output_path,
                } => {
                    analysis_stats.to_convert += 1;
                    conversions.push(ProcessableFile {
                        file,
                        info,
                        output_path,
                    });
                }
                AnalysisResult::NeedsRemux {
                    file,
                    info,
                    output_path,
                } => {
                    analysis_stats.to_remux += 1;
                    remuxes.push(ProcessableFile {
                        file,
                        info,
                        output_path,
                    });
                }
                AnalysisResult::NeedsRename { file } => {
                    analysis_stats.to_rename += 1;
                    renames.push(file);
                }
                AnalysisResult::Skip { file, reason } => {
                    match &reason {
                        SkipReason::AlreadyConverted => {
                            analysis_stats.skipped_converted += 1;
                        }
                        SkipReason::BitrateBelowThreshold { .. } => {
                            analysis_stats.skipped_bitrate += 1;
                        }
                        SkipReason::OutputExists { .. } => {
                            analysis_stats.skipped_duplicate += 1;
                        }
                        SkipReason::AnalysisFailed { error } => {
                            analysis_stats.analysis_failed += 1;
                            print_error!("{}: {error}", cli_tools::path_to_string_relative(&file.path));
                        }
                    }
                    // Print skipped files
                    if self.config.verbose && !matches!(reason, SkipReason::AnalysisFailed { .. }) {
                        print_warning!("{}: {reason}", cli_tools::path_to_string_relative(&file.path));
                    }
                }
            }
        }

        if self.config.sort_by_bitrate {
            // Sort by bitrate descending (highest first)
            conversions.sort_unstable_by(|a, b| b.info.bitrate_kbps.cmp(&a.info.bitrate_kbps));
        } else {
            conversions.sort_unstable_by(|a, b| a.file.path.cmp(&b.file.path));
        }

        remuxes.sort_unstable_by(|a, b| a.file.path.cmp(&b.file.path));

        self.log_analysis_stats(&analysis_stats);
        analysis_stats.print_summary();

        AnalysisOutput {
            conversions,
            remuxes,
            renames,
        }
    }

    /// Process all files that need renaming.
    fn process_renames(&self, files: &[VideoFile]) {
        let total = files.len();
        let num_digits = total.to_string().chars().count();

        for (index, file) in files.iter().enumerate() {
            let file_index = format!("[{:>width$}/{total}]", index + 1, width = num_digits);
            let new_path = cli_tools::insert_suffix_before_extension(&file.path, ".x265");

            if self.config.dryrun {
                println!("{}", format!("{file_index} [DRYRUN] Rename:").bold().purple());
                cli_tools::show_diff(
                    &cli_tools::path_to_string_relative(&file.path),
                    &cli_tools::path_to_string_relative(&new_path),
                );
            } else {
                println!("{}", format!("{file_index} Rename:").bold().purple());
                cli_tools::show_diff(
                    &cli_tools::path_to_string_relative(&file.path),
                    &cli_tools::path_to_string_relative(&new_path),
                );
                if let Err(e) = std::fs::rename(&file.path, &new_path) {
                    print_error!("Failed to rename {}: {e}", file.path.display());
                }
            }
        }
    }

    /// Process all files that need remuxing.
    fn process_remuxes(
        &self,
        files: Vec<ProcessableFile>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
    ) -> (RunStats, bool) {
        self.process_files(files, abort_flag, processed_count, |this, file, index| {
            this.remux_to_mp4(file, index)
        })
    }

    /// Process all files that need conversion.
    fn process_conversions(
        &self,
        files: Vec<ProcessableFile>,
        abort_flag: &AtomicBool,
        processed_count: &mut usize,
    ) -> (RunStats, bool) {
        self.process_files(files, abort_flag, processed_count, |this, file, index| {
            this.convert_to_hevc(file, index)
        })
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
        // Skip files with "x265" in the filename (already converted)
        if file.name.contains(".x265") && file.extension == TARGET_EXTENSION {
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

    fn log_init(&self) {
        self.logger.borrow_mut().log_init(&self.config);
    }

    fn log_analysis_stats(&self, stats: &AnalysisStats) {
        self.logger.borrow_mut().log_analysis_stats(stats);
    }

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

    fn log_failure(&self, file_path: &Path, operation: &str, file_index: &str, error: &str) {
        self.logger
            .borrow_mut()
            .log_failure(file_path, operation, file_index, error);
    }

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
    fn analyze_video_file(file: VideoFile, bitrate_limit: u64, overwrite: bool) -> AnalysisResult {
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

        let is_hevc = info.codec == "hevc" || info.codec == "h265";

        // Check if already converted (HEVC in MP4 with .x265 marker)
        if is_hevc && file.extension == TARGET_EXTENSION {
            if !file.name.contains(".x265") {
                // Needs rename to add .x265 suffix
                let new_path = cli_tools::insert_suffix_before_extension(&file.path, ".x265");
                if new_path.exists() && !overwrite {
                    return AnalysisResult::Skip {
                        file,
                        reason: SkipReason::OutputExists { path: new_path },
                    };
                }
                return AnalysisResult::NeedsRename { file };
            }
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::AlreadyConverted,
            };
        }

        // Check bitrate threshold
        if info.bitrate_kbps < bitrate_limit {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::BitrateBelowThreshold {
                    bitrate: info.bitrate_kbps,
                    threshold: bitrate_limit,
                },
            };
        }

        let output_path = file.output_path();

        // Check if output already exists
        if output_path.exists() && !overwrite {
            return AnalysisResult::Skip {
                file,
                reason: SkipReason::OutputExists { path: output_path },
            };
        }

        if is_hevc {
            AnalysisResult::NeedsRemux {
                file,
                info,
                output_path,
            }
        } else {
            AnalysisResult::NeedsConversion {
                file,
                info,
                output_path,
            }
        }
    }

    /// Run a command in a new process group to prevent Ctrl+C from propagating to it.
    /// This allows the main program to handle the signal and finish the current file gracefully.
    fn run_command_isolated(cmd: &mut Command) -> std::io::Result<ExitStatus> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
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
