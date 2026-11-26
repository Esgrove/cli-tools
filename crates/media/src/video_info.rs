//! Shared video metadata parsing, resolution helpers, and aggregate statistics.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use anyhow::Context;
use colored::Colorize;

pub use crate::resolution::Resolution;

/// Video file information gathered from filesystem metadata and ffprobe.
#[derive(Debug, Clone, Default)]
pub struct VideoInfo {
    /// File size in bytes.
    pub size_bytes: Option<u64>,
    /// Display video resolution after applying stream rotation (width and height in pixels).
    pub resolution: Option<Resolution>,
    /// Raw coded video resolution before rotation is applied.
    pub coded_resolution: Option<Resolution>,
    /// Stream rotation in degrees (0, 90, 180, or 270) when reported by ffprobe.
    pub rotation: Option<i32>,
    /// Duration in seconds.
    pub duration: Option<f64>,
    /// Video codec name (for example "h264", "hevc", "av1").
    pub codec: Option<String>,
    /// Video bitrate in kbps.
    pub bitrate_kbps: Option<u64>,
    /// Video frames per second.
    pub frames_per_second: Option<f64>,
    /// Per-channel video bit depth (for example 8 or 10).
    pub bit_depth: Option<u8>,
}

/// Collected statistics across all processed video files.
pub struct VideoStats {
    /// Count of each unique resolution encountered.
    resolutions: HashMap<Resolution, usize>,
    /// All durations in seconds.
    durations: Vec<f64>,
    /// Count of each unique codec encountered.
    codecs: HashMap<String, usize>,
    /// All bitrates in kbps.
    bitrates_kbps: Vec<u64>,
    /// All file sizes in bytes.
    file_sizes: Vec<u64>,
}

impl VideoInfo {
    /// Construct video information by running ffprobe on the given file path.
    ///
    /// File size is read from filesystem metadata.
    /// Individual fields are `None` if they cannot be determined from the ffprobe output.
    ///
    /// # Errors
    ///
    /// Returns an error if ffprobe cannot be executed or cannot read the file.
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let size_bytes = std::fs::metadata(path).map(|m| m.len()).ok();

        let input_path = ffmpeg_input_path(path);
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "V:0",
                "-show_entries",
                "stream=codec_name,bit_rate,width,height,r_frame_rate,avg_frame_rate,pix_fmt,bits_per_raw_sample:stream_tags=BPS,BPS-eng,rotate:stream_side_data=rotation:format=bit_rate,duration,size",
                "-of",
                "default=nokey=0:noprint_wrappers=1",
                "-i",
            ])
            .arg(&input_path)
            .output()
            .context("Failed to execute ffprobe")?;
        if !output.status.success() {
            anyhow::bail!(
                "ffprobe failed for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut info = Self::parse_ffprobe_output(&stdout);
        if info.codec.is_none() && info.resolution.is_none() {
            anyhow::bail!("No video stream found in {}", path.display());
        }
        if info.size_bytes.is_none() {
            info.size_bytes = size_bytes;
        } else if let Some(fs_size) = size_bytes {
            info.size_bytes = Some(fs_size);
        }
        Ok(info)
    }

    /// Parse ffprobe `key=value` output into a [`VideoInfo`].
    ///
    /// Fields that are missing or unparseable in the output are `None`.
    /// When both `bits_per_raw_sample` and `pix_fmt` are present,
    /// `bits_per_raw_sample` always takes precedence regardless of line order.
    #[must_use]
    pub fn parse_ffprobe_output(output: &str) -> Self {
        let mut codec: Option<String> = None;
        let mut bitrate_kbps: Option<u64> = None;
        let mut size_bytes: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut frames_per_second: Option<f64> = None;
        let mut average_frames_per_second: Option<f64> = None;
        let mut bits_per_raw_sample: Option<u8> = None;
        let mut pix_fmt_depth: Option<u8> = None;

        for line in output.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "codec_name" => {
                        if !value.is_empty() {
                            codec = Some(value.to_lowercase());
                        }
                    }
                    "bit_rate" | "BPS" | "BPS-eng" | "TAG:BPS" | "TAG:BPS-eng" => {
                        if bitrate_kbps.is_none()
                            && let Ok(bps) = value.parse::<u64>()
                            && bps > 0
                        {
                            bitrate_kbps = Some(bps / 1000);
                        }
                    }
                    "size" => {
                        if size_bytes.is_none()
                            && let Ok(size) = value.parse::<u64>()
                        {
                            size_bytes = Some(size);
                        }
                    }
                    "duration" => {
                        if let Ok(seconds) = value.parse::<f64>()
                            && seconds.is_finite()
                            && seconds > 0.0
                        {
                            duration = Some(seconds);
                        }
                    }
                    "bits_per_raw_sample" => {
                        if bits_per_raw_sample.is_none()
                            && let Ok(depth) = value.parse::<u8>()
                            && depth > 0
                        {
                            bits_per_raw_sample = Some(depth);
                        }
                    }
                    "pix_fmt" => {
                        if pix_fmt_depth.is_none() {
                            pix_fmt_depth = bit_depth_from_pixel_format(value);
                        }
                    }
                    "r_frame_rate" | "avg_frame_rate" => {
                        let target = if key == "r_frame_rate" {
                            &mut frames_per_second
                        } else {
                            &mut average_frames_per_second
                        };
                        if target.is_none() {
                            *target = parse_frame_rate(value);
                        }
                    }
                    _ => {}
                }
            }
        }

        let (coded_resolution, rotation, resolution) = parse_resolution_and_rotation(output);

        Self {
            size_bytes,
            resolution,
            coded_resolution,
            rotation,
            duration,
            codec,
            bitrate_kbps,
            frames_per_second: frames_per_second.or(average_frames_per_second),
            bit_depth: bits_per_raw_sample.or(pix_fmt_depth),
        }
    }

    /// Return the resolution if available.
    #[must_use]
    pub const fn resolution(&self) -> Option<Resolution> {
        self.resolution
    }

    /// Format resolution as a string using the `Display` implementation.
    #[must_use]
    pub fn resolution_string(&self) -> Option<String> {
        self.resolution.map(|resolution| resolution.to_string())
    }
}

