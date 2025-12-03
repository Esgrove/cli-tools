use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use clap_complete::Shell;
use colored::Colorize;
use serde::Deserialize;
use walkdir::WalkDir;

use cli_tools::{print_error, print_warning};

/// Default video extensions
const DEFAULT_EXTENSIONS: &[&str] = &["mp4", "mkv"];

/// Other video extensions excluding mp4
const OTHER_EXTENSIONS: &[&str] = &["mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

/// All video extensions
const ALL_EXTENSIONS: &[&str] = &["mp4", "mkv", "wmv", "flv", "m4v", "ts", "mpg", "avi", "mov", "webm"];

const TARGET_EXTENSION: &str = "mp4";

const FFMPEG_DEFAULT_ARGS: &[&str] = &["-hide_banner", "-nostdin", "-stats", "-loglevel", "info", "-y"];

#[derive(Parser)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Convert video files to HEVC (H.265) format using ffmpeg and NVENC")]
struct Args {
    /// Optional input directory or file
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Convert all known video file types
    #[arg(short, long)]
    all: bool,

    /// Skip files with bitrate lower than LIMIT kbps
    #[arg(short, long, name = "LIMIT", default_value_t = 8000)]
    bitrate: u64,

    /// Delete input files immediately instead of moving to trash
    #[arg(short, long)]
    delete: bool,

    /// Print commands without running them
    #[arg(short, long)]
    print: bool,

    /// Overwrite existing output files
    #[arg(short, long)]
    force: bool,

    /// Include files that match the given pattern
    #[arg(short = 'i', long, num_args = 1, action = clap::ArgAction::Append, name = "INCLUDE")]
    include: Vec<String>,

    /// Exclude files that match the given pattern
    #[arg(short = 'e', long, num_args = 1, action = clap::ArgAction::Append, name = "EXCLUDE")]
    exclude: Vec<String>,

    /// Override file extensions to convert
    #[arg(short = 't', long, num_args = 1, action = clap::ArgAction::Append, name = "EXTENSION")]
    extension: Vec<String>,

    /// Number of files to convert
    #[arg(short, long, default_value_t = 1)]
    number: usize,

    /// Convert file types other than mp4
    #[arg(short, long)]
    other: bool,

    /// Recurse into subdirectories
    #[arg(short, long)]
    recurse: bool,

    /// Generate shell completion
    #[arg(short = 'l', long, name = "SHELL")]
    completion: Option<Shell>,

    /// Display commands being executed
    #[arg(short, long)]
    verbose: bool,
}

/// Config from a config file
#[derive(Debug, Default, Deserialize)]
struct VideoConvertConfig {
    #[serde(default)]
    convert_all_types: bool,
    #[serde(default)]
    bitrate: Option<u64>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    exclude: Vec<String>,
    /// Custom list of file extensions to process (overrides all/other flags)
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    number: Option<usize>,
    #[serde(default)]
    convert_other_types: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    verbose: bool,
}

/// Wrapper needed for parsing the config file section.
#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    video_convert: VideoConvertConfig,
}

/// Final config created from CLI arguments and user config file.
#[derive(Debug, Default)]
#[allow(unused)]
struct Config {
    bitrate: u64,
    convert_all: bool,
    convert_other: bool,
    delete: bool,
    dryrun: bool,
    exclude: Vec<String>,
    include: Vec<String>,
    /// File extensions to convert (lowercase, without leading dot)
    extensions: Vec<String>,
    number: usize,
    overwrite: bool,
    path: PathBuf,
    recursive: bool,
    verbose: bool,
}

