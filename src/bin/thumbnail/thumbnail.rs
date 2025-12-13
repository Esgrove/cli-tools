use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use cli_tools::{print_error, print_warning};
use colored::Colorize;
use walkdir::WalkDir;

use crate::ThumbnailArgs;
use crate::config::Config;

/// Supported video file extensions.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v"];

/// Default font file path for macOS.
#[cfg(target_os = "macos")]
const DEFAULT_FONT_FILE: &str = "/System/Library/Fonts/Supplemental/Arial.ttf";

/// Default font file path for Windows.
#[cfg(target_os = "windows")]
const DEFAULT_FONT_FILE: &str = "C:/Windows/Fonts/arial.ttf";

/// Default font file path for Linux.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DEFAULT_FONT_FILE: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";

/// Maximum length for metadata text in the thumbnail.
const MAX_METADATA_LENGTH: usize = 128;

/// Name of the output directory for thumbnails.
const SCREENS_DIR_NAME: &str = "Screens";

/// Thumbnail creator that processes video files and creates thumbnail sheets.
pub struct ThumbnailCreator {
    config: Config,
    root: PathBuf,
    /// Pre-escaped font path for ffmpeg drawtext filter.
    escaped_font: String,
    /// Pre-computed quality string for ffmpeg.
    quality_str: String,
}

/// Parameters for creating a thumbnail.
#[derive(Debug)]
struct ThumbnailParams {
    /// Interval between frames in seconds.
    interval: f64,
    /// Number of columns in the grid.
    cols: u32,
    /// Number of rows in the grid.
    rows: u32,
    /// Padding between tiles in pixels.
    padding: u32,
    /// Font size for text overlays.
    font_size: u32,
    /// Metadata text to display.
    metadata_text: String,
}

/// Information about a video file from ffprobe.
#[derive(Debug)]
struct VideoInfo {
    /// Video width in pixels.
    width: Option<u32>,
    /// Video height in pixels.
    height: Option<u32>,
    /// Duration in seconds.
    duration: Option<f64>,
    /// Video codec name.
    codec: Option<String>,
    /// Video bitrate in kbps.
    bitrate_kbps: Option<u64>,
}

impl ThumbnailCreator {
    /// Create a new thumbnail creator from command line arguments.
    pub fn new(args: &ThumbnailArgs) -> Result<Self> {
        Self::check_dependencies()?;

        let input_path = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args);
        let escaped_font = Self::escape_for_drawtext(DEFAULT_FONT_FILE);
        let quality_str = config.quality.to_string();