impl VideoStats {
    /// Maximum number of resolution entries shown in non-verbose mode.
    const MAX_RESOLUTION_ROWS: usize = 10;

    /// Create a new empty video stats collector.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resolutions: HashMap::new(),
            durations: Vec::new(),
            codecs: HashMap::new(),
            bitrates_kbps: Vec::new(),
            file_sizes: Vec::new(),
        }
    }

    /// Add video information to the collected stats.
    pub fn add(&mut self, info: &VideoInfo) {
        if let Some(resolution) = info.resolution() {
            *self.resolutions.entry(resolution).or_insert(0) += 1;
        }
        if let Some(duration) = info.duration {
            self.durations.push(duration);
        }
        if let Some(ref codec) = info.codec {
            *self.codecs.entry(codec.clone()).or_insert(0) += 1;
        }
        if let Some(bitrate_kbps) = info.bitrate_kbps {
            self.bitrates_kbps.push(bitrate_kbps);
        }
        if let Some(size_bytes) = info.size_bytes {
            self.file_sizes.push(size_bytes);
        }
    }

    /// Print a combined summary of all collected video stats.
    ///
    /// When `verbose` is true, all resolution entries are shown.
    /// Otherwise, only the top rows by count are displayed.
    pub fn print_summary(&self, verbose: bool) {
        for line in self.summary_lines(verbose) {
            println!("{line}");
        }
    }

    /// Lines of the combined summary, empty when fewer than two files were collected.
    ///
    /// The lines are built rather than printed directly so they can be checked in a test.
    #[must_use]
    pub fn summary_lines(&self, verbose: bool) -> Vec<String> {
        let total_resolutions: usize = self.resolutions.values().sum();
        let total_codecs: usize = self.codecs.values().sum();
        let total = total_resolutions
            .max(self.durations.len())
            .max(total_codecs)
            .max(self.bitrates_kbps.len())
            .max(self.file_sizes.len());

        if total < 2 {
            return Vec::new();
        }

        let mut lines = vec![
            String::new(),
            format!("Video Statistics ({total} files):").cyan().bold().to_string(),
        ];
        lines.extend(self.resolution_lines(verbose));
        lines.extend(self.duration_line());
        lines.extend(self.codec_line());
        lines.extend(self.bitrate_line());
        lines.extend(self.file_size_line());
        lines
    }

    /// Resolution lines, grouped by fuzzy resolution label and sorted by count.
    fn resolution_lines(&self, verbose: bool) -> Vec<String> {
        if self.resolutions.is_empty() {
            return Vec::new();
        }

        let smallest = self
            .resolutions
            .keys()
            .min_by_key(|r| r.pixel_count())
            .expect("non-empty");
        let biggest = self
            .resolutions
            .keys()
            .max_by_key(|r| r.pixel_count())
            .expect("non-empty");

        let mut lines = vec![format!("  {}: {smallest} — {biggest}", "Resolutions".bold())];

        // Aggregate counts by fuzzy resolution label, tracking the longest edge for sorting.
        let mut label_counts: HashMap<Cow<'static, str>, (usize, u32)> = HashMap::new();
        for (resolution, count) in &self.resolutions {
            let label = resolution.label();
            let max_dimension = resolution.width.max(resolution.height);
            let entry = label_counts.entry(label).or_insert((0, max_dimension));
            entry.0 += count;
            // Track maximum width (prefer landscape) for sorting
            entry.1 = entry.1.max(max_dimension);
        }

        // Sort by count descending, then by max dimension descending, then by label ascending for determinism.
        let mut sorted_labels: Vec<_> = label_counts.into_iter().collect();
        sorted_labels.sort_by(|(label_a, (count_a, max_dim_a)), (label_b, (count_b, max_dim_b))| {
            count_b
                .cmp(count_a)
                .then_with(|| max_dim_b.cmp(max_dim_a))
                .then_with(|| label_a.cmp(label_b))
        });

        let total_labels = sorted_labels.len();
        let display_count = if verbose {
            total_labels
        } else {
            total_labels.min(Self::MAX_RESOLUTION_ROWS)
        };
        let display_labels = sorted_labels.get(..display_count).unwrap_or_default();

        // Calculate max widths for right-alignment
        let max_label_width = display_labels.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
        let max_count_width = display_labels
            .iter()
            .map(|(_, (count, _))| count.to_string().chars().count())
            .max()
            .unwrap_or(0);

        for (label, (count, _)) in display_labels {
            lines.push(format!("    {label:>max_label_width$}: {count:>max_count_width$}"));
        }

        if !verbose && total_labels > Self::MAX_RESOLUTION_ROWS {
            let remaining_labels = total_labels - Self::MAX_RESOLUTION_ROWS;
            let remaining_files: usize = sorted_labels
                .get(Self::MAX_RESOLUTION_ROWS..)
                .unwrap_or_default()
                .iter()
                .map(|(_, (count, _))| count)
                .sum();
            lines.push(
                format!(
                    "    ... {} with {}",
                    cli_tools_common::count_label(remaining_labels, "more resolution", "more resolutions"),
                    cli_tools_common::count_label(remaining_files, "file", "files")
                )
                .dimmed()
                .to_string(),
            );
        }
        lines
    }

    /// Duration line, or `None` when no duration was collected.
    fn duration_line(&self) -> Option<String> {
        if self.durations.is_empty() {
            return None;
        }

        let min_duration = self.durations.iter().copied().reduce(f64::min).expect("non-empty");
        let max_duration = self.durations.iter().copied().reduce(f64::max).expect("non-empty");
        let average = self.durations.iter().sum::<f64>() / self.durations.len() as f64;
        let median = compute_median_f64(&self.durations);

        Some(format!(
            "  {}: {} — {} | avg: {} | median: {}",
            "Durations".bold(),
            cli_tools_common::format_duration_seconds(min_duration),
            cli_tools_common::format_duration_seconds(max_duration),
            cli_tools_common::format_duration_seconds(average),
            cli_tools_common::format_duration_seconds(median),
        ))
    }

    /// Codec line, or `None` when no codec was collected.
    ///
    /// A single codec is named on its own, several are named with their counts.
    fn codec_line(&self) -> Option<String> {
        if self.codecs.is_empty() {
            return None;
        }

        let mut sorted_codecs: Vec<_> = self.codecs.iter().collect();
        sorted_codecs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

        let codec_summary: Vec<String> = if sorted_codecs.len() == 1 {
            sorted_codecs.iter().map(|(codec, _)| (*codec).clone()).collect()
        } else {
            sorted_codecs
                .iter()
                .map(|(codec, count)| format!("{codec} ({count})"))
                .collect()
        };
        Some(format!("  {}: {}", "Codecs".bold(), codec_summary.join(", ")))
    }

    /// Bitrate line in megabits per second, or `None` when no bitrate was collected.
    fn bitrate_line(&self) -> Option<String> {
        if self.bitrates_kbps.is_empty() {
            return None;
        }

        let min_bitrate = *self.bitrates_kbps.iter().min().expect("non-empty");
        let max_bitrate = *self.bitrates_kbps.iter().max().expect("non-empty");
        let average = self.bitrates_kbps.iter().sum::<u64>() as f64 / self.bitrates_kbps.len() as f64;
        let median = compute_median_u64(&self.bitrates_kbps);

        Some(format!(
            "  {}: {:.2} Mbps — {:.2} Mbps | avg: {:.2} Mbps | median: {:.2} Mbps",
            "Bitrates".bold(),
            min_bitrate as f64 / 1000.0,
            max_bitrate as f64 / 1000.0,
            average / 1000.0,
            median as f64 / 1000.0,
        ))
    }

    /// File size line, or `None` when no file size was collected.
    fn file_size_line(&self) -> Option<String> {
        if self.file_sizes.is_empty() {
            return None;
        }

        let min_size = *self.file_sizes.iter().min().expect("non-empty");
        let max_size = *self.file_sizes.iter().max().expect("non-empty");
        let average = self.file_sizes.iter().sum::<u64>() as f64 / self.file_sizes.len() as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let average_u64 = average as u64;
        let median = compute_median_u64(&self.file_sizes);
        let total: u64 = self.file_sizes.iter().sum();

        Some(format!(
            "  {}: {} — {} | avg: {} | median: {} | total: {}",
            "File sizes".bold(),
            cli_tools_common::format_size(min_size),
            cli_tools_common::format_size(max_size),
            cli_tools_common::format_size(average_u64),
            cli_tools_common::format_size(median),
            cli_tools_common::format_size(total),
        ))
    }
}

