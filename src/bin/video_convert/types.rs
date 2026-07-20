//! Core data types for video analysis and processing.
//!
//! Defines media metadata, file actions, processing results, subtitle sidecars, and output path rules.

use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use regex::Regex;

use crate::classification::{RE_10BIT, RE_AV1, RE_SOURCE_CODEC, RE_X265};
use crate::config::{Config, TARGET_EXTENSION};
use crate::stats::ConversionStats;

/// Information about a video file from ffprobe
#[derive(Debug, Clone)]
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
    /// Video bit depth per color channel.
    pub(crate) bit_depth: u8,
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
    /// Minimum resolution — both width and height must be at least this many pixels.
    pub(crate) min_resolution: Option<u32>,
    /// Whether to overwrite existing output files.
    pub(crate) overwrite: bool,
}

/// A video file with its path and parsed name components.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VideoFile {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) extension: String,
    /// File size in bytes, captured from filesystem metadata during discovery.
    /// Used for scan cache lookups without additional `stat()` calls.
    pub(crate) size_bytes: u64,
}

/// Result of running ffprobe on a cache miss, bundling everything needed to
/// write the result back to the scan cache and continue with analysis.
pub struct VideoInfoCache {
    /// Classification result for the file.
    pub(crate) result: AnalysisResult,
    /// Original file path, kept separately so the caller can write the cache
    /// entry without digging into the `AnalysisResult` enum.
    pub(crate) path: PathBuf,
    /// `VideoInfo` from ffprobe to persist in the scan cache.
    /// `None` only when ffprobe itself failed.
    pub(crate) info: Option<VideoInfo>,
}

/// A video file with its analyzed info, ready for processing.
#[derive(Debug)]
pub struct ProcessableFile {
    pub(crate) file: VideoFile,
    pub(crate) info: VideoInfo,
    pub(crate) output_path: PathBuf,
    pub(crate) subtitle_files: Vec<SubtitleFile>,
}

/// A subtitle sidecar file that can be embedded into a movie-mode output file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleFile {
    pub(crate) path: PathBuf,
    pub(crate) extension: String,
    pub(crate) paired_sub_path: Option<PathBuf>,
}

/// Output from the analysis phase.
pub struct AnalysisOutput {
    /// Files that need full conversion (non-HEVC to HEVC).
    pub(crate) conversions: Vec<ProcessableFile>,
    /// Files that need remuxing (target codec but wrong container).
    pub(crate) remuxes: Vec<ProcessableFile>,
    /// Files that need to be renamed (target codec MP4 without codec suffix).
    pub(crate) renames: Vec<ProcessableFile>,
    /// Files that only need external subtitle sidecars muxed in.
    pub(crate) subtitle_muxes: Vec<ProcessableFile>,
}

/// Codec used in output filenames to identify the video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// HEVC/H.265 codec, uses "x265" suffix.
    X265,
    /// AV1 codec, uses "av1" suffix.
    Av1,
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
    /// Either dimension (width or height) is below the minimum resolution limit
    ResolutionBelowLimit { width: u32, height: u32, limit: u32 },
    /// Output file already exists
    OutputExists { path: PathBuf, source_duration: f64 },
    /// File no longer exists (may have been moved or renamed)
    FileMissing,
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
    /// External subtitle sidecars were embedded without video conversion.
    SubtitlesMuxed {},
    /// Failed to process file
    Failed { error: String },
}

/// Outcome of processing a batch of files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingOutcome {
    /// All files in the batch were processed, or the configured limit was reached.
    Completed,
    /// Processing was aborted by the user with Ctrl+C.
    Aborted,
    /// Processing was stopped because the disk does not have enough free space.
    OutOfDiskSpace,
}

/// Result of analyzing a video file to determine what action to take.
#[derive(Debug)]
pub enum AnalysisResult {
    /// File needs to be converted to HEVC
    NeedsConversion(ProcessableFile),
    /// File is already HEVC but needs remuxing to MP4
    NeedsRemux(ProcessableFile),
    /// File should be renamed to add codec suffix
    NeedsRename(ProcessableFile),
    /// File needs external subtitle sidecars muxed in without video conversion.
    NeedsSubtitleMux(ProcessableFile),
    /// File should be skipped
    Skip { file: VideoFile, reason: SkipReason },
}

