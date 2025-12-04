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
use walkdir::WalkDir;

use crate::VideoConvertArgs;
use crate::config::{Config, VideoConvertConfig};
use crate::logger::FileLogger;
use crate::stats::{ConversionStats, RunStats};

const TARGET_EXTENSION: &str = "mp4";
const FFMPEG_DEFAULT_ARGS: &[&str] = &["-hide_banner", "-nostdin", "-stats", "-loglevel", "info", "-y"];

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
}

/// Result of processing a single file
#[derive(Debug)]
pub enum ProcessResult {
    /// File was converted successfully
    Converted { output: PathBuf, stats: ConversionStats },
    /// File was remuxed (already HEVC, just changed container to MP4)
    Remuxed { output: PathBuf },
    /// File was renamed (already HEVC, added .x265 suffix)
    Renamed { output: PathBuf },
    /// File was skipped
    Skipped(SkipReason),
    /// Failed to process file
    Failed { error: String },
}

impl ProcessResult {
    /// A successful conversion result with size statistics.
    const fn converted(original_size: u64, converted_size: u64, output: PathBuf) -> Self {
        Self::Converted {
            output,
            stats: ConversionStats::new(original_size, converted_size),
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

impl std::fmt::Display for VideoInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Codec:      {}", self.codec)?;
        writeln!(f, "Size:       {}", cli_tools::format_size(self.size_bytes))?;
        writeln!(f, "Bitrate:    {:.2} Mbps", self.bitrate_kbps as f64 / 1000.0)?;
        writeln!(f, "Duration:   {}", cli_tools::format_duration_seconds(self.duration))?;
        write!(f, "Resolution: {}x{}", self.width, self.height)
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
                write!(f, "Output file already exists: {}", path.display())
            }
        }
    }
}

impl From<walkdir::DirEntry> for VideoFile {
    fn from(entry: walkdir::DirEntry) -> Self {
        Self::new(&entry.into_path())
    }
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
        let files = self.gather_files_to_convert()?;
        if files.is_empty() {
            println!("No files to convert found");
            return Ok(());
        }

        if self.config.verbose {
            println!("Found {} file(s) to process", files.len());
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

        let (stats, aborted) = self.process_files(files, &abort_flag);

        if aborted {
            println!("\n{}", "Aborted by user".bold().red());
        }

        stats.print_summary();

        Ok(())
    }

    /// Gather video files based on the config settings.
    /// Returns a list of files to convert.
    fn gather_files_to_convert(&self) -> Result<Vec<VideoFile>> {
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

        files.sort();
        Ok(files)
    }

    /// Process video files.
    fn process_files(&self, files: Vec<VideoFile>, abort_flag: &AtomicBool) -> (RunStats, bool) {
        let mut stats = RunStats::default();
        let total = files.len();
        let num_files_to_process = total.min(self.config.number);
        let num_digits = num_files_to_process.to_string().chars().count();

        let mut processed_files: usize = 0;
        let mut aborted = false;

        self.log_init();

        for (index, file) in files.into_iter().enumerate() {
            // Check abort flag before starting a new file
            if abort_flag.load(Ordering::SeqCst) {
                aborted = true;
                break;
            }
            if processed_files >= num_files_to_process {
                println!("\nReached file limit");
                break;
            }

            if !self.config.verbose {
                print!("\rProcessing: {index}/{total}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            let file_index = format!(
                "[{:>width$}/{}]",
                processed_files + 1,
                num_files_to_process,
                width = num_digits
            );

            let start = Instant::now();
            let result = self.process_single_file(&file, &file_index);
            let duration = start.elapsed();

            match &result {
                ProcessResult::Converted { output, stats } => {
                    println!(
                        "{}",
                        format!("✓ Converted in {}: {stats}", cli_tools::format_duration(duration)).cyan()
                    );
                    self.log_success(output, "convert", &file_index, duration, Some(stats));
                    processed_files += 1;
                }
                ProcessResult::Remuxed { output } => {
                    println!(
                        "{}",
                        format!("✓ Remuxed in {}", cli_tools::format_duration(duration)).green()
                    );
                    self.log_success(output, "remux", &file_index, duration, None);
                    processed_files += 1;
                }
                ProcessResult::Renamed { output } => {
                    self.log_success(output, "rename", &file_index, duration, None);
                }
                ProcessResult::Skipped(reason) => {
                    if self.config.verbose {
                        print_warning!("[{index}]: {}", cli_tools::path_to_string_relative(&file.path));
                        println!("⊘ Skipped: {reason}");
                    }
                }
                ProcessResult::Failed { error } => {
                    print_error!("{error}");
                    self.log_failure(&file.path, "process", &file_index, error);
                }
            }

            stats.add_result(&result, duration);
        }

        self.log_stats(&stats);
        (stats, aborted)
    }

    /// Process a single video file
    fn process_single_file(&self, file: &VideoFile, file_index: &str) -> ProcessResult {
        if !self.config.verbose {
            // Clear the progress line before printing meaningful output
            print!("\r");
        }

        // Get video info
        let info = match self.get_video_info(&file.path) {
            Ok(info) => info,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to get video info: {e}"),
                };
            }
        };