impl Default for VideoStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a local file path for passing to `ffmpeg` or `ffprobe` as an input argument.
///
/// Relative paths starting with `-` or containing `:` can be misread by ffmpeg as options or URL protocols.
/// Prepending `.` forces local filesystem interpretation while leaving absolute paths unchanged.
#[must_use]
pub fn ffmpeg_input_path(path: &Path) -> std::path::PathBuf {
    if path.is_relative() && !path.starts_with(".") {
        Path::new(".").join(path)
    } else {
        path.to_path_buf()
    }
}

/// Compute the median of a slice of `f64` values, ignoring non-finite entries.
fn compute_median_f64(values: &[f64]) -> f64 {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|value| value.is_finite()).collect();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(f64::total_cmp);
    let length = sorted.len();
    let middle = length / 2;
    let Some(&upper) = sorted.get(middle) else {
        return 0.0;
    };
    if length.is_multiple_of(2) {
        let lower = sorted
            .get(..middle)
            .and_then(|values| values.last())
            .copied()
            .unwrap_or(upper);
        f64::midpoint(lower, upper)
    } else {
        upper
    }
}

/// Compute the median of a slice of `u64` values.
fn compute_median_u64(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let length = sorted.len();
    let middle = length / 2;
    let Some(&upper) = sorted.get(middle) else {
        return 0;
    };
    if length.is_multiple_of(2) {
        let lower = sorted
            .get(..middle)
            .and_then(|values| values.last())
            .copied()
            .unwrap_or(upper);
        u64::midpoint(lower, upper)
    } else {
        upper
    }
}

