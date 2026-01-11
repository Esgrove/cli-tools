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