        Ok(Self {
            config,
            root: input_path,
            escaped_font,
            quality_str,
        })
    }

    /// Run the thumbnail creation process.
    pub fn run(&self) -> Result<()> {
        let video_files = self.gather_video_files()?;

        if video_files.is_empty() {
            print_warning!("No video files found in: {}", self.root.display());
            return Ok(());
        }

        println!(
            "{}",
            format!("Found {} video file(s)", video_files.len()).green().bold()
        );

        let mut success_count = 0;
        let mut error_count = 0;

        for video_file in &video_files {
            match self.create_thumbnail(video_file) {
                Ok(()) => success_count += 1,
                Err(e) => {
                    print_error!("Failed to create thumbnail for {}: {e}", video_file.display());
                    error_count += 1;
                }
            }
        }

        println!(
            "Finished: {} successful, {} failed",
            success_count.to_string().green(),
            error_count.to_string().red()
        );

        Ok(())
    }

    /// Check that required dependencies (ffmpeg, ffprobe) are available.
    fn check_dependencies() -> Result<()> {
        let ffprobe_check = Command::new("ffprobe").arg("-version").output();
        if ffprobe_check.is_err() {
            anyhow::bail!("ffprobe not found. Install ffmpeg first and make sure it is in PATH");
        }

        let ffmpeg_check = Command::new("ffmpeg").arg("-version").output();
        if ffmpeg_check.is_err() {
            anyhow::bail!("ffmpeg not found. Install ffmpeg first and make sure it is in PATH");
        }

        Ok(())
    }

    /// Gather all video files from the input path.
    fn gather_video_files(&self) -> Result<Vec<PathBuf>> {
        let mut video_files = Vec::new();

        if self.root.is_file() {
            if Self::is_video_file(&self.root) {
                video_files.push(self.root.clone());
            } else {
                anyhow::bail!("File '{}' is not a supported video file", self.root.display());
            }
        } else if self.root.is_dir() {
            if self.config.recurse {
                println!(
                    "{}",
                    format!("Searching recursively for video files in: {}", self.root.display()).magenta()
                );
                for entry in WalkDir::new(&self.root)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                {
                    let path = entry.path().to_path_buf();
                    if Self::is_video_file(&path) {
                        video_files.push(path);
                    }
                }
            } else {
                println!(
                    "{}",
                    format!("Searching for video files in: {}", self.root.display()).magenta()
                );
                for entry in std::fs::read_dir(&self.root)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() && Self::is_video_file(&path) {
                        video_files.push(path);
                    }
                }
            }
        } else {
            anyhow::bail!("Path '{}' does not exist", self.root.display());
        }

        video_files.sort();
        Ok(video_files)
    }

    /// Check if a file is a video file based on its extension.
    fn is_video_file(path: &Path) -> bool {
        path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|video_ext| video_ext.eq_ignore_ascii_case(ext))
        })
    }

    /// Create a thumbnail for a single video file.
    fn create_thumbnail(&self, video_path: &Path) -> Result<()> {
        let filename = video_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Invalid filename")?;

        let file_stem = video_path
            .file_stem()
            .and_then(|n| n.to_str())
            .context("Invalid file stem")?;

        let parent_dir = video_path.parent().context("No parent directory")?;

        let screens_dir = parent_dir.join(SCREENS_DIR_NAME);
        let output_path = screens_dir.join(format!("{file_stem}.jpg"));

        if output_path.exists() && !self.config.overwrite {
            print_warning!("Thumbnail already exists: {}", output_path.display());
            return Ok(());
        }

        println!("{}", format!("Creating thumbnail for: {filename}").magenta().bold());

        // Get video info
        let video_info = Self::get_video_info(video_path)?;

        if self.config.verbose {
            let mut info_parts = Vec::new();
            if let (Some(width), Some(height)) = (video_info.width, video_info.height) {
                info_parts.push(format!("resolution: {width}x{height}"));
            }
            if let Some(duration) = video_info.duration {
                info_parts.push(format!("duration: {duration:.2}s"));
            }
            if let Some(ref codec) = video_info.codec {
                info_parts.push(format!("codec: {codec}"));
            }
            if let Some(bitrate_kbps) = video_info.bitrate_kbps {
                info_parts.push(format!("bitrate: {:.2} Mbps", bitrate_kbps as f64 / 1000.0));
            }
            if !info_parts.is_empty() {
                println!("  {}", info_parts.join(", "));
            }
        }

        // Determine layout based on aspect ratio
        let is_landscape = match (video_info.width, video_info.height) {
            (Some(width), Some(height)) => width > height,
            // Default to landscape if dimensions unknown
            _ => true,
        };
        let (cols, rows, padding) = if is_landscape {
            (
                self.config.cols_landscape,
                self.config.rows_landscape,
                self.config.padding_landscape,
            )
        } else {
            (
                self.config.cols_portrait,
                self.config.rows_portrait,
                self.config.padding_portrait,
            )
        };

        let num_shots = cols * rows;
        let interval = match video_info.duration {
            Some(duration) if duration > 0.0 => duration / f64::from(num_shots),
            _ => 1.0,
        };

        if self.config.verbose {
            println!("  interval: {interval:.2}s");
        }

        // Calculate font size based on aspect ratio
        let font_size = self.calculate_font_size(&video_info);

        // Build metadata text
        let metadata_text = Self::build_metadata_text(filename, &video_info);

        // Create output directory
        if !self.config.dryrun {
            std::fs::create_dir_all(&screens_dir)?;
        }

        // Build ffmpeg command
        let params = ThumbnailParams {
            interval,
            cols,
            rows,
            padding,
            font_size,
            metadata_text,
        };
        let mut command = self.build_ffmpeg_command(video_path, &output_path, &params);

        if self.config.dryrun {
            println!("[DRYRUN] {command:#?}");
            return Ok(());
        }

        let output = command.output().context("Failed to execute ffmpeg")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ffmpeg failed: {}", stderr.trim());
        }

        if self.config.verbose {
            println!("  output: {}", output_path.display());
        }

        Ok(())
    }

    /// Get video information using ffprobe.
    fn get_video_info(video_path: &Path) -> Result<VideoInfo> {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,bit_rate,width,height:stream_tags=BPS,BPS-eng:format=bit_rate,duration",
                "-of",
                "default=nokey=0:noprint_wrappers=1",
            ])
            .arg(video_path)
            .output()
            .context("Failed to execute ffprobe")?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut codec: Option<String> = None;
        let mut bitrate_kbps: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;

        for line in stdout.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "codec_name" => codec = Some(value.to_lowercase()),
                    "bit_rate" | "BPS" | "BPS-eng" => {
                        if bitrate_kbps.is_none()
                            && let Ok(bps) = value.parse::<u64>()
                            && bps > 0
                        {
                            bitrate_kbps = Some(bps / 1000);
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

        // Print warnings for missing information
        let filename = video_path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");

        if width.is_none() || height.is_none() {
            print_warning!("Could not detect video resolution for: {filename}");
        }
        if duration.is_none() {
            print_warning!("Could not detect duration for: {filename}");
        }
        if codec.is_none() {
            print_warning!("Could not detect codec for: {filename}");
        }
        if bitrate_kbps.is_none() {
            print_warning!("Could not detect bitrate for: {filename}");
        }

        Ok(VideoInfo {
            width,
            height,
            duration,
            codec,
            bitrate_kbps,
        })
    }

    /// Calculate appropriate font size based on video aspect ratio.
    fn calculate_font_size(&self, video_info: &VideoInfo) -> u32 {
        let (Some(width), Some(height)) = (video_info.width, video_info.height) else {
            return self.config.font_size;
        };

        if width == 0 || height == 0 {
            return self.config.font_size;
        }

        let ratio = f64::from(width) / f64::from(height);

        if ratio < 0.75 {
            // Very vertical video
            36
        } else if ratio < 1.25 {
            // Square-ish video
            28
        } else {
            // Landscape video
            self.config.font_size
        }
    }

    /// Build metadata text for the thumbnail header.
    fn build_metadata_text(filename: &str, video_info: &VideoInfo) -> String {
        let mut parts = Vec::new();

        if let Some(duration) = video_info.duration {
            parts.push(cli_tools::format_duration_seconds(duration));
        }
        if let (Some(width), Some(height)) = (video_info.width, video_info.height) {
            parts.push(format!("{width}x{height}"));
        }
        if let Some(ref codec) = video_info.codec {
            parts.push(codec.clone());
        }
        if let Some(bitrate_kbps) = video_info.bitrate_kbps {
            parts.push(format!("{:.1} Mbps", bitrate_kbps as f64 / 1000.0));
        }
        parts.push(filename.to_string());

        let metadata = parts.join(" | ");

        // Crop if too long
        if metadata.len() > MAX_METADATA_LENGTH {
            format!("{}...", &metadata[..MAX_METADATA_LENGTH - 3])
        } else {
            metadata
        }
    }

    /// Build the ffmpeg command for creating a thumbnail.
    fn build_ffmpeg_command(&self, input_path: &Path, output_path: &Path, params: &ThumbnailParams) -> Command {
        let escaped_metadata = Self::escape_for_drawtext(&params.metadata_text);

        let filter = format!(
            "fps=1/{interval},\
            scale={width}:-1,\
            drawtext=fontfile='{font}':text='%{{pts\\:hms}}':x=10:y=h-th-10:\
            fontsize={font_size}:fontcolor=white:box=1:boxcolor=black@0.5:boxborderw=5,\
            tile={cols}x{rows}:margin=0:padding={padding},\
            drawtext=fontfile='{font}':\
            text='{metadata}':x=10:y=10:fontsize={font_size}:fontcolor=white:box=1:boxcolor=black@0.9:boxborderw=5",
            interval = params.interval,
            width = self.config.scale_width,
            font = self.escaped_font,
            font_size = params.font_size,
            cols = params.cols,
            rows = params.rows,
            padding = params.padding,
            metadata = escaped_metadata,
        );

        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-nostats", "-loglevel", "warning", "-nostdin", "-y"])
            .arg("-i")
            .arg(input_path)
            .arg("-vf")
            .arg(&filter)
            .args(["-frames:v", "1"])
            .arg("-q:v")
            .arg(&self.quality_str)
            .args(["-update", "1"])
            .arg(output_path);

        command
    }

    /// Escape text for ffmpeg drawtext filter.
    fn escape_for_drawtext(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace(':', "\\:")
            .replace('\'', "\\'")
            .replace('|', "\\|")
    }
}