#[derive(Debug)]
struct VideoConvert {
    config: Config,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct VideoFile {
    path: PathBuf,
    name: String,
    extension: String,
}

/// Information about a video file from ffprobe
#[derive(Debug)]
struct VideoInfo {
    /// Video codec name (e.g., "hevc", "h264")
    codec: String,
    /// Video bitrate in kbps
    bitrate_kbps: u64,
    /// File size in bytes
    size_bytes: u64,
    /// Duration in seconds
    duration: f64,
    /// Video width in pixels
    width: u32,
    /// Video height in pixels
    height: u32,
}

/// Result of processing a single file
#[derive(Debug)]
enum ProcessResult {
    /// File was converted successfully
    Converted { original_size: u64, converted_size: u64 },
    /// File was remuxed (already HEVC, just changed container to MP4)
    Remuxed {},
    /// File was skipped (already HEVC in correct container or low bitrate)
    Skipped { reason: String },
    /// Failed to process file
    Failed { error: String },
}

/// Statistics for the conversion run
#[derive(Debug, Default)]
struct ConversionStats {
    files_converted: usize,
    files_remuxed: usize,
    files_skipped: usize,
    files_failed: usize,
    total_original_size: u64,
    total_converted_size: u64,
    total_duration: Duration,
}

impl ConversionStats {
    fn add_result(&mut self, result: &ProcessResult, duration: Duration) {
        self.total_duration += duration;
        match result {
            ProcessResult::Converted {
                original_size,
                converted_size,
            } => {
                self.files_converted += 1;
                self.total_original_size += original_size;
                self.total_converted_size += converted_size;
            }
            ProcessResult::Remuxed {} => {
                self.files_remuxed += 1;
            }
            ProcessResult::Skipped { .. } => {
                self.files_skipped += 1;
            }
            ProcessResult::Failed { .. } => {
                self.files_failed += 1;
            }
        }
    }

    #[allow(clippy::cast_possible_wrap, clippy::missing_const_for_fn)]
    fn space_saved(&self) -> i64 {
        self.total_original_size as i64 - self.total_converted_size as i64
    }

    fn print_summary(&self) {
        println!("{}", "\n--- Conversion Summary ---".bold().magenta());
        println!("Files converted: {}", self.files_converted);
        println!("Files remuxed:   {}", self.files_remuxed);
        println!("Files skipped:   {}", self.files_skipped);
        println!("Files failed:    {}", self.files_failed);
        println!();

        if self.files_converted > 0 {
            println!(
                "Total original size:    {}",
                cli_tools::format_size(self.total_original_size)
            );
            println!(
                "Total converted size:   {}",
                cli_tools::format_size(self.total_converted_size)
            );

            if self.total_original_size > 0 {
                let ratio = self.total_converted_size as f64 / self.total_original_size as f64 * 100.0;

                let saved = self.space_saved();
                if saved >= 0 {
                    println!(
                        "Space saved:     {} ({:.1}%)",
                        cli_tools::format_size(saved as u64),
                        ratio
                    );
                } else {
                    println!(
                        "Space increased: {} ({:.1}%)",
                        cli_tools::format_size((-saved) as u64),
                        ratio
                    );
                }
            }
        }

        println!("Total time:      {}", cli_tools::format_duration(self.total_duration));
    }
}

impl VideoConvertConfig {
    /// Try to read user config from the file if it exists.
    /// Otherwise, fall back to default config.
    fn get_user_config() -> Self {
        cli_tools::config::CONFIG_PATH
            .as_deref()
            .and_then(|path| {
                fs::read_to_string(path)
                    .map_err(|e| {
                        print_error!("Error reading config file {}: {e}", path.display());
                    })
                    .ok()
            })
            .and_then(|config_string| {
                toml::from_str::<UserConfig>(&config_string)
                    .map_err(|e| {
                        print_error!("Error reading config file: {e}");
                    })
                    .ok()
            })
            .map(|config| config.video_convert)
            .unwrap_or_default()
    }
}

impl VideoFile {
    pub fn new(path: &Path) -> Self {
        let path = path.to_owned();
        let name = cli_tools::path_to_filename_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }

    pub fn from_dir_entry(entry: walkdir::DirEntry) -> Self {
        let path = entry.into_path();
        let name = cli_tools::path_to_filename_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }
}

impl std::fmt::Display for VideoInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Size: {}, Duration: {:.2}, Codec: {}\nResolution: {}x{}, Bitrate: {:.1} Mbps",
            cli_tools::format_size(self.size_bytes),
            cli_tools::format_duration_seconds(self.duration),
            self.codec,
            self.width,
            self.height,
            self.bitrate_kbps / 1000,
        )
    }
}

impl std::fmt::Display for VideoFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