/// Parse the displayed resolution from ffprobe `key=value` output.
///
/// Keys may come in any order, and a later value replaces an earlier one.
/// A video stored sideways with a quarter turn rotation, such as a portrait phone video,
/// is displayed with its width and height swapped, so they are swapped here too.
/// The rotation comes from the display matrix side data as `rotation`,
/// or from the older `rotate` tag as `TAG:rotate`.
#[must_use]
pub fn parse_display_resolution(output: &str) -> Option<Resolution> {
    parse_resolution_and_rotation(output).2
}

/// Infer the per-channel bit depth from an ffmpeg pixel format name.
#[must_use]
pub fn bit_depth_from_pixel_format(pixel_format: &str) -> Option<u8> {
    if matches!(pixel_format, "nv12" | "nv21" | "nv16" | "yuv410p") {
        Some(8)
    } else if pixel_format.contains("10") {
        Some(10)
    } else if pixel_format.contains("12") {
        Some(12)
    } else if pixel_format.contains("16")
        || pixel_format.starts_with("rgb48")
        || pixel_format.starts_with("bgr48")
        || pixel_format.starts_with("rgba64")
        || pixel_format.starts_with("bgra64")
    {
        Some(16)
    } else if pixel_format.contains("yuv") || pixel_format.contains("rgb") {
        Some(8)
    } else {
        None
    }
}

/// Parse an ffprobe rational frame rate string (such as `"30000/1001"`) within `1.0..=240.0` fps.
#[must_use]
pub fn parse_frame_rate(value: &str) -> Option<f64> {
    let (numerator, denominator) = value.split_once('/')?;
    let numerator = numerator.parse::<f64>().ok()?;
    let denominator = denominator.parse::<f64>().ok()?;
    if denominator <= 0.0 {
        return None;
    }
    let fps = numerator / denominator;
    (1.0..=240.0).contains(&fps).then_some(fps)
}

/// Parse coded resolution, normalized rotation degrees, and display resolution from ffprobe output.
fn parse_resolution_and_rotation(output: &str) -> (Option<Resolution>, Option<i32>, Option<Resolution>) {
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut rotation: Option<f64> = None;
    for line in output.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key {
            "width" => width = value.parse().ok(),
            "height" => height = value.parse().ok(),
            "rotation" | "TAG:rotate" => rotation = value.parse().ok(),
            _ => {}
        }
    }
    let coded = Resolution::from_options(width, height);
    let normalized_rotation = rotation.map(|degrees| degrees.rem_euclid(360.0).round() as i32 % 360);
    let quarter_turn = rotation.is_some_and(|degrees| (degrees.rem_euclid(180.0) - 90.0).abs() < 1.0);
    let display = coded.map(|resolution| {
        if quarter_turn {
            Resolution::new(resolution.height, resolution.width)
        } else {
            resolution
        }
    });
    (coded, normalized_rotation, display)
}

#[cfg(test)]
mod test_video_info {
    use super::*;

    #[test]
    fn a_file_ffprobe_cannot_read_is_an_error() {
        if !cli_tools_common::command_exists("ffprobe") {
            return;
        }
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("garbage.mp4");
        std::fs::write(&path, b"not a video").expect("write");

        let result = VideoInfo::from_path(&path);

        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn resolution_returns_some_when_present() {
        let info = VideoInfo {
            resolution: Some(Resolution::new(1920, 1080)),
            ..Default::default()
        };
        let resolution = info.resolution().expect("should have resolution");
        assert_eq!(resolution.width, 1920);
        assert_eq!(resolution.height, 1080);
    }

    #[test]
    fn resolution_returns_none_when_missing() {
        let info = VideoInfo {
            resolution: None,
            ..Default::default()
        };
        assert!(info.resolution().is_none());
    }

    #[test]
    fn resolution_string_formats_correctly() {
        let info = VideoInfo {
            resolution: Some(Resolution::new(1920, 1080)),
            ..Default::default()
        };
        assert_eq!(info.resolution_string(), Some("1920x1080".to_string()));
    }

    #[test]
    fn resolution_string_formats_portrait() {
        let info = VideoInfo {
            resolution: Some(Resolution::new(1080, 1920)),
            ..Default::default()
        };
        assert_eq!(info.resolution_string(), Some("1080x1920".to_string()));
    }

    #[test]
    fn resolution_string_returns_none_when_resolution_missing() {
        let info = VideoInfo::default();
        assert!(info.resolution_string().is_none());
    }

    #[test]
    fn default_has_all_none() {
        let info = VideoInfo::default();
        assert!(info.size_bytes.is_none());
        assert!(info.resolution.is_none());
        assert!(info.duration.is_none());
        assert!(info.codec.is_none());
        assert!(info.bitrate_kbps.is_none());
    }
}

#[cfg(test)]
mod test_video_stats {
    use super::*;

    #[test]
    fn new_creates_empty_stats() {
        let stats = VideoStats::new();
        assert!(stats.resolutions.is_empty());
        assert_eq!(stats.durations, [] as [f64; 0]);
        assert!(stats.codecs.is_empty());
        assert_eq!(stats.bitrates_kbps, [] as [u64; 0]);
        assert_eq!(stats.file_sizes, [] as [u64; 0]);
    }