impl AnalysisResult {
    /// Create a skipped analysis result for a file that could not be analyzed.
    pub(crate) const fn analysis_failed(file: VideoFile, error: String) -> Self {
        Self::Skip {
            file,
            reason: SkipReason::AnalysisFailed { error },
        }
    }
}

impl From<&Config> for AnalysisFilter {
    /// Create analysis filters from the resolved runtime configuration.
    fn from(config: &Config) -> Self {
        Self {
            min_bitrate: config.bitrate_limit,
            max_bitrate: config.max_bitrate,
            min_duration: config.min_duration,
            max_duration: config.max_duration,
            min_resolution: config.min_resolution,
            overwrite: config.overwrite,
        }
    }
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

impl ProcessableFile {
    /// Create a new `ProcessableFile` with its resolved output path.
    pub(crate) const fn new(
        file: VideoFile,
        info: VideoInfo,
        output_path: PathBuf,
        subtitle_files: Vec<SubtitleFile>,
    ) -> Self {
        Self {
            file,
            info,
            output_path,
            subtitle_files,
        }
    }
}

impl SubtitleFile {
    /// Create a subtitle sidecar file from a path and optional paired `.sub` file.
    pub(crate) fn new(path: &Path, paired_sub_path: Option<PathBuf>) -> Self {
        Self {
            path: path.to_owned(),
            extension: cli_tools::path_to_file_extension_string(path),
            paired_sub_path,
        }
    }

    /// Return true when the extension is a supported external subtitle format.
    pub(crate) fn is_supported_extension(extension: &str) -> bool {
        matches!(extension, "idx" | "srt" | "sub")
    }

    /// Return all sidecar paths that should be removed after successful muxing.
    pub(crate) fn paths_to_delete(&self) -> Vec<&Path> {
        let mut paths = vec![self.path.as_path()];
        if let Some(path) = &self.paired_sub_path {
            paths.push(path.as_path());
        }
        paths
    }
}

impl VideoFile {
    /// Create a new `VideoFile` from a path and pre-fetched file size.
    pub(crate) fn new(path: &Path, size_bytes: u64) -> Self {
        let path = path.to_owned();
        let name = cli_tools::path_to_file_stem_string(&path);
        let extension = cli_tools::path_to_file_extension_string(&path);

        Self {
            path,
            name,
            extension,
            size_bytes,
        }
    }

    /// Create a new `VideoFile` from a path, reading file size from filesystem metadata.
    ///
    /// Falls back to 0 if the metadata cannot be read.
    pub(crate) fn new_with_metadata(path: &Path) -> Self {
        let size_bytes = fs::metadata(path).map_or(0, |metadata| metadata.len());
        Self::new(path, size_bytes)
    }

    /// Compute the output path using movie-mode and bit-depth filename rules.
    pub(crate) fn get_output_path_for_mode_and_bit_depth(
        &self,
        suffix: Codec,
        movie_mode: bool,
        has_external_subtitles: bool,
        bit_depth: u8,
    ) -> PathBuf {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let target_extension = self.target_extension(movie_mode, has_external_subtitles);
        let new_stem = if suffix.regex().is_match(&self.name) {
            self.name.clone()
        } else if RE_SOURCE_CODEC.is_match(&self.name) {
            RE_SOURCE_CODEC.replace_all(&self.name, suffix.as_str()).into_owned()
        } else {
            format!("{}.{suffix}", self.name)
        };
        let new_stem = if bit_depth > 8 {
            Self::add_10bit_label(&new_stem, suffix)
        } else {
            new_stem
        };
        parent.join(format!("{new_stem}.{target_extension}"))
    }

    /// Compute the output path after removing stale labels.
    pub(crate) fn get_output_path_without_stale_labels(
        &self,
        remove_target_codec_labels: bool,
        remove_10bit_label: bool,
    ) -> Option<PathBuf> {
        let cleaned_stem = self.name_without_stale_labels(remove_target_codec_labels, remove_10bit_label)?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        Some(parent.join(format!("{cleaned_stem}.{}", self.extension)))
    }

