//! Video thumbnail sheet discovery, metadata formatting, and ffmpeg command construction.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use cli_tools::print_error;
use cli_tools::print_yellow;
use cli_tools::video_info::{VideoInfo, VideoStats};
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

impl ThumbnailCreator {
    /// Create a new thumbnail creator from command line arguments.
    pub fn new(args: &ThumbnailArgs) -> Result<Self> {
        Self::check_dependencies()?;

        let input_path = cli_tools::resolve_input_path(args.path.as_deref())?;
        let config = Config::from_args(args)?;
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
            print_yellow!("No video files found in: {}", self.root.display());
            return Ok(());
        }

        println!(
            "{}",
            format!(
                "Found {}",
                cli_tools::count_label(video_files.len(), "video file", "video files")
            )
            .green()
            .bold()
        );

        let mut success_count = 0;
        let mut error_count = 0;
        let mut stats = VideoStats::new();

        let total_count = video_files.len();
        let progress_width = total_count.to_string().len();

        for (index, video_file) in video_files.iter().enumerate() {
            let progress_prefix = Self::format_progress_prefix(index + 1, total_count, progress_width);
            match self.create_thumbnail(video_file, &mut stats, &progress_prefix) {
                Ok(()) => success_count += 1,
                Err(e) => {
                    print_error!(
                        "Failed to create thumbnail for {progress_prefix} {}: {e}",
                        video_file.display()
                    );
                    error_count += 1;
                }
            }
        }

        println!(
            "Finished: {} successful, {} failed",
            success_count.to_string().green(),
            error_count.to_string().red()
        );

        stats.print_summary(self.config.verbose);

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
                    .filter_entry(|e| !cli_tools::should_skip_entry(e))
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
    fn create_thumbnail(&self, video_path: &Path, stats: &mut VideoStats, progress_prefix: &str) -> Result<()> {
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
            print_yellow!(
                "Thumbnail already exists for {progress_prefix} {filename}: {}",
                output_path.display()
            );
            return Ok(());
        }

        println!(
            "{}",
            format!("Creating thumbnail for: {progress_prefix} {filename}")
                .magenta()
                .bold()
        );

        // Get video info
        let video_info = VideoInfo::from_path(video_path)?;
        stats.add(&video_info);

        Self::print_missing_video_info_warnings(&video_info, filename, progress_prefix);
        self.print_verbose_video_info(&video_info);

        // Determine layout based on aspect ratio (default to landscape if dimensions unknown)
        let is_landscape = video_info.resolution.is_none_or(|r| r.is_landscape());
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

    /// Format a progress prefix with current index aligned to the total digit width.
    fn format_progress_prefix(index: usize, total: usize, width: usize) -> String {
        format!("[{index:>width$} / {total}]")
    }

    /// Print warnings for video metadata that could not be detected.
    fn print_missing_video_info_warnings(video_info: &VideoInfo, filename: &str, progress_prefix: &str) {
        if video_info.resolution.is_none() {
            print_yellow!("Could not detect video resolution for: {progress_prefix} {filename}");
        }
        if video_info.duration.is_none() {
            print_yellow!("Could not detect duration for: {progress_prefix} {filename}");
        }
        if video_info.codec.is_none() {
            print_yellow!("Could not detect codec for: {progress_prefix} {filename}");
        }
        if video_info.bitrate_kbps.is_none() {
            print_yellow!("Could not detect bitrate for: {progress_prefix} {filename}");
        }
    }

