use std::fs;
use std::path::{Path, PathBuf};

use crate::convert::RE_X265;
use crate::convert::TARGET_EXTENSION;
use crate::stats::ConversionStats;

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

/// Filter options for video file analysis.
#[derive(Debug, Clone, Copy)]
pub struct AnalysisFilter {
    /// Minimum bitrate threshold in kbps.
    pub(crate) min_bitrate: u64,
    /// Maximum bitrate threshold in kbps.
    pub(crate) max_bitrate: Option<u64>,
    /// Minimum duration threshold in seconds.
    pub(crate) min_duration: Option<f64>,
    /// Maximum duration threshold in seconds.
    pub(crate) max_duration: Option<f64>,
    /// Whether to overwrite existing output files.
    pub(crate) overwrite: bool,
}

/// A video file with its path and parsed name components.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VideoFile {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) extension: String,
}

/// A video file with its analyzed info, ready for processing.
#[derive(Debug)]
pub struct ProcessableFile {
    pub(crate) file: VideoFile,
    pub(crate) info: VideoInfo,
    pub(crate) output_path: PathBuf,
}

/// Output from the analysis phase.
pub struct AnalysisOutput {
    /// Files that need full conversion (non-HEVC to HEVC).
    pub(crate) conversions: Vec<ProcessableFile>,
    /// Files that need remuxing (HEVC but wrong container).
    pub(crate) remuxes: Vec<ProcessableFile>,
    /// Files that need to be renamed (HEVC MP4 without .x265 suffix).
    pub(crate) renames: Vec<VideoFile>,
}