    /// Return the target container extension for this file.
    pub(crate) fn target_extension(&self, movie_mode: bool, has_external_subtitles: bool) -> &'static str {
        if movie_mode && (self.extension == "mkv" || has_external_subtitles) {
            "mkv"
        } else {
            TARGET_EXTENSION
        }
    }

    pub(crate) fn has_10bit_label(&self) -> bool {
        RE_10BIT.is_match(&self.name)
    }

    pub(crate) fn has_target_codec_label(&self) -> bool {
        RE_X265.is_match(&self.name) || RE_AV1.is_match(&self.name)
    }

    fn name_without_stale_labels(&self, remove_target_codec_labels: bool, remove_10bit_label: bool) -> Option<String> {
        if (!remove_target_codec_labels || !self.has_target_codec_label())
            && (!remove_10bit_label || !self.has_10bit_label())
        {
            return None;
        }

        let mut cleaned = self.name.clone();
        if remove_target_codec_labels {
            cleaned = RE_X265.replace_all(&cleaned, "").into_owned();
            cleaned = RE_AV1.replace_all(&cleaned, "").into_owned();
        }
        if remove_10bit_label {
            cleaned = RE_10BIT.replace_all(&cleaned, "").into_owned();
        }

        let cleaned = cli_tools::collapse_repeated_separators(&cleaned);
        if cleaned.is_empty() || cleaned == self.name {
            None
        } else {
            Some(cleaned)
        }
    }

    fn add_10bit_label(stem: &str, suffix: Codec) -> String {
        if RE_10BIT.is_match(stem) {
            return stem.to_string();
        }
        if suffix.regex().is_match(stem) {
            suffix.regex().replace(stem, format!("10bit.{suffix}")).into_owned()
        } else {
            format!("{stem}.10bit")
        }
    }
}

impl VideoInfo {
    /// Parse `VideoInfo` from ffprobe output.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn from_ffprobe_output(stdout: &str, stderr: &str, path: &Path) -> anyhow::Result<Self> {
        let mut codec = String::new();
        let mut bitrate_kbps: Option<u64> = None;
        let mut size_bytes: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;
        let mut frames_per_second: Option<f64> = None;
        let mut bit_depth: Option<u8> = None;

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
                    "codec_name" => {
                        if codec.is_empty() {
                            codec = value.to_lowercase();
                        }
                    }
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
                        if width.is_none()
                            && let Ok(w) = value.parse::<u32>()
                        {
                            width = Some(w);
                        }
                    }
                    "height" => {
                        if height.is_none()
                            && let Ok(h) = value.parse::<u32>()
                        {
                            height = Some(h);
                        }
                    }
                    "bits_per_raw_sample" => {
                        if bit_depth.is_none()
                            && let Ok(depth) = value.parse::<u8>()
                            && depth > 0
                        {
                            bit_depth = Some(depth);
                        }
                    }
                    "pix_fmt" => {
                        if bit_depth.is_none() {
                            bit_depth = Self::bit_depth_from_pixel_format(value);
                        }
                    }
                    "r_frame_rate" => {
                        // Parse fractional framerate like "30/1" or "30000/1001".
                        // Only accept the first valid value within a reasonable range,
                        // as ffprobe may output multiple streams where later entries
                        // can have bogus values like 0/1 or 90000/1 (timebase).
                        if frames_per_second.is_none()
                            && let Some((num, den)) = value.split_once('/')
                            && let (Ok(n), Ok(d)) = (num.parse::<f64>(), den.parse::<f64>())
                            && d > 0.0
                            && n > 0.0
                        {
                            let fps = n / d;
                            if (1.0..=240.0).contains(&fps) {
                                frames_per_second = Some(fps);
                            }
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

        let bit_depth = bit_depth.unwrap_or(8);

        let warning = if stderr.is_empty() {
            None
        } else {
            Some(stderr.trim().to_string())
        };

        // Fall back to file metadata for size if not in ffprobe output
        let size_bytes = size_bytes.unwrap_or_else(|| fs::metadata(path).map_or(0, |metadata| metadata.len()));

        Ok(Self {
            codec,
            bitrate_kbps,
            size_bytes,
            duration,
            width,
            height,
            frames_per_second,
            bit_depth,
            warning,
        })
    }

    fn bit_depth_from_pixel_format(pixel_format: &str) -> Option<u8> {
        if pixel_format.contains("10") {
            Some(10)
        } else if pixel_format.contains("12") {
            Some(12)
        } else if pixel_format.contains("16") {
            Some(16)
        } else if pixel_format.contains("yuv") || pixel_format.contains("rgb") || pixel_format == "nv12" {
            Some(8)
        } else {
            None
        }
    }

    /// Determine quality level based on resolution and bitrate.
    /// Quality level 1 to 51, lower is better quality and bigger file size.
    pub(crate) fn quality_level(&self) -> u8 {
        let shorter_edge = self.width.min(self.height);
        let bitrate_mbps = self.bitrate_kbps as f64 / 1000.0;

        if shorter_edge >= 2160 {
            if bitrate_mbps > 34.0 {
                28
            } else if bitrate_mbps > 26.0 {
                29
            } else if bitrate_mbps > 18.0 {
                30
            } else if bitrate_mbps > 12.0 {
                31
            } else {
                32
            }
        } else if shorter_edge >= 1080 {
            if bitrate_mbps > 20.0 {
                26
            } else if bitrate_mbps > 16.0 {
                27
            } else if bitrate_mbps > 12.0 {
                28
            } else if bitrate_mbps > 6.0 {
                29
            } else {
                30
            }
        } else if bitrate_mbps > 8.0 {
            29
        } else if bitrate_mbps > 6.0 {
            30
        } else if bitrate_mbps > 3.0 {
            31
        } else {
            32
        }
    }

    /// Check if the codec is a target codec that does not need conversion.
    pub(crate) fn is_target_codec(&self) -> bool {
        matches!(self.codec.as_str(), "hevc" | "h265" | "av1")
    }

    /// Return true when the source video uses more than 8 bits per color channel.
    pub(crate) const fn is_10_bit(&self) -> bool {
        self.bit_depth > 8
    }

    /// Get the codec suffix for this video's codec.
    pub(crate) fn codec_suffix(&self) -> Codec {
        match self.codec.as_str() {
            "av1" => Codec::Av1,
            _ => Codec::X265,
        }
    }
}