impl Config {
    /// Create config from given command line args and user config file.
    pub fn try_from_args(args: Args, user_config: VideoConvertConfig) -> Result<Self> {
        let mut include = args.include;
        include.extend(user_config.include);

        let mut exclude = args.exclude;
        exclude.extend(user_config.exclude);

        let path = cli_tools::resolve_input_path(args.path.as_deref())?;

        let convert_all = args.all || user_config.convert_all_types;
        let convert_other = args.other || user_config.convert_other_types;

        let extensions = if !args.extension.is_empty() {
            args.extension.iter().map(|s| s.to_lowercase()).collect()
        } else if !user_config.extensions.is_empty() {
            user_config.extensions.iter().map(|s| s.to_lowercase()).collect()
        } else if args.all || user_config.convert_all_types {
            ALL_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
        } else if args.other || user_config.convert_other_types {
            OTHER_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
        } else {
            DEFAULT_EXTENSIONS.iter().map(|s| (*s).to_string()).collect()
        };

        Ok(Self {
            bitrate: user_config.bitrate.unwrap_or(args.bitrate),
            convert_all,
            convert_other,
            delete: args.delete || user_config.delete,
            dryrun: args.print,
            exclude,
            extensions,
            include,
            number: user_config.number.unwrap_or(args.number),
            overwrite: args.force || user_config.overwrite,
            path,
            recursive: args.recurse || user_config.recursive,
            verbose: args.verbose || user_config.verbose,
        })
    }
}

impl VideoConvert {
    pub fn new(args: Args) -> Result<Self> {
        let user_config = VideoConvertConfig::get_user_config();
        let config = Config::try_from_args(args, user_config)?;

        Ok(Self { config })
    }

    pub fn run(&self) -> Result<()> {
        let files = self.gather_files_to_convert()?;
        if files.is_empty() {
            println!("No files to convert found");
            return Ok(());
        }

        if self.config.verbose {
            println!("Found {} file(s) to process", files.len());
        }

        let stats = self.process_files(files)?;
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

        let walker = WalkDir::new(path)
            .max_depth(max_depth)
            .into_iter()
            .filter_map(std::result::Result::ok);

        let mut files: Vec<VideoFile> = walker
            .filter(|entry| entry.file_type().is_file())
            .map(VideoFile::from_dir_entry)
            .filter(|file| self.should_include_file(file))
            .collect();

        files.sort();
        Ok(files)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn process_files(&self, files: Vec<VideoFile>) -> Result<ConversionStats> {
        let mut stats = ConversionStats::default();
        let total = files.len().min(self.config.number);
        let total_digits = total.to_string().chars().count();

        let mut processed_files: usize = 0;
        for file in files {
            let file_display = cli_tools::path_to_string_relative(&file.path);
            println!(
                "{}",
                format!(
                    "[{:>width$}/{}] Processing: {}",
                    processed_files + 1,
                    self.config.number,
                    file_display,
                    width = total_digits
                )
                .bold()
                .magenta()
            );

            let start = Instant::now();
            let result = self.process_single_file(&file);
            let duration = start.elapsed();

            match &result {
                ProcessResult::Converted {
                    original_size,
                    converted_size,
                } => {
                    println!(
                        "  ✓ Converted: {} -> {}",
                        cli_tools::format_size(*original_size),
                        cli_tools::format_size(*converted_size),
                    );
                    processed_files += 1;
                }
                ProcessResult::Remuxed {} => {
                    println!("  ✓ Remuxed",);
                    processed_files += 1;
                }
                ProcessResult::Skipped { reason } => {
                    println!("  ⊘ Skipped: {reason}");
                }
                ProcessResult::Failed { error } => {
                    print_error!("  ✗ Failed: {error}");
                }
            }

            stats.add_result(&result, duration);
        }

        Ok(stats)
    }

    /// Process a single video file
    fn process_single_file(&self, file: &VideoFile) -> ProcessResult {
        // Get video info
        let info = match self.get_video_info(&file.path) {
            Ok(info) => info,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to get video info: {e}"),
                };
            }
        };

        println!("{info}");

        // Check bitrate threshold
        if info.bitrate_kbps < self.config.bitrate {
            let bitrate = info.bitrate_kbps;
            let threshold = self.config.bitrate;
            return ProcessResult::Skipped {
                reason: format!("Bitrate {bitrate} kbps is below threshold {threshold} kbps"),
            };
        }