/// Reasons why a file was skipped
#[derive(Debug)]
pub enum SkipReason {
    /// File is already HEVC in MP4 container
    AlreadyConverted,
    /// File bitrate is below the minimum threshold
    BitrateBelowThreshold { bitrate: u64, threshold: u64 },
    /// File bitrate is above the maximum threshold
    BitrateAboveThreshold { bitrate: u64, threshold: u64 },
    /// File duration is below the minimum threshold
    DurationBelowThreshold { duration: f64, threshold: f64 },
    /// File duration is above the maximum threshold
    DurationAboveThreshold { duration: f64, threshold: f64 },
    /// Output file already exists
    OutputExists { path: PathBuf, source_duration: f64 },
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

/// Result of analyzing a video file to determine what action to take.
#[derive(Debug)]
pub enum AnalysisResult {
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

impl ProcessResult {
    /// A successful conversion result with size statistics.
    pub(crate) const fn converted(
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
    pub(crate) fn new(path: &Path) -> Self {
        let path = path.to_owned();
        let name = cli_tools::path_to_file_stem_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self { path, name, extension }
    }

    /// Get the output path for the converted file.
    pub(crate) fn output_path(&self) -> PathBuf {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        // Only add .x265 suffix if the filename doesn't already contain x265
        let new_name = if RE_X265.is_match(&self.name) {
            format!("{}.{TARGET_EXTENSION}", self.name)
        } else {
            format!("{}.x265.{TARGET_EXTENSION}", self.name)
        };
        parent.join(new_name)
    }
}

impl VideoInfo {
    /// Parse `VideoInfo` from ffprobe output.
    pub(crate) fn from_ffprobe_output(stdout: &str, stderr: &str, path: &Path) -> anyhow::Result<Self> {
        let mut codec = String::new();
        let mut bitrate_kbps: Option<u64> = None;
        let mut size_bytes: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;
        let mut frames_per_second: Option<f64> = None;

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
        for line in stdout.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "codec_name" => codec = value.to_lowercase(),
                    "bit_rate" | "BPS" | "BPS-eng" => {
                        if bitrate_kbps.is_none()
                            && let Ok(bps) = value.parse::<u64>()
                            && bps > 0
                        {
                            bitrate_kbps = Some(bps / 1000);
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

        // Validate required fields
        if codec.is_empty() {
            anyhow::bail!("failed to detect video codec");
        }
        let Some(bitrate_kbps) = bitrate_kbps else {
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

        // Fall back to file metadata for size if not in ffprobe output
        let size_bytes = size_bytes.unwrap_or_else(|| fs::metadata(path).map(|m| m.len()).unwrap_or(0));

        Ok(Self {
            codec,
            bitrate_kbps,
            size_bytes,
            duration,
            width,
            height,
            frames_per_second,
            warning,
        })
    }

    /// Determine quality level based on resolution and bitrate.
    /// Quality level 1 to 51, lower is better quality and bigger file size.
    pub(crate) fn quality_level(&self) -> u8 {
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
            Self::BitrateAboveThreshold { bitrate, threshold } => {
                write!(f, "Bitrate {bitrate} kbps is above threshold {threshold} kbps")
            }
            Self::DurationBelowThreshold { duration, threshold } => {
                write!(f, "Duration {duration:.1}s is below threshold {threshold:.1}s")
            }
            Self::DurationAboveThreshold { duration, threshold } => {
                write!(f, "Duration {duration:.1}s is above threshold {threshold:.1}s")
            }
            Self::OutputExists { path, .. } => {
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

#[cfg(test)]
mod video_file_tests {
    use super::*;

    #[test]
    fn new_extracts_name_and_extension() {
        let file = VideoFile::new(Path::new("/path/to/video.mp4"));
        assert_eq!(file.name, "video");
        assert_eq!(file.extension, "mp4");
    }

    #[test]
    fn new_handles_no_extension() {
        let file = VideoFile::new(Path::new("/path/to/video"));
        assert_eq!(file.name, "video");
        assert_eq!(file.extension, "");
    }

    #[test]
    fn output_path_adds_x265_suffix() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"));
        let output = file.output_path();
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mp4"));
    }

    #[test]
    fn output_path_preserves_existing_x265() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"));
        let output = file.output_path();
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mp4"));
    }

    #[test]
    fn display_shows_path() {
        let file = VideoFile::new(Path::new("/path/to/video.mp4"));
        assert_eq!(format!("{file}"), "/path/to/video.mp4");
    }
}

#[cfg(test)]
mod video_info_tests {
    use super::*;

    fn sample_ffprobe_output() -> &'static str {
        "codec_name=h264\n\
         bit_rate=7345573\n\
         duration=120.5\n\
         size=110000000\n\
         width=1920\n\
         height=1080\n\
         r_frame_rate=30/1\n"
    }

    #[test]
    fn from_ffprobe_output_parses_correctly() {
        let info = VideoInfo::from_ffprobe_output(sample_ffprobe_output(), "", Path::new("test.mp4")).unwrap();
        assert_eq!(info.codec, "h264");
        assert_eq!(info.bitrate_kbps, 7345);
        assert!((info.duration - 120.5).abs() < 0.01);
        assert_eq!(info.size_bytes, 110_000_000);
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert!((info.frames_per_second - 30.0).abs() < 0.01);
        assert!(info.warning.is_none());
    }

    #[test]
    fn from_ffprobe_output_captures_warning() {
        let info =
            VideoInfo::from_ffprobe_output(sample_ffprobe_output(), "some warning", Path::new("test.mp4")).unwrap();
        assert_eq!(info.warning, Some("some warning".to_string()));
    }

    #[test]
    fn from_ffprobe_output_fails_without_codec() {
        let output = "bit_rate=7345573\nduration=120.5\nwidth=1920\nheight=1080\nr_frame_rate=30/1\n";
        let result = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("codec"));
    }

    #[test]
    fn from_ffprobe_output_fails_without_bitrate() {
        let output = "codec_name=h264\nduration=120.5\nwidth=1920\nheight=1080\nr_frame_rate=30/1\n";
        let result = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bitrate"));
    }

    #[test]
    fn from_ffprobe_output_fails_without_duration() {
        let output = "codec_name=h264\nbit_rate=7345573\nwidth=1920\nheight=1080\nr_frame_rate=30/1\n";
        let result = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duration"));
    }

    #[test]
    fn from_ffprobe_output_parses_fractional_framerate() {
        let output = "codec_name=h264\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      r_frame_rate=30000/1001\n";
        let info = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4")).unwrap();
        assert!((info.frames_per_second - 29.97).abs() < 0.01);
    }

    #[test]
    fn quality_level_4k_high_bitrate() {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 28000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            warning: None,
        };
        assert_eq!(info.quality_level(), 30);
    }

    #[test]
    fn quality_level_4k_medium_bitrate() {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 20000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            warning: None,
        };
        assert_eq!(info.quality_level(), 31);
    }

