//! Video thumbnail sheet discovery, metadata formatting, and ffmpeg command construction.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use cli_tools_common::print_error;
use cli_tools_common::print_yellow;
use cli_tools_media::video_info::{VideoInfo, VideoStats};
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

        let input_path = cli_tools_common::resolve_input_path(args.path.as_deref())?;
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
                cli_tools_common::count_label(video_files.len(), "video file", "video files")
            )
            .green()
            .bold()
        );

        let mut success_count = 0;
        let mut error_count = 0;
        let mut stats = VideoStats::new();

        let total_count = video_files.len();
        let progress_width = total_count.to_string().len();

        let output_paths = self.output_paths(&video_files)?;
        for (index, (video_file, output_path)) in video_files.iter().zip(&output_paths).enumerate() {
            let progress_prefix = Self::format_progress_prefix(index + 1, total_count, progress_width);
            match self.create_thumbnail(video_file, output_path, &mut stats, &progress_prefix) {
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
            } else {
                println!(
                    "{}",
                    format!("Searching for video files in: {}", self.root.display()).magenta()
                );
            }
            let walker = if self.config.recurse {
                WalkDir::new(&self.root)
            } else {
                WalkDir::new(&self.root).max_depth(1)
            };
            for entry in walker
                .into_iter()
                .filter_entry(|entry| !cli_tools_common::should_skip_entry(entry))
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
            {
                let path = entry.path().to_path_buf();
                if Self::is_video_file(&path) {
                    video_files.push(path);
                }
            }
        } else {
            anyhow::bail!("Path '{}' does not exist", self.root.display());
        }

        video_files.sort();

        Ok(video_files)
    }

    /// Choose thumbnail paths based on all video siblings, even for a single file invocation.
    fn output_paths(&self, video_files: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let [video] = video_files else {
            return Ok(thumbnail_paths(video_files));
        };
        if self.root != *video {
            return Ok(thumbnail_paths(video_files));
        }

        let parent = video.parent().context("Video has no parent directory")?;
        let include_hidden = cli_tools_common::is_hidden_path(video);
        let mut siblings = vec![video.clone()];
        for entry in std::fs::read_dir(parent).with_context(|| format!("Reading {}", parent.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path != *video
                && Self::is_video_file(&path)
                && (include_hidden || !cli_tools_common::is_hidden_path(&path))
                && entry.file_type()?.is_file()
            {
                siblings.push(path);
            }
        }
        let mut paths = thumbnail_paths(&siblings);
        paths.truncate(1);
        Ok(paths)
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
    fn create_thumbnail(
        &self,
        video_path: &Path,
        output_path: &Path,
        stats: &mut VideoStats,
        progress_prefix: &str,
    ) -> Result<()> {
        let filename = video_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Invalid filename")?;

        let screens_dir = output_path.parent().context("No thumbnail directory")?;

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

        let params = self.thumbnail_params(filename, &video_info);

        if self.config.verbose {
            println!("  interval: {:.2}s", params.interval);
        }

        // Create output directory
        if !self.config.dryrun {
            std::fs::create_dir_all(screens_dir)?;
        }

        // The header holds the file name, which can contain any character,
        // so it is read from a file instead of being quoted into the filter graph.
        let text_file = Self::write_text_file(&params.metadata_text)?;
        let mut command = self.build_ffmpeg_command(video_path, output_path, &params, text_file.path());

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

    /// Choose the grid, frame interval, font size, and header text for one video.
    ///
    /// The landscape grid is used when the resolution is unknown,
    /// and a one second interval when the duration is unknown or zero.
    fn thumbnail_params(&self, filename: &str, video_info: &VideoInfo) -> ThumbnailParams {
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

        let num_shots = cols.saturating_mul(rows).max(1);
        let interval = match video_info.duration {
            Some(duration) if duration > 0.0 => duration / f64::from(num_shots),
            _ => 1.0,
        };

        ThumbnailParams {
            interval,
            cols,
            rows,
            padding,
            font_size: self.calculate_font_size(video_info),
            metadata_text: Self::build_metadata_text(filename, video_info),
        }
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
            parts.push(cli_tools_common::format_duration_seconds(duration));
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
    ///
    /// The header text is read from `text_file` with expansion turned off, so it is drawn exactly as written.
    fn build_ffmpeg_command(
        &self,
        input_path: &Path,
        output_path: &Path,
        params: &ThumbnailParams,
        text_file: &Path,
    ) -> Command {
        let escaped_text_file = Self::escape_for_drawtext(&text_file.to_string_lossy());

        let filter = format!(
            "fps=1/{interval},\
            scale={width}:-1,\
            drawtext=fontfile='{font}':text='%{{pts\\:hms}}':x=10:y=h-th-10:\
            fontsize={font_size}:fontcolor=white:box=1:boxcolor=black@0.5:boxborderw=5,\
            tile={cols}x{rows}:margin=0:padding={padding},\
            drawtext=fontfile='{font}':\
            textfile='{text_file}':expansion=none:x=10:y=10:fontsize={font_size}:fontcolor=white:box=1:boxcolor=black@0.9:boxborderw=5",
            interval = params.interval,
            width = self.config.scale_width,
            font = self.escaped_font,
            font_size = params.font_size,
            cols = params.cols,
            rows = params.rows,
            padding = params.padding,
            text_file = escaped_text_file,
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

    /// Write the header text to a temporary file that is removed when the returned handle is dropped.
    fn write_text_file(text: &str) -> Result<tempfile::NamedTempFile> {
        let mut file = tempfile::Builder::new()
            .prefix("thumbs-header")
            .suffix(".txt")
            .tempfile()
            .context("Failed to create header text file")?;
        file.write_all(text.as_bytes())
            .context("Failed to write header text file")?;
        Ok(file)
    }

    /// Escape a path or option value for placement inside single quotes in an ffmpeg drawtext filter.
    ///
    /// Filtergraph strings go through two `av_get_token` passes (graph splitting then option parsing).
    /// Inside single quotes, `\\` passes through level 1 and yields a literal `\` at level 2,
    /// `\:` yields a literal `:`, and `'\\\''` closes the level-1 quote, emits `\'` for level 2,
    /// and reopens the level-1 quote so a literal `'` survives both passes without breaking out.
    fn escape_for_drawtext(text: &str) -> String {
        text.replace('\\', "\\\\")
            .replace(':', "\\:")
            .replace('\'', r"'\\\''")
            .replace('|', "\\|")
    }
}

/// Thumbnail path for each video, in the same order.
///
/// A thumbnail is named after the video stem unless that name collides with another video's thumbnail
/// in the same directory (case-insensitively), in which case the colliding videos keep their full file names.
fn thumbnail_paths(video_files: &[PathBuf]) -> Vec<PathBuf> {
    let mut use_full_name = vec![false; video_files.len()];
    loop {
        let mut counts: HashMap<(PathBuf, String), usize> = HashMap::new();
        for (video, full_name) in video_files.iter().zip(&use_full_name) {
            let parent = video.parent().map_or_default(Path::to_path_buf);
            let candidate = if *full_name {
                video.file_name().unwrap_or_default().to_string_lossy().to_lowercase()
            } else {
                video.file_stem().unwrap_or_default().to_string_lossy().to_lowercase()
            };
            *counts.entry((parent, candidate)).or_default() += 1;
        }

        let mut promoted = false;
        for (video, full_name) in video_files.iter().zip(&mut use_full_name) {
            if *full_name {
                continue;
            }
            let parent = video.parent().map_or_default(Path::to_path_buf);
            let candidate = video.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
            if counts.get(&(parent, candidate)).is_some_and(|count| *count > 1) {
                *full_name = true;
                promoted = true;
            }
        }
        if !promoted {
            break;
        }
    }

    video_files
        .iter()
        .zip(&use_full_name)
        .map(|(video, full_name)| {
            let parent = video.parent().unwrap_or_else(|| Path::new(""));
            let name = if *full_name {
                video.file_name().unwrap_or_default().to_string_lossy()
            } else {
                video.file_stem().unwrap_or_default().to_string_lossy()
            };
            parent.join(SCREENS_DIR_NAME).join(format!("{name}.jpg"))
        })
        .collect()
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
    use cli_tools_media::Resolution;

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
            r"C\:\\font\:path'\\\''s\|name"
        );
    }

    #[test]
    fn renders_thumbnail_when_textfile_path_contains_apostrophe_comma_and_colon() {
        if !cli_tools_common::command_exists("ffmpeg") {
            return;
        }
        let filters = Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
            .ok()
            .map_or_default(|output| String::from_utf8_lossy(&output.stdout).into_owned());
        if !filters.contains("drawtext") {
            return;
        }

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let tricky_dir = temp_dir.path().join("O'Brien,Jr;a:b");
        std::fs::create_dir_all(&tricky_dir).expect("create tricky dir");
        let video_path = temp_dir.path().join("clip.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=128x128:d=1",
                "-c:v",
                "libx264",
                "-y",
            ])
            .arg(&video_path)
            .status()
            .expect("generate clip");
        assert!(status.success(), "ffmpeg should generate tiny test clip");

        let text_file = tricky_dir.join("header's,1.txt");
        std::fs::write(&text_file, "clip.mp4\n128x128").expect("write header text");
        let output_path = temp_dir.path().join("out.jpg");

        let mut creator = creator(temp_dir.path().to_path_buf(), config());
        creator.escaped_font = ThumbnailCreator::escape_for_drawtext(DEFAULT_FONT_FILE);
        let params = ThumbnailParams {
            interval: 1.0,
            cols: 1,
            rows: 1,
            padding: 0,
            font_size: 16,
            metadata_text: "clip.mp4".to_string(),
        };

        let mut command = creator.build_ffmpeg_command(&video_path, &output_path, &params, &text_file);
        let output = command.output().expect("run ffmpeg drawtext");
        assert!(
            output.status.success(),
            "ffmpeg drawtext failed with tricky textfile path: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output_path.is_file(), "thumbnail jpg should be written");
    }

    #[test]
    fn header_text_never_appears_inside_the_filter_graph() {
        let creator = creator(PathBuf::new(), config());
        let params = ThumbnailParams {
            interval: 10.0,
            cols: 3,
            rows: 4,
            padding: 8,
            font_size: 20,
            metadata_text: "Don't, Stop [100%]; now.mp4".to_string(),
        };

        let command = creator.build_ffmpeg_command(
            Path::new("input.mp4"),
            Path::new("output.jpg"),
            &params,
            Path::new("header.txt"),
        );
        let filter = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .skip_while(|argument| argument != "-vf")
            .nth(1)
            .expect("the command should have a filter");

        assert!(
            !filter.contains("Stop"),
            "the header text is quoted into the filter: {filter}"
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

        let command = creator.build_ffmpeg_command(
            Path::new("input.mp4"),
            Path::new("output.jpg"),
            &params,
            Path::new("header.txt"),
        );
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
        assert!(
            arguments
                .iter()
                .any(|argument| argument.contains("textfile='header.txt'"))
        );
    }

    #[test]
    fn landscape_video_uses_landscape_grid_and_spreads_frames_over_duration() {
        let creator = creator(PathBuf::new(), config());
        let info = VideoInfo {
            resolution: Some(Resolution::new(1920, 1080)),
            duration: Some(120.0),
            ..Default::default()
        };

        let params = creator.thumbnail_params("video.mp4", &info);

        assert_eq!((params.cols, params.rows, params.padding), (3, 4, 8));
        assert!((params.interval - 10.0).abs() < f64::EPSILON);
        assert_eq!(params.font_size, 20);
        assert!(params.metadata_text.ends_with("video.mp4"));
    }

    #[test]
    fn portrait_video_uses_portrait_grid() {
        let creator = creator(PathBuf::new(), config());
        let info = VideoInfo {
            resolution: Some(Resolution::new(720, 1280)),
            duration: Some(60.0),
            ..Default::default()
        };

        let params = creator.thumbnail_params("clip.mp4", &info);

        assert_eq!((params.cols, params.rows, params.padding), (4, 3, 16));
        assert!((params.interval - 5.0).abs() < f64::EPSILON);
        assert_eq!(params.font_size, 36);
    }

    #[test]
    fn unknown_resolution_and_duration_fall_back_to_landscape_and_one_second() {
        let creator = creator(PathBuf::new(), config());

        let unknown = creator.thumbnail_params("clip.mp4", &VideoInfo::default());
        let zero_duration = creator.thumbnail_params(
            "clip.mp4",
            &VideoInfo {
                duration: Some(0.0),
                ..Default::default()
            },
        );

        assert_eq!((unknown.cols, unknown.rows), (3, 4));
        assert!((unknown.interval - 1.0).abs() < f64::EPSILON);
        assert!((zero_duration.interval - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn prints_video_info_with_and_without_details() {
        let quiet = creator(PathBuf::new(), config());
        let verbose = creator(
            PathBuf::new(),
            Config {
                verbose: true,
                ..config()
            },
        );
        let full = VideoInfo {
            size_bytes: Some(1_000),
            resolution: Some(Resolution::new(1920, 1080)),
            duration: Some(90.0),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(5_000),
        };

        ThumbnailCreator::print_missing_video_info_warnings(&VideoInfo::default(), "clip.mp4", "[1 / 1]");
        ThumbnailCreator::print_missing_video_info_warnings(&full, "clip.mp4", "[1 / 1]");
        quiet.print_verbose_video_info(&full);
        verbose.print_verbose_video_info(&full);
        verbose.print_verbose_video_info(&VideoInfo::default());
    }
}

#[cfg(test)]
mod test_thumbnail_run {
    use super::*;

    fn creator(root: PathBuf) -> ThumbnailCreator {
        ThumbnailCreator {
            config: Config {
                cols_landscape: 3,
                cols_portrait: 4,
                dryrun: true,
                font_size: 20,
                overwrite: false,
                padding_landscape: 8,
                padding_portrait: 16,
                quality: 2,
                recurse: false,
                rows_landscape: 4,
                rows_portrait: 3,
                scale_width: 480,
                verbose: true,
            },
            root,
            escaped_font: "font.ttf".to_string(),
            quality_str: "2".to_string(),
        }
    }

    #[test]
    fn empty_directory_finishes_without_work() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;

        creator(temp_directory.path().to_path_buf()).run()
    }

    #[test]
    fn existing_thumbnails_are_skipped_for_every_video() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let root = temp_directory.path().join("videos");
        let screens = root.join(SCREENS_DIR_NAME);
        std::fs::create_dir_all(&screens)?;
        for stem in ["first", "second"] {
            std::fs::write(root.join(format!("{stem}.mp4")), b"not a real video")?;
            std::fs::write(screens.join(format!("{stem}.jpg")), b"existing")?;
        }

        creator(root).run()?;

        assert_eq!(std::fs::read(screens.join("first.jpg"))?, b"existing");
        assert_eq!(std::fs::read(screens.join("second.jpg"))?, b"existing");
        Ok(())
    }

    #[test]
    fn separate_single_file_runs_use_distinct_paths_for_same_stem_siblings() {
        let directory = tempfile::tempdir().unwrap();
        let mp4 = directory.path().join("Film.mp4");
        let mkv = directory.path().join("Film.mkv");
        let dotted_stem = directory.path().join("Film.mp4.mkv");
        for video in [&mp4, &mkv, &dotted_stem] {
            std::fs::write(video, []).unwrap();
        }

        let directory_run = creator(directory.path().to_path_buf());
        let videos = directory_run.gather_video_files().unwrap();
        let directory_paths = directory_run.output_paths(&videos).unwrap();
        assert_eq!(directory_paths.len(), 3);
        for (video, path) in videos.into_iter().zip(directory_paths) {
            let file_run = creator(video.clone());
            let selected = file_run.gather_video_files().unwrap();
            let isolated_paths = file_run.output_paths(&selected).unwrap();
            let name = video.file_name().unwrap().to_str().unwrap();
            assert_eq!(path, directory.path().join(format!("Screens/{name}.jpg")));
            assert_eq!(isolated_paths, [path]);
        }
    }

    #[test]
    fn missing_root_is_an_error() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");

        assert!(creator(temp_directory.path().join("missing")).run().is_err());
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

        creator.create_thumbnail(&video, &screens.join("video.jpg"), &mut stats, "[1 / 1]")?;

        assert_eq!(std::fs::read(screens.join("video.jpg"))?, b"existing");
        Ok(())
    }

    #[test]
    fn non_recursive_discovery_skips_hidden_files_and_symlinks() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let visible = temp_directory.path().join("visible.mp4");
        let hidden = temp_directory.path().join(".hidden.mp4");
        std::fs::write(&visible, b"visible")?;
        std::fs::write(&hidden, b"hidden")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&visible, temp_directory.path().join("link.mp4"))?;

        let found = creator(temp_directory.path().to_path_buf(), false).gather_video_files()?;
        assert_eq!(found, vec![visible]);
        Ok(())
    }
}

#[cfg(test)]
mod test_thumbnail_paths {
    use super::*;

    #[test]
    fn avoids_collision_between_fallback_full_name_and_nested_extension_stem() {
        let videos = vec![
            PathBuf::from("dir/a.mp4"),
            PathBuf::from("dir/a.mkv"),
            PathBuf::from("dir/a.mp4.mkv"),
        ];

        let paths = thumbnail_paths(&videos);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("dir/Screens/a.mp4.jpg"),
                PathBuf::from("dir/Screens/a.mkv.jpg"),
                PathBuf::from("dir/Screens/a.mp4.mkv.jpg"),
            ]
        );
    }

    #[test]
    fn videos_sharing_a_stem_get_separate_thumbnails() {
        let videos = vec![
            PathBuf::from("/videos/a.mp4"),
            PathBuf::from("/videos/A.mkv"),
            PathBuf::from("/videos/b.mp4"),
            PathBuf::from("/other/a.mp4"),
        ];

        let paths = thumbnail_paths(&videos);

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/videos/Screens/a.mp4.jpg"),
                PathBuf::from("/videos/Screens/A.mkv.jpg"),
                PathBuf::from("/videos/Screens/b.jpg"),
                PathBuf::from("/other/Screens/a.jpg"),
            ]
        );
    }
}
