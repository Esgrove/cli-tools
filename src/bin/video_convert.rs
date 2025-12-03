use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// Processing failed
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

    fn space_saved(&self) -> i64 {
        self.total_original_size as i64 - self.total_converted_size as i64
    }

    fn print_summary(&self) {
        println!("{}", "\n--- Conversion Summary ---".bold().magenta());
        println!("Files converted: {}", self.files_converted);
        println!("Files remuxed:   {}", self.files_remuxed);
        println!("Files skipped:   {}", self.files_skipped);
        println!("Files failed:    {}", self.files_failed);
        println!("");

        if self.files_converted > 0 {
            println!("Total original size:    {}", format_size(self.total_original_size));
            println!("Total converted size:   {}", format_size(self.total_converted_size));

            if self.total_original_size > 0 {
                let ratio = self.total_converted_size as f64 / self.total_original_size as f64 * 100.0;

                let saved = self.space_saved();
                if saved >= 0 {
                    println!("Space saved:     {} ({:.1}%)", format_size(saved as u64), ratio);
                } else {
                    println!("Space increased: {} ({:.1}%)", format_size((-saved) as u64), ratio);
                }
            }
        }

        println!("Total time:      {}", format_duration(self.total_duration));
    }
}

/// Format bytes as human-readable size
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    }
}

/// Format duration as human-readable string
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
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

    fn process_files(&self, files: Vec<VideoFile>) -> Result<ConversionStats> {
        let mut stats = ConversionStats::default();
        let total = files.len().min(self.config.number);
        let total_digits = total.to_string().chars().count();

        let mut processed_files: usize = 0;
        for (index, file) in files.into_iter().enumerate() {
            let file_display = cli_tools::path_to_string_relative(&file.path);
            println!(
                "[{:>width$}/{}] Processing: {}",
                processed_files + 1,
                self.config.number,
                file_display,
                width = total_digits
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
                        format_size(*original_size),
                        format_size(*converted_size),
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

        println!(
            "  Codec: {}, Bitrate: {} Mbps, Size: {}",
            info.codec,
            info.bitrate_kbps,
            format_size(info.size_bytes)
        );

        // Check bitrate threshold
        if info.bitrate_kbps < self.config.bitrate {
            return ProcessResult::Skipped {
                reason: format!(
                    "Bitrate {} kbps is below threshold {} kbps",
                    info.bitrate_kbps, self.config.bitrate
                ),
            };
        }

        // Determine if we need to convert or just remux
        let is_hevc = info.codec == "hevc" || info.codec == "h265";

        if is_hevc && file.extension == TARGET_EXTENSION {
            return ProcessResult::Skipped {
                reason: "Already HEVC in MP4 container".to_string(),
            };
        }

        let output_path = self.generate_output_path(file);

        // Check if output already exists
        if output_path.exists() && !self.config.overwrite {
            return ProcessResult::Skipped {
                reason: format!("Output file already exists: {}", output_path.display()),
            };
        }

        let result = if is_hevc {
            self.remux_to_mp4(&file.path, &output_path)
        } else {
            self.convert_to_hevc_mp4(&file.path, &output_path)
        };

        match result {
            Ok(()) => {
                if !self.config.dryrun {
                    if let Err(e) = self.delete_original_file(&file.path) {
                        print_warning!("Failed to delete original file: {e}");
                    }
                }

                if is_hevc {
                    ProcessResult::Remuxed {}
                } else {
                    let new_size = std::fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);
                    ProcessResult::Converted {
                        original_size: info.size_bytes,
                        converted_size: new_size,
                    }
                }
            }
            Err(e) => ProcessResult::Failed { error: e.to_string() },
        }
    }

    /// Get video information using ffprobe
    fn get_video_info(&self, path: &Path) -> Result<VideoInfo> {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-select_streams",
                "v:0",
            ])
            .arg(path)
            .output()
            .context("Failed to execute ffprobe")?;

        if !output.status.success() {
            anyhow::bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("Failed to parse ffprobe output")?;

        // Get codec from first video stream
        let codec = json["streams"]
            .as_array()
            .and_then(|streams| streams.first())
            .and_then(|stream| stream["codec_name"].as_str())
            .unwrap_or("unknown")
            .to_lowercase();

        // Get bitrate from format (in bits/sec, convert to kbps)
        let bitrate_bps = json["format"]["bit_rate"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let bitrate_kbps = bitrate_bps / 1000;

        // Get file size
        let size_bytes = json["format"]["size"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_else(|| fs::metadata(path).map(|m| m.len()).unwrap_or(0));

        Ok(VideoInfo {
            codec,
            bitrate_kbps,
            size_bytes,
        })
    }

    /// Generate output path for converted file
    fn generate_output_path(&self, file: &VideoFile) -> PathBuf {
        let stem = file.path.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
        let parent = file.path.parent().unwrap_or_else(|| Path::new("."));

        // Add .x265 marker and use mp4 extension
        let new_name = format!("{stem}.x265.mp4");
        parent.join(new_name)
    }

    /// Remux video (copy streams to new container)
    fn remux_to_mp4(&self, input: &Path, output: &Path) -> Result<()> {
        if self.config.verbose {
            println!("  Remuxing to: {}", output.display());
        }

        if self.config.dryrun {
            println!("  [DRY RUN] ffmpeg -i {:?} -c copy {:?}", input, output);
            return Ok(());
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-i"]).arg(input).args(["-c", "copy", "-y"]).arg(output);

        if !self.config.verbose {
            cmd.args(["-v", "quiet", "-stats"]);
        }

        let status = cmd.status().context("Failed to execute ffmpeg")?;

        if !status.success() {
            anyhow::bail!("ffmpeg remux failed with status: {status}");
        }

        Ok(())
    }

    /// Convert video to HEVC using NVENC
    fn convert_to_hevc_mp4(&self, input: &Path, output: &Path) -> Result<()> {
        if self.config.verbose {
            println!("  Converting to HEVC: {}", output.display());
        }

        if self.config.dryrun {
            println!(
                "  [DRY RUN] ffmpeg -i {:?} -c:v hevc_nvenc -preset p7 -cq 23 -c:a copy {:?}",
                input, output
            );
            return Ok(());
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-i"])
            .arg(input)
            .args(["-c:v", "hevc_nvenc", "-preset", "p7", "-cq", "23", "-c:a", "copy", "-y"])
            .arg(output);

        if !self.config.verbose {
            cmd.args(["-v", "quiet", "-stats"]);
        }

        let status = cmd.status().context("Failed to execute ffmpeg")?;

        if !status.success() {
            // Clean up failed output file
            let _ = fs::remove_file(output);
            anyhow::bail!("ffmpeg conversion failed with status: {status}");
        }

        Ok(())
    }

    /// Handle the original file after successful conversion
    fn delete_original_file(&self, path: &Path) -> Result<()> {
        if self.config.delete {
            fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
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
}

fn main() -> Result<()> {
    let args = Args::parse();
    VideoConvert::new(args)?.run()
}