    #[test]
    fn quality_level_4k_low_bitrate() {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_000_000_000,
            duration: 3600.0,
            width: 3840,
            height: 2160,
            frames_per_second: 30.0,
            warning: None,
        };
        assert_eq!(info.quality_level(), 33);
    }

    #[test]
    fn quality_level_1080p_high_bitrate() {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 18000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 30.0,
            warning: None,
        };
        assert_eq!(info.quality_level(), 28);
    }

    #[test]
    fn quality_level_1080p_low_bitrate() {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 5000,
            size_bytes: 500_000_000,
            duration: 3600.0,
            width: 1920,
            height: 1080,
            frames_per_second: 30.0,
            warning: None,
        };
        assert_eq!(info.quality_level(), 31);
    }

    #[test]
    fn display_formats_correctly() {
        let info = VideoInfo {
            codec: "hevc".to_string(),
            bitrate_kbps: 8000,
            size_bytes: 1_073_741_824,
            duration: 3661.5,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            warning: None,
        };
        let display = format!("{info}");
        assert!(display.contains("hevc"));
        assert!(display.contains("1.00 GB"));
        assert!(display.contains("8.00 Mbps"));
        assert!(display.contains("1920x1080"));
        assert!(display.contains("1h 01m 01s"));
    }
}

#[cfg(test)]
mod skip_reason_tests {
    use super::*;

    #[test]
    fn display_already_converted() {
        let reason = SkipReason::AlreadyConverted;
        assert_eq!(format!("{reason}"), "Already HEVC in MP4 container");
    }

    #[test]
    fn display_bitrate_below_threshold() {
        let reason = SkipReason::BitrateBelowThreshold {
            bitrate: 5000,
            threshold: 8000,
        };
        let display = format!("{reason}");
        assert!(display.contains("5000"));
        assert!(display.contains("8000"));
        assert!(display.contains("below"));
    }

    #[test]
    fn display_bitrate_above_threshold() {
        let reason = SkipReason::BitrateAboveThreshold {
            bitrate: 50000,
            threshold: 40000,
        };
        let display = format!("{reason}");
        assert!(display.contains("50000"));
        assert!(display.contains("40000"));
        assert!(display.contains("above"));
    }

    #[test]
    fn display_duration_below_threshold() {
        let reason = SkipReason::DurationBelowThreshold {
            duration: 30.0,
            threshold: 60.0,
        };
        let display = format!("{reason}");
        assert!(display.contains("30.0"));
        assert!(display.contains("60.0"));
    }

    #[test]
    fn display_duration_above_threshold() {
        let reason = SkipReason::DurationAboveThreshold {
            duration: 7200.0,
            threshold: 3600.0,
        };
        let display = format!("{reason}");
        assert!(display.contains("7200.0"));
        assert!(display.contains("3600.0"));
    }

    #[test]
    fn display_output_exists() {
        let reason = SkipReason::OutputExists {
            path: PathBuf::from("/path/to/output.mp4"),
            source_duration: 120.0,
        };
        let display = format!("{reason}");
        assert!(display.contains("output.mp4"));
        assert!(display.contains("already exists"));
    }

    #[test]
    fn display_analysis_failed() {
        let reason = SkipReason::AnalysisFailed {
            error: "ffprobe failed".to_string(),
        };
        let display = format!("{reason}");
        assert!(display.contains("ffprobe failed"));
    }
}

#[cfg(test)]
mod process_result_tests {
    use super::*;

    #[test]
    fn converted_creates_stats() {
        let result = ProcessResult::converted(1_000_000, 8000, 500_000, 4000);
        match result {
            ProcessResult::Converted { stats } => {
                let display = format!("{stats}");
                assert!(display.contains("8.00 Mbps"));
                assert!(display.contains("4.00 Mbps"));
            }
            _ => panic!("Expected Converted variant"),
        }
    }
}

#[cfg(test)]
mod analysis_filter_tests {
    use super::*;

    #[test]
    fn filter_debug_format() {
        let filter = AnalysisFilter {
            min_bitrate: 8000,
            max_bitrate: Some(50000),
            min_duration: Some(60.0),
            max_duration: Some(7200.0),
            overwrite: false,
        };
        let debug = format!("{filter:?}");
        assert!(debug.contains("8000"));
        assert!(debug.contains("50000"));
        assert!(debug.contains("60.0"));
        assert!(debug.contains("7200.0"));
    }
}