impl Codec {
    /// Get the string representation of this codec suffix.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X265 => "x265",
            Self::Av1 => "av1",
        }
    }

    /// Get the regex that matches this codec suffix in filenames.
    #[must_use]
    pub fn regex(self) -> &'static Regex {
        match self {
            Self::X265 => &RE_X265,
            Self::Av1 => &RE_AV1,
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Return a match score when a subtitle sidecar stem appears to belong to a video stem.
pub fn movie_subtitle_match_score(video_stem: &str, subtitle_stem: &str) -> Option<usize> {
    let video_tokens = normalized_movie_tokens(video_stem);
    let subtitle_tokens = normalized_movie_tokens(subtitle_stem);

    if video_tokens.is_empty() || subtitle_tokens.is_empty() {
        return None;
    }

    if video_tokens == subtitle_tokens {
        return Some(10_000 + video_tokens.len());
    }

    if contains_tokens(&subtitle_tokens, &video_tokens) {
        return Some(5_000 + video_tokens.len());
    }

    if subtitle_tokens.len() >= 2 && contains_tokens(&video_tokens, &subtitle_tokens) {
        return Some(2_000 + subtitle_tokens.len());
    }

    None
}

fn normalized_movie_tokens(stem: &str) -> Vec<String> {
    stem.to_lowercase()
        .replace("h.264", "h264")
        .replace("h.265", "h265")
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .filter(|token| !is_ignored_movie_token(token))
        .map(ToOwned::to_owned)
        .collect()
}

fn contains_tokens(haystack: &[String], needle: &[String]) -> bool {
    haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|window| window == needle)
}

