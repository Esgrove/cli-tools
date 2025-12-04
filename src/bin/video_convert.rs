use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Local;
use clap::{CommandFactory, Parser};
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

    /// Don't convert MP4 files
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
struct Config {
    bitrate_limit: u64,
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

struct VideoConvert {
    config: Config,
    logger: RefCell<FileLogger>,
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

/// Statistics for the conversion run
#[derive(Debug, Default)]
struct RunStats {
    files_converted: usize,
    files_remuxed: usize,
    files_skipped_converted: usize,
    files_skipped_bitrate: usize,
    files_skipped_duplicate: usize,
    files_failed: usize,
    total_original_size: u64,
    total_converted_size: u64,
    total_duration: Duration,
}

/// Statistics for a single file conversion
#[derive(Debug, Default, Clone, Copy)]
struct ConversionStats {
    original_size: u64,
    converted_size: u64,
}

/// Simple file logger for conversion operations with buffered writes
struct FileLogger {
    writer: BufWriter<File>,
}

/// Reasons why a file was skipped
#[derive(Debug)]
enum SkipReason {
    /// File is already HEVC in MP4 container
    AlreadyConverted,
    /// File bitrate is below the threshold
    BitrateBelowThreshold { bitrate: u64, threshold: u64 },
    /// Output file already exists
    OutputExists { path: PathBuf },
}

/// Result of processing a single file
#[derive(Debug)]
enum ProcessResult {
    /// File was converted successfully
    Converted { output: PathBuf, stats: ConversionStats },
    /// File was remuxed (already HEVC, just changed container to MP4)
    Remuxed { output: PathBuf },
    /// File was skipped
    Skipped(SkipReason),
    /// Failed to process file
    Failed { error: String },
}

impl ProcessResult {
    const fn converted(original_size: u64, converted_size: u64, output: PathBuf) -> Self {
        Self::Converted {
            output,
            stats: ConversionStats::new(original_size, converted_size),
        }
    }
}

impl ConversionStats {
    const fn new(original_size: u64, converted_size: u64) -> Self {
        Self {
            original_size,
            converted_size,
        }
    }

    /// Calculate the size difference (positive = saved, negative = increased)
    #[allow(clippy::cast_possible_wrap)]
    const fn size_difference(&self) -> i64 {
        self.original_size as i64 - self.converted_size as i64
    }