    #[test]
    fn add_collects_all_fields() {
        let mut stats = VideoStats::new();
        let info = VideoInfo {
            size_bytes: Some(1_000_000),
            resolution: Some(Resolution::new(1920, 1080)),
            duration: Some(120.0),
            codec: Some("h264".to_string()),
            bitrate_kbps: Some(5000),
            ..Default::default()
        };
        stats.add(&info);

        assert_eq!(stats.resolutions.len(), 1);
        assert_eq!(
            *stats
                .resolutions
                .get(&Resolution::new(1920, 1080))
                .expect("should exist"),
            1
        );
        assert_eq!(stats.durations.len(), 1);
        assert_eq!(stats.codecs.len(), 1);
        assert_eq!(stats.bitrates_kbps.len(), 1);
        assert_eq!(stats.file_sizes.len(), 1);
    }

    #[test]
    fn add_skips_none_fields() {
        let mut stats = VideoStats::new();
        let info = VideoInfo::default();
        stats.add(&info);

        assert!(stats.resolutions.is_empty());
        assert_eq!(stats.durations, [] as [f64; 0]);
        assert!(stats.codecs.is_empty());
        assert_eq!(stats.bitrates_kbps, [] as [u64; 0]);
        assert_eq!(stats.file_sizes, [] as [u64; 0]);
    }

    #[test]
    fn add_counts_duplicate_resolutions() {
        let mut stats = VideoStats::new();
        let info = VideoInfo {
            resolution: Some(Resolution::new(1920, 1080)),
            ..Default::default()
        };
        stats.add(&info);
        stats.add(&info);

        assert_eq!(stats.resolutions.len(), 1);
        assert_eq!(
            *stats
                .resolutions
                .get(&Resolution::new(1920, 1080))
                .expect("should exist"),
            2
        );
    }

    #[test]
    fn add_counts_duplicate_codecs() {
        let mut stats = VideoStats::new();
        let info = VideoInfo {
            codec: Some("h264".to_string()),
            ..Default::default()
        };
        stats.add(&info);
        stats.add(&info);

        assert_eq!(stats.codecs.len(), 1);
        assert_eq!(*stats.codecs.get("h264").expect("should exist"), 2);
    }
}

#[cfg(test)]
mod test_compute_median_f64 {
    use super::*;