    /// Print detected video metadata when verbose output is enabled.
    fn print_verbose_video_info(&self, video_info: &VideoInfo) {
        if !self.config.verbose {
            return;
        }

        let mut info_parts = Vec::new();
        if let Some(resolution) = video_info.resolution {
            info_parts.push(format!("resolution: {resolution}"));
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

    /// Calculate appropriate font size based on video aspect ratio.
    fn calculate_font_size(&self, video_info: &VideoInfo) -> u32 {
        let Some(resolution) = video_info.resolution else {
            return self.config.font_size;
        };

        if resolution.width == 0 || resolution.height == 0 {
            return self.config.font_size;
        }

        let ratio = resolution.aspect_ratio();

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
        if let Some(resolution) = video_info.resolution {
            parts.push(resolution.to_string());
        }
        if let Some(ref codec) = video_info.codec {
            parts.push(codec.clone());
        }
        if let Some(bitrate_kbps) = video_info.bitrate_kbps {
            parts.push(format!("{:.1} Mbps", bitrate_kbps as f64 / 1000.0));
        }
        parts.push(filename.to_string());

        let mut metadata = parts.join(" | ");

        // Crop if too long
        if metadata.len() > MAX_METADATA_LENGTH {
            let truncate_at = metadata.floor_char_boundary(MAX_METADATA_LENGTH - 3);
            metadata.truncate(truncate_at);
            metadata.push_str("...");
        }

        metadata
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

#[cfg(test)]
mod thumbnail_progress_prefix_tests {
    use super::*;

    #[test]
    fn right_aligns_index_to_total_width() {
        assert_eq!(ThumbnailCreator::format_progress_prefix(12, 100, 3), "[ 12 / 100]");
        assert_eq!(ThumbnailCreator::format_progress_prefix(1, 100, 3), "[  1 / 100]");
    }
}

#[cfg(test)]
mod test_thumbnail_helpers {
    use cli_tools::Resolution;

    use super::*;

    fn config() -> Config {
        Config {
            cols_landscape: 3,
            cols_portrait: 4,
            dryrun: false,
            font_size: 20,
            overwrite: false,
            padding_landscape: 8,
            padding_portrait: 16,
            quality: 2,
            recurse: false,
            rows_landscape: 4,
            rows_portrait: 3,
            scale_width: 480,
            verbose: false,
        }
    }

    fn creator(root: PathBuf, config: Config) -> ThumbnailCreator {
        ThumbnailCreator {
            quality_str: config.quality.to_string(),
            config,
            root,
            escaped_font: "font.ttf".to_string(),
        }
    }

    #[test]
    fn recognizes_supported_video_extensions_case_insensitively() {
        assert!(ThumbnailCreator::is_video_file(Path::new("video.mp4")));
        assert!(ThumbnailCreator::is_video_file(Path::new("video.MKV")));
        assert!(!ThumbnailCreator::is_video_file(Path::new("notes.txt")));
        assert!(!ThumbnailCreator::is_video_file(Path::new("README")));
    }

    #[test]
    fn calculates_font_size_for_missing_vertical_square_and_landscape_resolutions() {
        let creator = creator(PathBuf::new(), config());
        assert_eq!(creator.calculate_font_size(&VideoInfo::default()), 20);
        assert_eq!(
            creator.calculate_font_size(&VideoInfo {
                resolution: Some(Resolution::new(0, 1080)),
                ..Default::default()
            }),
            20
        );
        assert_eq!(
            creator.calculate_font_size(&VideoInfo {
                resolution: Some(Resolution::new(720, 1280)),
                ..Default::default()
            }),
            36
        );
        assert_eq!(
            creator.calculate_font_size(&VideoInfo {
                resolution: Some(Resolution::new(1080, 1080)),
                ..Default::default()
            }),
            28
        );
        assert_eq!(
            creator.calculate_font_size(&VideoInfo {
                resolution: Some(Resolution::new(1920, 1080)),
                ..Default::default()
            }),
            20
        );
    }

    #[test]
    fn builds_metadata_in_display_order_and_truncates_long_ascii_text() {
        let info = VideoInfo {
            size_bytes: None,
            resolution: Some(Resolution::new(1920, 1080)),
            duration: Some(90.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(5_000),
        };

        assert_eq!(
            ThumbnailCreator::build_metadata_text("video.mp4", &info),
            "1m 30s | 1920x1080 | hevc | 5.0 Mbps | video.mp4"
        );
        assert_eq!(
            ThumbnailCreator::build_metadata_text("video.mp4", &VideoInfo::default()),
            "video.mp4"
        );

        let long_name = format!("{}.mp4", "a".repeat(MAX_METADATA_LENGTH));
        let truncated = ThumbnailCreator::build_metadata_text(&long_name, &VideoInfo::default());
        assert_eq!(truncated.len(), MAX_METADATA_LENGTH);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn escapes_all_drawtext_special_characters() {
        assert_eq!(
            ThumbnailCreator::escape_for_drawtext(r"C:\font:path's|name"),
            r"C\:\\font\:path\'s\|name"
        );
    }

    #[test]
    fn builds_expected_ffmpeg_command_without_executing_it() {
        let creator = creator(PathBuf::new(), config());
        let params = ThumbnailParams {
            interval: 10.0,
            cols: 3,
            rows: 4,
            padding: 8,
            font_size: 20,
            metadata_text: "video: sample".to_string(),
        };

        let command = creator.build_ffmpeg_command(Path::new("input.mp4"), Path::new("output.jpg"), &params);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "ffmpeg");
        assert!(arguments.contains(&"-nostdin".to_string()));
        assert!(arguments.contains(&"input.mp4".to_string()));
        assert!(arguments.contains(&"output.jpg".to_string()));
        assert!(arguments.contains(&"2".to_string()));
        assert!(arguments.iter().any(|argument| argument.contains("tile=3x4")));
        assert!(arguments.iter().any(|argument| argument.contains(r"video\: sample")));
    }
}

#[cfg(test)]
mod test_thumbnail_file_discovery {
    use super::*;

    fn config_with_recursion(recurse: bool) -> Config {
        Config {
            cols_landscape: 3,
            cols_portrait: 4,
            dryrun: false,
            font_size: 20,
            overwrite: false,
            padding_landscape: 8,
            padding_portrait: 16,
            quality: 2,
            recurse,
            rows_landscape: 4,
            rows_portrait: 3,
            scale_width: 480,
            verbose: false,
        }
    }

    fn creator(root: PathBuf, recurse: bool) -> ThumbnailCreator {
        ThumbnailCreator {
            config: config_with_recursion(recurse),
            root,
            escaped_font: "font.ttf".to_string(),
            quality_str: "2".to_string(),
        }
    }

    #[test]
    fn gathers_supported_single_file_and_rejects_unsupported_file() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let video = temp_directory.path().join("video.mp4");
        let text = temp_directory.path().join("notes.txt");
        std::fs::write(&video, b"video")?;
        std::fs::write(&text, b"notes")?;

        assert_eq!(creator(video.clone(), false).gather_video_files()?, vec![video]);
        assert!(creator(text, false).gather_video_files().is_err());
        Ok(())
    }

    #[test]
    fn recursion_controls_nested_discovery_and_hidden_directories_are_skipped() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let scan_root = temp_directory.path().join("videos");
        let nested = scan_root.join("nested");
        let hidden = scan_root.join(".hidden");
        std::fs::create_dir_all(&nested)?;
        std::fs::create_dir(&hidden)?;
        let top = scan_root.join("top.mp4");
        let nested_video = nested.join("nested.mkv");
        std::fs::write(&top, b"top")?;
        std::fs::write(&nested_video, b"nested")?;
        std::fs::write(hidden.join("ignored.mp4"), b"hidden")?;

        assert_eq!(
            creator(scan_root.clone(), false).gather_video_files()?,
            vec![top.clone()]
        );
        let mut expected_recursive = vec![top, nested_video];
        expected_recursive.sort();
        assert_eq!(creator(scan_root, true).gather_video_files()?, expected_recursive);
        Ok(())
    }

    #[test]
    fn missing_path_returns_error() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        assert!(
            creator(temp_directory.path().join("missing"), false)
                .gather_video_files()
                .is_err()
        );
    }

    #[test]
    fn existing_thumbnail_returns_before_video_probe() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let video = temp_directory.path().join("video.mp4");
        let screens = temp_directory.path().join(SCREENS_DIR_NAME);
        std::fs::write(&video, b"not a real video")?;
        std::fs::create_dir(&screens)?;
        std::fs::write(screens.join("video.jpg"), b"existing")?;
        let creator = creator(video.clone(), false);
        let mut stats = VideoStats::new();

        creator.create_thumbnail(&video, &mut stats, "[1 / 1]")?;

        assert_eq!(std::fs::read(screens.join("video.jpg"))?, b"existing");
        Ok(())
    }
}