fn is_ignored_movie_token(token: &str) -> bool {
    matches!(
        token,
        "3d" | "4k"
            | "480p"
            | "576p"
            | "720p"
            | "1080p"
            | "1440p"
            | "2160p"
            | "aac"
            | "atmos"
            | "av1"
            | "bluray"
            | "brrip"
            | "cc"
            | "dts"
            | "dv"
            | "eng"
            | "english"
            | "fin"
            | "finnish"
            | "forced"
            | "h264"
            | "h265"
            | "hdr"
            | "hevc"
            | "japanese"
            | "jpn"
            | "nor"
            | "norwegian"
            | "sdh"
            | "sub"
            | "subs"
            | "subtitle"
            | "subtitles"
            | "suomi"
            | "swe"
            | "swedish"
            | "truehd"
            | "uhd"
            | "web"
            | "webdl"
            | "webrip"
            | "x264"
            | "x265"
    )
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
        writeln!(f, "Resolution: {}x{}", self.width, self.height)?;
        write!(f, "Bit depth:  {}-bit", self.bit_depth)?;
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
            Self::AlreadyConverted => write!(f, "Already target codec in target container"),
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
            Self::ResolutionBelowLimit { width, height, limit } => {
                write!(f, "Resolution {width}x{height} is below minimum limit {limit}")
            }
            Self::OutputExists { path, .. } => {
                write!(f, "Output file already exists: \"{}\"", path.display())
            }
            Self::FileMissing => write!(f, "File no longer exists"),
            Self::AnalysisFailed { error } => {
                write!(f, "Failed to analyze: {error}")
            }
        }
    }
}

impl std::fmt::Display for ProcessingOutcome {
    /// Format the reason processing stopped. `Completed` formats as an empty string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => Ok(()),
            Self::Aborted => write!(f, "{}", "Aborted by user".bold().red()),
            Self::OutOfDiskSpace => write!(f, "{}", "Stopped: not enough free disk space".bold().red()),
        }
    }
}

impl From<walkdir::DirEntry> for VideoFile {
    fn from(entry: walkdir::DirEntry) -> Self {
        let size_bytes = entry.metadata().map_or(0, |metadata| metadata.len());
        Self::new(entry.path(), size_bytes)
    }
}

#[cfg(test)]
mod video_file_tests {
    use super::*;

    fn output_path(file: &VideoFile, suffix: Codec) -> PathBuf {
        file.get_output_path_for_mode_and_bit_depth(suffix, false, false, 8)
    }

    fn output_path_for_mode(
        file: &VideoFile,
        suffix: Codec,
        movie_mode: bool,
        has_external_subtitles: bool,
    ) -> PathBuf {
        file.get_output_path_for_mode_and_bit_depth(suffix, movie_mode, has_external_subtitles, 8)
    }

    #[test]
    fn new_extracts_name_and_extension() {
        let file = VideoFile::new(Path::new("/path/to/video.mp4"), 1000);
        assert_eq!(file.name, "video");
        assert_eq!(file.extension, "mp4");
        assert_eq!(file.size_bytes, 1000);
    }

    #[test]
    fn new_handles_no_extension() {
        let file = VideoFile::new(Path::new("/path/to/video"), 0);
        assert_eq!(file.name, "video");
        assert_eq!(file.extension, "");
    }