    #[test]
    fn empty_returns_zero() {
        assert!((compute_median_f64(&[]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn single_value() {
        assert!((compute_median_f64(&[5.0]) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn odd_count() {
        assert!((compute_median_f64(&[1.0, 3.0, 2.0]) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn even_count() {
        assert!((compute_median_f64(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod test_compute_median_u64 {
    use super::*;

    #[test]
    fn empty_returns_zero() {
        assert_eq!(compute_median_u64(&[]), 0);
    }

    #[test]
    fn single_value() {
        assert_eq!(compute_median_u64(&[42]), 42);
    }

    #[test]
    fn odd_count() {
        assert_eq!(compute_median_u64(&[3, 1, 2]), 2);
    }

    #[test]
    fn even_count() {
        assert_eq!(compute_median_u64(&[1, 2, 3, 4]), 2);
    }

    #[test]
    fn maximum_values_do_not_overflow() {
        assert_eq!(compute_median_u64(&[u64::MAX - 1, u64::MAX]), u64::MAX - 1);
    }
}

#[cfg(test)]
mod test_parse_ffprobe_output {
    use super::*;

    #[test]
    fn a_quarter_turn_rotation_swaps_the_dimensions() {
        for rotation in [
            "rotation=90",
            "rotation=-90",
            "rotation=270",
            "TAG:rotate=90",
            "rotation=-90.000000",
        ] {
            let output = format!("codec_name=h264\nwidth=1920\nheight=1080\n{rotation}\nduration=10.0\n");
            let info = VideoInfo::parse_ffprobe_output(&output);
            assert_eq!(info.resolution, Some(Resolution::new(1080, 1920)), "{rotation}");
        }
    }

    #[test]
    fn a_half_turn_rotation_keeps_the_dimensions() {
        let output = "codec_name=h264\nwidth=1920\nheight=1080\nrotation=180\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
    }

    #[test]
    fn parses_all_fields() {
        let output = "codec_name=h264\nwidth=1920\nheight=1080\nduration=3661.5\nbit_rate=5000000\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.codec.as_deref(), Some("h264"));
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
        assert!((info.duration.expect("should have duration") - 3661.5).abs() < 0.01);
        assert_eq!(info.bitrate_kbps, Some(5000));
        assert!(info.size_bytes.is_none());
    }

    #[test]
    fn parses_codec_as_lowercase() {
        let output = "codec_name=H264\nwidth=1920\nheight=1080\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.codec.as_deref(), Some("h264"));
    }

    #[test]
    fn parses_bitrate_from_bps_tag() {
        let output = "codec_name=hevc\nwidth=1920\nheight=1080\nBPS=8000000\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.bitrate_kbps, Some(8000));
    }

    #[test]
    fn parses_bitrate_from_bps_eng_tag() {
        let output = "codec_name=hevc\nwidth=1920\nheight=1080\nBPS-eng=4000000\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.bitrate_kbps, Some(4000));
    }

    #[test]
    fn parses_bitrate_from_tag_prefixed_bps_keys() {
        let bps = VideoInfo::parse_ffprobe_output("codec_name=hevc\nbit_rate=N/A\nTAG:BPS=8000000\nbit_rate=9500000\n");
        assert_eq!(bps.bitrate_kbps, Some(8000));

        let bps_eng =
            VideoInfo::parse_ffprobe_output("codec_name=hevc\nbit_rate=N/A\nTAG:BPS-eng=4000000\nbit_rate=9500000\n");
        assert_eq!(bps_eng.bitrate_kbps, Some(4000));
    }

    #[test]
    fn ignores_non_finite_and_negative_durations_and_median_handles_nan() {
        assert!(VideoInfo::parse_ffprobe_output("duration=NaN\n").duration.is_none());
        assert!(VideoInfo::parse_ffprobe_output("duration=inf\n").duration.is_none());
        assert!(VideoInfo::parse_ffprobe_output("duration=-5.0\n").duration.is_none());

        let values = [
            1.0,
            f64::NAN,
            3.0,
            2.0,
            f64::NAN,
            4.0,
            5.0,
            6.0,
            7.0,
            8.0,
            9.0,
            10.0,
            11.0,
            12.0,
            13.0,
            14.0,
            15.0,
            16.0,
            17.0,
            18.0,
            19.0,
            20.0,
        ];
        let median = compute_median_f64(&values);
        assert!(median.is_finite());
    }

    #[test]
    fn probes_real_mkv_with_tag_bps_and_dash_or_colon_filenames() {
        if !cli_tools_common::command_exists("ffmpeg") || !cli_tools_common::command_exists("ffprobe") {
            return;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let mkv = dir.path().join("sample.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1",
                "-c:v",
                "libx264",
                "-metadata:s:v:0",
                "BPS=8000000",
                "-y",
            ])
            .arg(&mkv)
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg should generate tiny sample mkv");

        let info = VideoInfo::from_path(&mkv).expect("ffprobe should succeed");
        assert_eq!(info.bitrate_kbps, Some(8000));

        // Verify relative paths starting with '-' or containing ':' resolve cleanly via ffmpeg_input_path.
        let dash_file = dir.path().join("-dash.mkv");
        std::fs::copy(&mkv, &dash_file).expect("copy dash file");
        let dash_output = Command::new("ffprobe")
            .current_dir(dir.path())
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,bit_rate:stream_tags=BPS,BPS-eng:format=bit_rate",
                "-of",
                "default=nokey=0:noprint_wrappers=1",
                "-i",
            ])
            .arg(ffmpeg_input_path(Path::new("-dash.mkv")))
            .output()
            .expect("probe dash file");
        assert!(dash_output.status.success(), "ffprobe should accept ./-dash.mkv");
        let dash_info = VideoInfo::parse_ffprobe_output(&String::from_utf8_lossy(&dash_output.stdout));
        assert_eq!(dash_info.bitrate_kbps, Some(8000));

        if std::fs::copy(&mkv, dir.path().join("foo:bar.mkv")).is_ok() {
            let colon_output = Command::new("ffprobe")
                .current_dir(dir.path())
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=codec_name,bit_rate:stream_tags=BPS,BPS-eng:format=bit_rate",
                    "-of",
                    "default=nokey=0:noprint_wrappers=1",
                    "-i",
                ])
                .arg(ffmpeg_input_path(Path::new("foo:bar.mkv")))
                .output()
                .expect("probe colon file");
            assert!(colon_output.status.success(), "ffprobe should accept ./foo:bar.mkv");
            let colon_info = VideoInfo::parse_ffprobe_output(&String::from_utf8_lossy(&colon_output.stdout));
            assert_eq!(colon_info.bitrate_kbps, Some(8000));
        }
    }

    #[test]
    fn first_bitrate_source_wins() {
        let output = "bit_rate=5000000\nBPS=8000000\nBPS-eng=4000000\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.bitrate_kbps, Some(5000));
    }

    #[test]
    fn ignores_zero_bitrate() {
        let output = "bit_rate=0\nBPS=8000000\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.bitrate_kbps, Some(8000));
    }

    #[test]
    fn missing_codec() {
        let output = "width=1920\nheight=1080\nduration=120.5\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.codec.is_none());
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
        assert!((info.duration.expect("should have duration") - 120.5).abs() < 0.01);
    }

    #[test]
    fn missing_resolution() {
        let output = "codec_name=h264\nduration=120.5\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.resolution.is_none());
        assert_eq!(info.codec.as_deref(), Some("h264"));
    }

    #[test]
    fn missing_width() {
        let output = "codec_name=h264\nheight=1080\nduration=120.5\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.resolution.is_none());
    }

    #[test]
    fn missing_height() {
        let output = "codec_name=h264\nwidth=1920\nduration=120.5\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.resolution.is_none());
    }

    #[test]
    fn missing_duration() {
        let output = "codec_name=h264\nwidth=1920\nheight=1080\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.duration.is_none());
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
    }

    #[test]
    fn missing_bitrate() {
        let output = "codec_name=h264\nwidth=1920\nheight=1080\nduration=120.5\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.bitrate_kbps.is_none());
    }

    #[test]
    fn empty_output() {
        let info = VideoInfo::parse_ffprobe_output("");
        assert!(info.codec.is_none());
        assert!(info.resolution.is_none());
        assert!(info.duration.is_none());
        assert!(info.bitrate_kbps.is_none());
        assert!(info.size_bytes.is_none());
    }

    #[test]
    fn skips_empty_lines() {
        let output = "codec_name=h264\n\nwidth=1920\n\nheight=1080\n\nduration=45.0\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.codec.as_deref(), Some("h264"));
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
        assert!((info.duration.expect("should have duration") - 45.0).abs() < 0.01);
    }

    #[test]
    fn skips_unknown_keys() {
        let output = "codec_name=h264\nunknown_key=value\nwidth=1920\nheight=1080\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.codec.as_deref(), Some("h264"));
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
    }

    #[test]
    fn handles_na_bitrate_value() {
        let output = "codec_name=h264\nwidth=1920\nheight=1080\nbit_rate=N/A\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.bitrate_kbps.is_none());
    }

    #[test]
    fn uses_valid_bitrate_after_malformed_primary_value() {
        let output = "bit_rate=N/A\nBPS=8000000\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.bitrate_kbps, Some(8000));
    }

    #[test]
    fn malformed_duration_remains_missing() {
        let output = "duration=N/A\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.duration.is_none());
    }

    #[test]
    fn later_scalar_values_replace_earlier_values() {
        let output = "codec_name=h264\ncodec_name=hevc\nduration=10\nduration=20\nwidth=1280\nwidth=1920\nheight=720\nheight=1080\n";
        let info = VideoInfo::parse_ffprobe_output(output);

        assert_eq!(info.codec.as_deref(), Some("hevc"));
        assert_eq!(info.duration, Some(20.0));
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
    }

    #[test]
    fn handles_malformed_width() {
        let output = "codec_name=h264\nwidth=abc\nheight=1080\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.resolution.is_none());
    }

    #[test]
    fn handles_malformed_height() {
        let output = "codec_name=h264\nwidth=1920\nheight=xyz\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert!(info.resolution.is_none());
    }

    #[test]
    fn handles_large_resolution() {
        let output = "width=7680\nheight=4320\n";
        let info = VideoInfo::parse_ffprobe_output(output);
        assert_eq!(info.resolution, Some(Resolution::new(7680, 4320)));
    }

    #[test]
    fn trims_whitespace_around_lines() {
        let output = "  codec_name=HEVC  \n  width=1920  \n  height=1080  \n";
        let info = VideoInfo::parse_ffprobe_output(output);

        assert_eq!(info.codec.as_deref(), Some("hevc"));
        assert_eq!(info.resolution, Some(Resolution::new(1920, 1080)));
    }

    #[test]
    fn ignores_lines_without_key_value_separator() {
        let output = "codec_name=h264\nmalformed line\nwidth=1280\nheight=720\n";
        let info = VideoInfo::parse_ffprobe_output(output);

        assert_eq!(info.codec.as_deref(), Some("h264"));
        assert_eq!(info.resolution, Some(Resolution::new(1280, 720)));
    }
}

#[cfg(test)]
mod test_summary_lines {
    use super::*;

    /// Message without the terminal color codes.
    ///
    /// Colors are on when the terminal supports them,
    /// which otherwise splits the text with escape sequences.
    fn plain(message: &str) -> String {
        let mut result = String::with_capacity(message.len());
        let mut characters = message.chars();
        while let Some(character) = characters.next() {
            if character != '\u{1b}' {
                result.push(character);
                continue;
            }
            for escape in characters.by_ref() {
                if escape == 'm' {
                    break;
                }
            }
        }
        result
    }

    /// Stats collected from the given infos.
    fn stats(infos: &[VideoInfo]) -> VideoStats {
        let mut stats = VideoStats::new();
        for info in infos {
            stats.add(info);
        }
        stats
    }

    /// Info with the given resolution, duration, codec, bitrate, and size.
    fn info(width: u32, height: u32, duration: f64, codec: &str, bitrate_kbps: u64, size_bytes: u64) -> VideoInfo {
        VideoInfo {
            size_bytes: Some(size_bytes),
            resolution: Some(Resolution::new(width, height)),
            duration: Some(duration),
            codec: Some(codec.to_string()),
            bitrate_kbps: Some(bitrate_kbps),
            ..Default::default()
        }
    }

    /// The whole summary as one plain string.
    fn summary(stats: &VideoStats, verbose: bool) -> String {
        plain(&stats.summary_lines(verbose).join("\n"))
    }

    #[test]
    fn a_single_file_produces_no_summary() {
        let stats = stats(&[info(1920, 1080, 60.0, "h264", 4000, 1_000_000)]);

        assert_eq!(stats.summary_lines(false), [] as [String; 0]);
    }

    #[test]
    fn no_files_produce_no_summary() {
        assert_eq!(VideoStats::new().summary_lines(false), [] as [String; 0]);
        assert_eq!(VideoStats::default().summary_lines(true), [] as [String; 0]);
    }

    #[test]
    fn reports_every_section_for_two_files() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
            info(3840, 2160, 120.0, "hevc", 12000, 8_000_000),
        ]);

        let text = summary(&stats, false);

        assert!(text.contains("Video Statistics (2 files):"), "{text}");
        assert!(text.contains("Resolutions"), "{text}");
        assert!(text.contains("Durations"), "{text}");
        assert!(text.contains("Codecs"), "{text}");
        assert!(text.contains("Bitrates"), "{text}");
        assert!(text.contains("File sizes"), "{text}");
    }

    #[test]
    fn the_resolution_range_runs_from_the_smallest_to_the_biggest() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
            info(3840, 2160, 60.0, "h264", 4000, 1_000_000),
        ]);

        let text = summary(&stats, false);

        assert!(text.contains("1920x1080 — 3840x2160"), "{text}");
    }

    #[test]
    fn groups_close_dimensions_by_fuzzy_label_without_changing_the_exact_range() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
            info(1918, 1078, 60.0, "h264", 4000, 1_000_000),
            info(1080, 1920, 60.0, "h264", 4000, 1_000_000),
        ]);

        let text = summary(&stats, false);

        assert!(text.contains("Resolutions: 1918x1078 — "), "{text}");
        assert!(text.lines().any(|line| line.trim() == "1080p: 2"), "{text}");
        assert!(text.lines().any(|line| line.trim() == "Vertical.1080p: 1"), "{text}");
    }

    #[test]
    fn a_single_codec_is_named_without_a_count() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
        ]);