        // Determine if we need to convert or just remux
        let is_hevc = info.codec == "hevc" || info.codec == "h265";

        if is_hevc && file.extension == TARGET_EXTENSION {
            // Rename to add .x265. suffix if missing
            if !file.name.contains(".x265") {
                let new_path = cli_tools::insert_suffix_before_extension(&file.path, ".x265");
                // Check if the new path already exists
                if new_path.exists() && !self.config.overwrite {
                    return ProcessResult::Skipped(SkipReason::OutputExists { path: new_path });
                }
                if self.config.dryrun {
                    println!("{}", "[DRYRUN] Rename:                ".bold());
                    cli_tools::show_diff(
                        &cli_tools::path_to_string_relative(&file.path),
                        &cli_tools::path_to_string_relative(&new_path),
                    );
                    return ProcessResult::Renamed { output: new_path };
                } else if let Err(e) = std::fs::rename(&file.path, &new_path) {
                    return ProcessResult::Failed {
                        error: format!("Failed to rename file: {e}"),
                    };
                }
                // Extra whitespace to ensure running index is erased
                println!("{}", "Renamed:                ".bold());
                cli_tools::show_diff(
                    &cli_tools::path_to_string_relative(&file.path),
                    &cli_tools::path_to_string_relative(&new_path),
                );
                return ProcessResult::Renamed { output: new_path };
            }
            return ProcessResult::Skipped(SkipReason::AlreadyConverted);
        }

        // Check bitrate threshold
        if info.bitrate_kbps < self.config.bitrate_limit {
            return ProcessResult::Skipped(SkipReason::BitrateBelowThreshold {
                bitrate: info.bitrate_kbps,
                threshold: self.config.bitrate_limit,
            });
        }

        let output_path = file.output_path();

        // Check if output already exists
        if output_path.exists() && !self.config.overwrite {
            return ProcessResult::Skipped(SkipReason::OutputExists { path: output_path });
        }