    #[test]
    fn output_path_adds_x265_suffix() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let output = output_path(&file, Codec::X265);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mp4"));
    }

    #[test]
    fn output_path_preserves_existing_x265() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let output = output_path(&file, Codec::X265);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mp4"));
    }

    #[test]
    fn output_path_without_target_codec_label_removes_x265() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let output = file.get_output_path_without_stale_labels(true, false);
        assert_eq!(output, Some(PathBuf::from("/videos/movie.mkv")));
    }

    #[test]
    fn output_path_without_target_codec_label_removes_av1() {
        let file = VideoFile::new(Path::new("/videos/movie.av1.mkv"), 0);
        let output = file.get_output_path_without_stale_labels(true, false);
        assert_eq!(output, Some(PathBuf::from("/videos/movie.mkv")));
    }

    #[test]
    fn output_path_without_target_codec_label_collapses_separators() {
        let file = VideoFile::new(Path::new("/videos/movie..x265..1080p.mkv"), 0);
        let output = file.get_output_path_without_stale_labels(true, false);
        assert_eq!(output, Some(PathBuf::from("/videos/movie.1080p.mkv")));
    }

    #[test]
    fn output_path_without_target_codec_label_returns_none_without_label() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let output = file.get_output_path_without_stale_labels(true, false);
        assert_eq!(output, None);
    }

    #[test]
    fn output_path_without_10bit_label_removes_stale_label() {
        let file = VideoFile::new(Path::new("/videos/movie.10bit.mkv"), 0);
        let output = file.get_output_path_without_stale_labels(false, true);
        assert_eq!(output, Some(PathBuf::from("/videos/movie.mkv")));
    }

    #[test]
    fn output_path_without_stale_labels_removes_codec_and_10bit_labels() {
        let file = VideoFile::new(Path::new("/videos/movie.10bit.x265.mkv"), 0);
        let output = file.get_output_path_without_stale_labels(true, true);
        assert_eq!(output, Some(PathBuf::from("/videos/movie.mkv")));
    }

    #[test]
    fn output_path_for_10bit_adds_label_before_codec_suffix() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let output = file.get_output_path_for_mode_and_bit_depth(Codec::X265, true, false, 10);
        assert_eq!(output, PathBuf::from("/videos/movie.10bit.x265.mkv"));
    }

    #[test]
    fn output_path_for_10bit_does_not_duplicate_existing_label() {
        let file = VideoFile::new(Path::new("/videos/movie.10bit.mkv"), 0);
        let output = file.get_output_path_for_mode_and_bit_depth(Codec::X265, true, false, 10);
        assert_eq!(output, PathBuf::from("/videos/movie.10bit.x265.mkv"));
    }

    #[test]
    fn output_path_adds_av1_suffix() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let output = output_path(&file, Codec::Av1);
        assert_eq!(output, PathBuf::from("/videos/movie.av1.mp4"));
    }

    #[test]
    fn movie_mode_output_path_keeps_mkv_container() {
        let file = VideoFile::new(Path::new("/videos/movie.mkv"), 0);
        let output = output_path_for_mode(&file, Codec::X265, true, false);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn movie_mode_output_path_preserves_existing_x265() {
        let file = VideoFile::new(Path::new("/videos/movie.x265.mkv"), 0);
        let output = output_path_for_mode(&file, Codec::X265, true, false);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn movie_mode_output_path_replaces_existing_x264() {
        let file = VideoFile::new(Path::new("/videos/Movie.Title.2024.1080p.x264.mkv"), 0);
        let output = output_path_for_mode(&file, Codec::X265, true, false);
        assert_eq!(output, PathBuf::from("/videos/Movie.Title.2024.1080p.x265.mkv"));
    }

    #[test]
    fn output_path_replaces_existing_x264() {
        let file = VideoFile::new(Path::new("/videos/movie.x264.mkv"), 0);
        let output = output_path(&file, Codec::X265);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mp4"));
    }

    #[test]
    fn movie_mode_output_path_keeps_mp4_for_mp4_input() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let output = output_path_for_mode(&file, Codec::X265, true, false);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mp4"));
    }

    #[test]
    fn movie_mode_output_path_uses_mkv_for_external_subtitles() {
        let file = VideoFile::new(Path::new("/videos/movie.mp4"), 0);
        let output = output_path_for_mode(&file, Codec::X265, true, true);
        assert_eq!(output, PathBuf::from("/videos/movie.x265.mkv"));
    }

    #[test]
    fn output_path_preserves_existing_av1() {
        let file = VideoFile::new(Path::new("/videos/movie.av1.mkv"), 0);
        let output = output_path(&file, Codec::Av1);
        assert_eq!(output, PathBuf::from("/videos/movie.av1.mp4"));
    }

    #[test]
    fn display_shows_path() {
        let file = VideoFile::new(Path::new("/path/to/video.mp4"), 0);
        assert_eq!(format!("{file}"), "/path/to/video.mp4");
    }
}

#[cfg(test)]
mod subtitle_match_tests {
    use super::*;

    #[test]
    fn matches_subtitle_with_extra_language_token() {
        let score = movie_subtitle_match_score(
            "Last.Night.in.Soho.2021.1080p.x265",
            "Last.Night.in.Soho.2021.English.1080p.x265",
        );
        assert!(score.is_some());
    }

    #[test]
    fn matches_multiple_language_subtitles_to_same_movie() {
        let english =
            movie_subtitle_match_score("Dangerous.Liasons.1988.1080p.x264", "Dangerous.Liasons.1988.1080p.x264");
        let finnish = movie_subtitle_match_score(
            "Dangerous.Liasons.1988.1080p.x264",
            "Dangerous.Liasons.1988.Finnish.1080p.x264",
        );
        assert!(english.is_some());
        assert!(finnish.is_some());
    }