        let text = summary(&stats, false);

        assert!(text.contains("Codecs: h264"), "{text}");
        assert!(!text.contains("h264 ("), "a lone codec needs no count: {text}");
    }

    #[test]
    fn several_codecs_are_named_with_their_counts_most_common_first() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "hevc", 4000, 1_000_000),
            info(1920, 1080, 60.0, "hevc", 4000, 1_000_000),
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
        ]);

        let text = summary(&stats, false);

        assert!(text.contains("Codecs: hevc (2), h264 (1)"), "{text}");
    }

    #[test]
    fn the_bitrate_line_reports_megabits_per_second() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 2000, 1_000_000),
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
        ]);

        let text = summary(&stats, false);

        assert!(
            text.contains("Bitrates: 2.00 Mbps — 4.00 Mbps | avg: 3.00 Mbps | median: 3.00 Mbps"),
            "{text}"
        );
    }

    #[test]
    fn the_file_size_line_reports_the_range_and_the_total() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 4000, 1024 * 1024),
            info(1920, 1080, 60.0, "h264", 4000, 3 * 1024 * 1024),
        ]);

        let text = summary(&stats, false);

        assert!(text.contains("File sizes"), "{text}");
        assert!(text.contains("total: 4.00"), "the total should be the sum: {text}");
    }

    #[test]
    fn only_the_top_resolution_rows_are_listed_unless_verbose() {
        let infos: Vec<VideoInfo> = (0..VideoStats::MAX_RESOLUTION_ROWS + 3)
            .map(|index| {
                let height = 100 + index as u32 * 100;
                info(height * 2, height, 60.0, "h264", 4000, 1_000_000)
            })
            .collect();
        let stats = stats(&infos);

        let short = summary(&stats, false);
        let long = summary(&stats, true);

        assert!(
            short.contains("more resolutions"),
            "the rest should be summarised: {short}"
        );
        assert!(
            !long.contains("more resolutions"),
            "verbose should list every row: {long}"
        );
        assert!(long.lines().count() > short.lines().count());
    }

    #[test]
    fn missing_fields_leave_their_sections_out() {
        let bare = VideoInfo {
            resolution: Some(Resolution::new(1920, 1080)),
            ..VideoInfo::default()
        };
        let stats = stats(&[bare.clone(), bare]);

        let text = summary(&stats, false);

        assert!(text.contains("Resolutions"), "{text}");
        assert!(!text.contains("Durations"), "{text}");
        assert!(!text.contains("Codecs"), "{text}");
        assert!(!text.contains("Bitrates"), "{text}");
        assert!(!text.contains("File sizes"), "{text}");
    }

    #[test]
    fn printing_the_summary_does_not_panic() {
        let stats = stats(&[
            info(1920, 1080, 60.0, "h264", 4000, 1_000_000),
            info(3840, 2160, 120.0, "hevc", 12000, 8_000_000),
        ]);

        stats.print_summary(false);
        stats.print_summary(true);
        VideoStats::new().print_summary(true);
    }
}