        // Determine if we need to convert or just remux
        let is_hevc = info.codec == "hevc" || info.codec == "h265";

        if is_hevc && file.extension == TARGET_EXTENSION {
            return ProcessResult::Skipped {
                reason: "Already HEVC in MP4 container".to_string(),
            };
        }

        let output_path = Self::get_output_path(file);

        // Check if output already exists
        if output_path.exists() && !self.config.overwrite {
            return ProcessResult::Skipped {
                reason: format!("Output file already exists: {}", output_path.display()),
            };
        }

        if is_hevc {
            self.remux_to_mp4(&file.path, &output_path)
        } else {
            self.convert_to_hevc_mp4(&file.path, &output_path, &info, &file.extension)
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
    fn remux_to_mp4(&self, input: &Path, output: &Path) -> ProcessResult {
        if self.config.verbose {
            println!("Remuxing: {}", cli_tools::path_to_string_relative(output));
        }

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

        let status = match cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit()).status() {
            Ok(s) => s,
            Err(e) => {
                return ProcessResult::Failed {
                    error: format!("Failed to execute ffmpeg: {e}"),
                };
            }
        };

        if status.success() {
            if let Err(e) = self.delete_original_file(input) {
                print_warning!("Failed to delete original file: {e}");
            }
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

        let status = match cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit()).status() {
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
                error: format!("ffmpeg remux with AAC transcode failed with status: {status}"),
            };
        }

        if let Err(e) = self.delete_original_file(input) {
            print_warning!("Failed to delete original file: {e}");
        }

        ProcessResult::Remuxed {}
    }

    /// Convert video to HEVC using NVENC
    fn convert_to_hevc_mp4(&self, input: &Path, output: &Path, info: &VideoInfo, extension: &str) -> ProcessResult {
        if self.config.verbose {
            println!("Converting to HEVC: {}", cli_tools::path_to_string_relative(output));
        }

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

        println!("  Using quality level: {quality_level}");

        // GPU tuning for RTX 4090 to use more VRAM and improve performance
        let extra_hw_frames = "64";
        let lookahead = "48";
        let preset = "p5"; // slow (good quality)

        // Determine audio codec: copy for mp4/mkv, transcode for others
        let copy_audio = extension == "mp4" || extension == "mkv";

        let mut cmd = Command::new("ffmpeg");
        cmd.args(FFMPEG_DEFAULT_ARGS)
            .args(["-probesize", "50M", "-analyzeduration", "1M"])
            .args(["-extra_hw_frames", extra_hw_frames])
            .arg("-i")
            .arg(input)
            .args(["-vf", "hwupload_cuda,scale_cuda=format=nv12"])
            .args(["-c:v", "hevc_nvenc"])
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

        if self.config.dryrun {
            println!("[DRYRUN] {cmd:#?}");
            return ProcessResult::Converted {
                original_size: info.size_bytes,
                converted_size: 0,
            };
        }

        let status = match cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit()).status() {
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
            return ProcessResult::Failed {
                error: format!("ffmpeg conversion failed with status: {status}"),
            };
        }

        if let Err(e) = self.delete_original_file(input) {
            print_warning!("Failed to delete original file: {e}");
        }

        let new_size = fs::metadata(output).map(|m| m.len()).unwrap_or(0);

        ProcessResult::Converted {
            original_size: info.size_bytes,
            converted_size: new_size,
        }
    }

    /// Check if a file should be converted based on extension and include/exclude patterns.
    fn should_include_file(&self, file: &VideoFile) -> bool {
        // Skip files with "x265" in the name (already converted)
        if file.name.contains(".x265.") {
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

    /// Get output path for the new file
    fn get_output_path(file: &VideoFile) -> PathBuf {
        let parent = file.path.parent().unwrap_or_else(|| Path::new("."));
        let new_name = format!("{}.x265.mp4", file.name);
        parent.join(new_name)
    }

    /// Handle the original file after successful processing
    fn delete_original_file(&self, path: &Path) -> Result<()> {
        if self.config.delete {
            std::fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    VideoConvert::new(args)?.run()
}