    #[test]
    fn rejects_unrelated_subtitle() {
        let score = movie_subtitle_match_score("Movie.Title.2024.1080p.x265", "Different.Movie.2024.English");
        assert!(score.is_none());
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
        assert_eq!(info.bit_depth, 8);
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
    fn from_ffprobe_output_parses_10bit_from_pixel_format() {
        let output = "codec_name=hevc\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      pix_fmt=yuv420p10le\n\
                      r_frame_rate=24000/1001\n";
        let info = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mkv")).unwrap();
        assert_eq!(info.bit_depth, 10);
    }

    #[test]
    fn from_ffprobe_output_prefers_raw_sample_bit_depth() {
        let output = "codec_name=hevc\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      bits_per_raw_sample=10\n\
                      pix_fmt=yuv420p\n\
                      r_frame_rate=24000/1001\n";
        let info = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mkv")).unwrap();
        assert_eq!(info.bit_depth, 10);
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
    fn from_ffprobe_output_rejects_zero_framerate() {
        let output = "codec_name=h264\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      r_frame_rate=0/1\n";
        let result = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("framerate"));
    }

    #[test]
    fn from_ffprobe_output_rejects_timebase_framerate() {
        let output = "codec_name=h264\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      r_frame_rate=90000/1\n";
        let result = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("framerate"));
    }

    #[test]
    fn from_ffprobe_output_takes_first_video_metadata_when_later_stream_is_cover_art() {
        let output = "codec_name=hevc\n\
                      bit_rate=N/A\n\
                      BPS=55592880\n\
                      duration=7675.877\n\
                      size=53340514337\n\
                      width=3840\n\
                      height=1504\n\
                      pix_fmt=yuv420p10le\n\
                      r_frame_rate=24000/1001\n\
                      codec_name=mjpeg\n\
                      width=4050\n\
                      height=6000\n\
                      r_frame_rate=90000/1\n";
        let info = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mkv")).unwrap();
        assert_eq!(info.codec, "hevc");
        assert_eq!(info.width, 3840);
        assert_eq!(info.height, 1504);
        assert_eq!(info.bit_depth, 10);
        assert!((info.frames_per_second - 23.98).abs() < 0.01);
    }

    #[test]
    fn from_ffprobe_output_takes_first_valid_framerate() {
        let output = "codec_name=h264\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      r_frame_rate=24/1\n\
                      r_frame_rate=0/1\n";
        let info = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4")).unwrap();
        assert!((info.frames_per_second - 24.0).abs() < 0.01);
    }

    #[test]
    fn from_ffprobe_output_skips_bogus_takes_valid_framerate() {
        let output = "codec_name=h264\n\
                      bit_rate=7345573\n\
                      duration=120.5\n\
                      width=1920\n\
                      height=1080\n\
                      r_frame_rate=90000/1\n\
                      r_frame_rate=25/1\n";
        let info = VideoInfo::from_ffprobe_output(output, "", Path::new("test.mp4")).unwrap();
        assert!((info.frames_per_second - 25.0).abs() < 0.01);
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
            bit_depth: 8,
            warning: None,
        };
        let display = format!("{info}");
        assert!(display.contains("hevc"));
        assert!(display.contains("1.00 GB"));
        assert!(display.contains("8.00 Mbps @ 24 FPS"));
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
        assert_eq!(format!("{reason}"), "Already target codec in target container");
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
    fn display_resolution_below_limit() {
        let reason = SkipReason::ResolutionBelowLimit {
            width: 854,
            height: 480,
            limit: 1000,
        };
        let display = format!("{reason}");
        assert!(display.contains("854"));
        assert!(display.contains("480"));
        assert!(display.contains("1000"));
        assert!(display.contains("below"));
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

    #[test]
    fn display_file_missing() {
        let reason = SkipReason::FileMissing;
        assert_eq!(format!("{reason}"), "File no longer exists");
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
            min_resolution: Some(1000),
            overwrite: false,
        };
        let debug = format!("{filter:?}");
        assert!(debug.contains("8000"));
        assert!(debug.contains("50000"));
        assert!(debug.contains("60.0"));
        assert!(debug.contains("7200.0"));
        assert!(debug.contains("1000"));
    }
}