#[cfg(test)]
mod test_full_metadata_parsing {
    use super::*;

    #[test]
    fn bits_per_raw_sample_overrides_pix_fmt_regardless_of_line_order() {
        let ffprobe_output = "\
codec_name=hevc
width=3840
height=2160
pix_fmt=yuv420p
r_frame_rate=30000/1001
bits_per_raw_sample=10
duration=123.45
bit_rate=12000000
";
        let info = VideoInfo::parse_ffprobe_output(ffprobe_output);
        assert_eq!(info.codec.as_deref(), Some("hevc"));
        assert_eq!(info.bit_depth, Some(10));
        assert_eq!(info.resolution, Some(Resolution::new(3840, 2160)));
        assert_eq!(info.coded_resolution, Some(Resolution::new(3840, 2160)));
        assert!(info.frames_per_second.is_some_and(|fps| (fps - 29.97).abs() < 0.01));
    }

    #[test]
    fn rotated_video_tracks_both_coded_and_display_resolution() {
        let ffprobe_output = "\
codec_name=h264
width=1920
height=1080
rotation=-90
duration=10.0
";
        let info = VideoInfo::parse_ffprobe_output(ffprobe_output);
        assert_eq!(info.coded_resolution, Some(Resolution::new(1920, 1080)));
        assert_eq!(info.resolution, Some(Resolution::new(1080, 1920)));
        assert_eq!(info.rotation, Some(270));
    }

    #[test]
    fn zero_duration_is_treated_as_missing() {
        let info = VideoInfo::parse_ffprobe_output("codec_name=h264\nduration=0.0\n");
        assert!(info.duration.is_none());
    }

    #[test]
    fn video_stats_breaks_codec_and_resolution_ties_deterministically() {
        let mut stats = VideoStats::new();
        stats.add(&VideoInfo {
            resolution: Some(Resolution::new(1920, 1080)),
            codec: Some("hevc".to_string()),
            ..Default::default()
        });
        stats.add(&VideoInfo {
            resolution: Some(Resolution::new(1080, 1920)),
            codec: Some("av1".to_string()),
            ..Default::default()
        });

        let summary = stats.summary_lines(true).join("\n");
        let av1_pos = summary.find("av1 (1)").expect("av1 present");
        let hevc_pos = summary.find("hevc (1)").expect("hevc present");
        assert!(av1_pos < hevc_pos, "expected alphabetical tie-break: {summary}");
    }
}