    /// Calculate the percentage change (positive = reduced, negative = increased)
    fn change_percentage(&self) -> f64 {
        if self.original_size == 0 || self.converted_size == 0 {
            return 0.0;
        }
        let diff = self.size_difference();
        diff as f64 / self.original_size as f64 * 100.0
    }
}

impl FileLogger {
    /// Create a new file logger, writing to ~/logs/cli-tools/video_convert_<timestamp>.log
    fn new() -> Result<Self> {
        let home_dir = dirs::home_dir().context("Failed to get home directory")?;
        let log_dir = home_dir.join("logs").join("cli-tools");

        // Create log directory if it doesn't exist
        if !log_dir.exists() {
            fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
        }

        let log_path = log_dir.join(format!(
            "video_convert_{}.log",
            Local::now().format("%Y-%m-%d_%H-%M-%S")
        ));

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to create log file: {}", log_path.display()))?;

        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    fn timestamp() -> String {
        Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Log when starting the program
    fn log_init(&mut self, config: &Config) {
        let _ = writeln!(
            self.writer,
            "[{}] INIT \"{}\"",
            Self::timestamp(),
            config.path.display()
        );
        let _ = writeln!(self.writer, "  bitrate_limit: {}", config.bitrate_limit);
        let _ = writeln!(self.writer, "  convert_all: {}", config.convert_all);
        let _ = writeln!(self.writer, "  convert_other: {}", config.convert_other);
        if !config.include.is_empty() {
            let _ = writeln!(self.writer, "  include: {:?}", config.include);
        }
        if !config.exclude.is_empty() {
            let _ = writeln!(self.writer, "  exclude: {:?}", config.exclude);
        }
        let _ = writeln!(self.writer, "  extensions: {:?}", config.extensions);
        let _ = writeln!(self.writer, "  recursive: {}", config.recursive);
        let _ = writeln!(self.writer, "  delete: {}", config.delete);
        let _ = writeln!(self.writer, "  overwrite: {}", config.overwrite);
        let _ = writeln!(self.writer, "  dryrun: {}", config.dryrun);
        let _ = writeln!(self.writer, "  number: {}", config.number);
        let _ = writeln!(self.writer, "  verbose: {}", config.verbose);
        let _ = self.writer.flush();
    }

    /// Log when starting a conversion or remux operation
    fn log_start(&mut self, file_path: &Path, operation: &str, file_index: &str, info: &VideoInfo) {
        let _ = writeln!(
            self.writer,
            "[{}] START   {} {} - \"{}\" | {} {}x{} {:.2} Mbps ",
            Self::timestamp(),
            operation.to_uppercase(),
            file_index,
            file_path.display(),
            info.codec,
            info.width,
            info.height,
            info.bitrate_kbps as f64 / 1000.0,
        );
        let _ = self.writer.flush();
    }

    /// Log when a conversion or remux finishes successfully
    fn log_success(
        &mut self,
        file_path: &Path,
        operation: &str,
        file_index: &str,
        duration: Duration,
        stats: Option<&ConversionStats>,
    ) {
        let duration_str = cli_tools::format_duration(duration);
        let size_info = stats.map_or(String::new(), |s| format!(" | {s}"));
        let _ = writeln!(
            self.writer,
            "[{}] SUCCESS {} {} - \"{}\" | Time: {}{}",
            Self::timestamp(),
            operation.to_uppercase(),
            file_index,
            file_path.display(),
            duration_str,
            size_info
        );
        let _ = self.writer.flush();
    }

    /// Log when a conversion or remux fails
    fn log_failure(&mut self, file_path: &Path, operation: &str, file_index: &str, error: &str) {
        let _ = writeln!(
            self.writer,
            "[{}] ERROR   {} {} - \"{}\" | {}",
            Self::timestamp(),
            operation.to_uppercase(),
            file_index,
            file_path.display(),
            error
        );
        let _ = self.writer.flush();
    }

    /// Log final statistics
    fn log_stats(&mut self, stats: &RunStats) {
        let _ = writeln!(self.writer, "[{}] STATISTICS", Self::timestamp());
        let _ = writeln!(self.writer, "  Files converted: {}", stats.files_converted);
        let _ = writeln!(self.writer, "  Files remuxed:   {}", stats.files_remuxed);
        let _ = writeln!(self.writer, "  Files skipped:   {}", stats.total_skipped());
        if stats.total_skipped() > 0 {
            let _ = writeln!(
                self.writer,
                "    - Already converted:   {}",
                stats.files_skipped_converted
            );
            let _ = writeln!(
                self.writer,
                "    - Below bitrate limit: {}",
                stats.files_skipped_bitrate
            );
            let _ = writeln!(
                self.writer,
                "    - Duplicate:           {}",
                stats.files_skipped_duplicate
            );
        }
        let _ = writeln!(self.writer, "  Files failed:    {}", stats.files_failed);

        if stats.files_converted > 0 {
            let _ = writeln!(
                self.writer,
                "  Total original size:  {}",
                cli_tools::format_size(stats.total_original_size)
            );
            let _ = writeln!(
                self.writer,
                "  Total converted size: {}",
                cli_tools::format_size(stats.total_converted_size)
            );

            let saved = stats.space_saved();
            if saved >= 0 {
                let _ = writeln!(self.writer, "  Space saved: {}", cli_tools::format_size(saved as u64));
            } else {
                let _ = writeln!(
                    self.writer,
                    "  Space increased: {}",
                    cli_tools::format_size((-saved) as u64)
                );
            }
        }

        let _ = writeln!(
            self.writer,
            "  Total time: {}",
            cli_tools::format_duration(stats.total_duration)
        );
        let _ = writeln!(self.writer, "[{}] END", Self::timestamp());
        let _ = writeln!(self.writer);
        let _ = self.writer.flush();
    }
}

impl RunStats {
    fn add_result(&mut self, result: &ProcessResult, duration: Duration) {
        self.total_duration += duration;
        match result {
            ProcessResult::Converted { stats, .. } => {
                self.files_converted += 1;
                *self += *stats;
            }
            ProcessResult::Remuxed { .. } => {
                self.files_remuxed += 1;
            }
            ProcessResult::Skipped(reason) => match reason {
                SkipReason::AlreadyConverted => self.files_skipped_converted += 1,
                SkipReason::BitrateBelowThreshold { .. } => self.files_skipped_bitrate += 1,
                SkipReason::OutputExists { .. } => self.files_skipped_duplicate += 1,
            },
            ProcessResult::Failed { .. } => {
                self.files_failed += 1;
            }
        }
    }

    const fn total_skipped(&self) -> usize {
        self.files_skipped_converted + self.files_skipped_bitrate + self.files_skipped_duplicate
    }

    #[allow(clippy::cast_possible_wrap)]
    const fn space_saved(&self) -> i64 {
        self.total_original_size as i64 - self.total_converted_size as i64
    }

    fn print_summary(&self) {
        println!("{}", "\n--- Conversion Summary ---".bold().magenta());
        println!("Files converted:        {}", self.files_converted);
        println!("Files remuxed:          {}", self.files_remuxed);
        println!(
            "Files failed:           {}",
            if self.files_failed > 0 {
                self.files_failed.to_string().red()
            } else {
                "0".normal()
            }
        );
        println!("Files skipped:          {}", self.total_skipped());
        if self.total_skipped() > 0 {
            println!("  - Already converted:  {}", self.files_skipped_converted);
            println!("  - Below bitrate:      {}", self.files_skipped_bitrate);
            println!("  - Duplicates:         {}", self.files_skipped_duplicate);
        }
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
                let saved = self.space_saved();
                let ratio = saved.abs() as f64 / self.total_original_size as f64 * 100.0;

                if saved >= 0 {
                    println!(
                        "Space saved:            {} ({:.1}%)",
                        cli_tools::format_size(saved as u64),
                        ratio
                    );
                } else {
                    println!(
                        "Space increased:        {} ({:.1}%)",
                        cli_tools::format_size((-saved) as u64),
                        ratio
                    );
                }
            }
        }

        println!(
            "Total time:             {}",
            cli_tools::format_duration(self.total_duration)
        );
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
        let name = cli_tools::path_to_file_stem_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }

    pub fn from_dir_entry(entry: walkdir::DirEntry) -> Self {
        let path = entry.into_path();
        let name = cli_tools::path_to_file_stem_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
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

impl std::fmt::Display for ConversionStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -> {} ({:.1}%)",
            cli_tools::format_size(self.original_size),
            cli_tools::format_size(self.converted_size),
            self.change_percentage()
        )
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

impl std::ops::AddAssign<ConversionStats> for RunStats {
    fn add_assign(&mut self, stats: ConversionStats) {
        self.total_original_size += stats.original_size;
        self.total_converted_size += stats.converted_size;
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
            bitrate_limit: user_config.bitrate.unwrap_or(args.bitrate),
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
        let logger = RefCell::new(FileLogger::new()?);

        Ok(Self { config, logger })
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
            .map(VideoFile::from_dir_entry)
            .filter(|file| self.should_include_file(file))
            .collect();

        files.sort();
        Ok(files)
    }

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
                    println!(
                        "[DRYRUN] Would rename: {} -> {}",
                        cli_tools::path_to_string_relative(&file.path),
                        cli_tools::path_to_string_relative(&new_path)
                    );
                } else if let Err(e) = std::fs::rename(&file.path, &new_path) {
                    print_warning!("Failed to rename file: {e}");
                } else {
                    println!(
                        "Renamed: {} -> {}",
                        cli_tools::path_to_string_relative(&file.path),
                        cli_tools::path_to_string_relative(&new_path)
                    );
                }
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

        let output_path = Self::get_output_path(file);

        // Check if output already exists
        if output_path.exists() && !self.config.overwrite {
            return ProcessResult::Skipped(SkipReason::OutputExists { path: output_path });
        }

        if is_hevc {
            println!(
                "{}",
                format!(
                    "{file_index} Remuxing: {}",
                    cli_tools::path_to_string_relative(&file.path)
                )
                .bold()
                .magenta()
            );
            println!("{info}");
            self.remux_to_mp4(&file.path, &output_path, &info, file_index)
        } else {
            println!(
                "{}",
                format!(
                    "{file_index} Converting: {}",
                    cli_tools::path_to_string_relative(&file.path)
                )
                .bold()
                .magenta()
            );
            println!("{info}");
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
        if self.config.verbose {
            println!("Converting to HEVC: {}", cli_tools::path_to_string_relative(output));
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

    /// Get output path for the new file
    fn get_output_path(file: &VideoFile) -> PathBuf {
        let parent = file.path.parent().unwrap_or_else(|| Path::new("."));
        let new_name = format!("{}.x265.mp4", file.name);
        parent.join(new_name)
    }

    /// Handle the original file after successful processing
    fn delete_original_file(&self, path: &Path) -> Result<()> {
        // Use direct delete if configured or if on a network drive (trash doesn't work there)
        if self.config.delete || Self::is_network_path(path) {
            println!("Deleting: {}", path.display());
            std::fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            println!("Trashing: {}", path.display());
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
    }

    /// Check if a path is on a network drive (Windows only).
    /// Uses Windows API to detect mapped network drives and UNC paths.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn is_network_path(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

        const DRIVE_REMOTE: u32 = 4;

        // Check for UNC paths (\\server\share)
        let path_str = path.to_string_lossy();
        if path_str.starts_with(r"\\") {
            return true;
        }

        // Check drive type for mapped network drives
        if let Some(prefix) = path.components().next() {
            let prefix_str = prefix.as_os_str();
            // Create a root path like "X:\"
            let mut root: Vec<u16> = prefix_str.encode_wide().collect();
            if root.len() >= 2 && root[1] == u16::from(b':') {
                root.push(u16::from(b'\\'));
                root.push(0); // null terminator

                // SAFETY: GetDriveTypeW is a safe Windows API call that only reads
                // the null-terminated string to determine drive type
                let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
                return drive_type == DRIVE_REMOTE;
            }
        }

        false
    }

    /// Check if a path is on a network drive (non-Windows: always returns false)
    #[cfg(not(windows))]
    fn is_network_path(_path: &Path) -> bool {
        false
    }
}

/// Run a command in a new process group to prevent Ctrl+C from propagating to it.
/// This allows the main program to handle the signal and finish the current file gracefully.
#[cfg(windows)]
fn run_command_isolated(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

#[cfg(unix)]
fn run_command_isolated(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt;
    // Set process group to 0 to prevent SIGINT propagation
    cmd.process_group(0)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(ref shell) = args.completion {
        cli_tools::generate_shell_completion(*shell, Args::command(), true, env!("CARGO_BIN_NAME"))
    } else {
        VideoConvert::new(args)?.run()
    }
}