        if is_hevc {
            self.remux_to_mp4(&file.path, &output_path, &info, file_index)
        } else {
            self.convert_to_hevc_mp4(&file.path, &output_path, &info, &file.extension, file_index)
        }
    }

    /// Get video information using ffprobe
    fn get_video_info(&self, path: &Path) -> Result<VideoInfo> {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=codec_name,bit_rate,width,height:stream_tags=BPS,BPS-eng:format=bit_rate,size,duration",
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
        // ```
        let mut codec = String::new();
        let mut bitrate_bps: Option<u64> = None;
        let mut size_bytes: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;

        for line in stdout.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "codec_name" => codec = value.to_lowercase(),
                    // Try multiple bitrate sources: stream bit_rate, format bit_rate, or BPS tags
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
                    _ => {}
                }
            }
        }

        // Fall back to file metadata for size if not in ffprobe output
        let size_bytes = size_bytes.unwrap_or_else(|| fs::metadata(path).map(|m| m.len()).unwrap_or(0));

        // Convert bitrate from bps to kbps
        let bitrate_kbps = bitrate_bps.unwrap_or(0) / 1000;

        let duration = duration.unwrap_or(0.0);
        let width = width.unwrap_or(0);
        let height = height.unwrap_or(0);

        if !stderr.is_empty() && self.config.verbose {
            print_warning!("ffprobe: {}", stderr.trim());
        }

        Ok(VideoInfo {
            codec,
            bitrate_kbps,
            size_bytes,
            duration,
            width,
            height,
        })
    }

    /// Remux video (copy streams to new container)
    fn remux_to_mp4(&self, input: &Path, output: &Path, info: &VideoInfo, file_index: &str) -> ProcessResult {
        println!(
            "{}",
            format!("{file_index} Remuxing: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .green()
        );
        println!("{info}");

        if self.config.verbose {
            println!("Remuxing: {}", cli_tools::path_to_string_relative(output));
        }

        self.log_start(input, "remux", file_index, info);

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
            return ProcessResult::Remuxed {
                output: output.to_path_buf(),
            };
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
            if let Err(e) = self.delete_original_file(input) {
                print_error!("Failed to delete original file: {e}");
            }
            return ProcessResult::Remuxed {
                output: output.to_path_buf(),
            };
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

        let status = match run_command_isolated(&mut cmd) {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if !status.success() {
            let _ = fs::remove_file(output);
            return ProcessResult::Failed {
                error: format!(
                    "ffmpeg remux with AAC transcode failed with status: {}",
                    status.code().unwrap_or(-1)
                ),
            };
        }

        if let Err(e) = self.delete_original_file(input) {
            print_error!("Failed to delete original file: {e}");
        }

        ProcessResult::Remuxed {
            output: output.to_path_buf(),
        }
    }

    /// Convert video to HEVC using NVENC
    fn convert_to_hevc_mp4(
        &self,
        input: &Path,
        output: &Path,
        info: &VideoInfo,
        extension: &str,
        file_index: &str,
    ) -> ProcessResult {
        println!(
            "{}",
            format!("{file_index} Converting: {}", cli_tools::path_to_string_relative(input))
                .bold()
                .magenta()
        );
        println!("{info}");

        if self.config.verbose {
            println!("Converting: {}", cli_tools::path_to_string_relative(output));
        }

        self.log_start(input, "convert", file_index, info);

        // Determine quality level based on resolution and bitrate.
        // Quality level 1 to 51, lower is better quality and bigger file size.
        let is_4k = info.width.max(info.height) >= 2160;
        let bitrate_mbps = info.bitrate_kbps as f64 / 1000.0;

        let quality_level = if is_4k {
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
        };

        println!("Using quality level: {quality_level}");

        // Determine audio codec: copy for mp4/mkv, transcode for others
        let copy_audio = extension == "mp4" || extension == "mkv";

        let mut cmd = Self::build_hevc_command(input, output, quality_level, copy_audio, true);

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::converted(info.size_bytes, 0, output.to_path_buf());
        }

        // First attempt: try with CUDA filters for better performance
        let status = match run_command_isolated(&mut cmd) {
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
            let mut cmd = Self::build_hevc_command(input, output, quality_level, copy_audio, false);
            let status = match run_command_isolated(&mut cmd) {
                Ok(s) => s,
                Err(e) => {
                    return ProcessResult::Failed {
                        error: format!("Failed to execute ffmpeg (retry): {e}"),
                    };
                }
            };

            if !status.success() {
                // Clean up failed output file
                let _ = fs::remove_file(output);
                return ProcessResult::Failed {
                    error: format!("ffmpeg conversion failed with status: {}", status.code().unwrap_or(-1)),
                };
            }
        }

        if let Err(e) = self.delete_original_file(input) {
            print_error!("Failed to delete original file: {e}");
        }

        let new_size = fs::metadata(output).map(|m| m.len()).unwrap_or(0);

        ProcessResult::converted(info.size_bytes, new_size, output.to_path_buf())
    }

    /// Build the ffmpeg command for HEVC conversion.
    /// When `use_cuda_filters` is true, uses `hwupload_cuda` and `scale_cuda` for GPU-accelerated filtering.
    /// When false, uses CPU-based filtering which is more compatible but slightly slower.
    fn build_hevc_command(
        input: &Path,
        output: &Path,
        quality_level: u32,
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
        // Skip files with "x265" in the name (already converted)
        if file.name.contains(".x265") && file.extension == TARGET_EXTENSION {
            return false;
        }

        if !self.config.extensions.iter().any(|ext| ext == &file.extension) {
            return false;
        }

        // Check include patterns (if specified, file must match at least one)
        if !self.config.include.is_empty() {
            let matches_include = self.config.include.iter().any(|pattern| file.name.contains(pattern));
            if !matches_include {
                return false;
            }
        }

        // Check exclude patterns (file must not match any)
        if self.config.exclude.iter().any(|pattern| file.name.contains(pattern)) {
            return false;
        }

        true
    }

    fn log_init(&self) {
        self.logger.borrow_mut().log_init(&self.config);
    }

    fn log_start(&self, file_path: &Path, operation: &str, file_index: &str, info: &VideoInfo) {
        self.logger
            .borrow_mut()
            .log_start(file_path, operation, file_index, info);
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

    /// Handle the original file after successful processing
    fn delete_original_file(&self, path: &Path) -> Result<()> {
        // Use direct delete if configured or if on a network drive (trash doesn't work there)
        if self.config.delete || cli_tools::is_network_path(path) {
            println!("Deleting: {}", path.display());
            std::fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            println!("Trashing: {}", path.display());
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
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
