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
    /// Video resolution (width and height in pixels).
    pub resolution: Option<Resolution>,
    /// Duration in seconds.
    pub duration: Option<f64>,
    /// Video codec name (e.g., "h264", "hevc").
    pub codec: Option<String>,
    /// Video bitrate in kbps.
    pub bitrate_kbps: Option<u64>,
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
    /// Returns an error if ffprobe cannot be executed.
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let size_bytes = std::fs::metadata(path).map(|m| m.len()).ok();

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
            .arg(path)
            .output()
            .context("Failed to execute ffprobe")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut info = Self::parse_ffprobe_output(&stdout);
        info.size_bytes = size_bytes;
        Ok(info)
    }

    /// Parse ffprobe key=value output into a `VideoInfo`.
    ///
    /// Fields that are missing or unparseable in the output will be `None`.
    fn parse_ffprobe_output(output: &str) -> Self {
        let mut codec: Option<String> = None;
        let mut bitrate_kbps: Option<u64> = None;
        let mut duration: Option<f64> = None;
        let mut width: Option<u32> = None;
        let mut height: Option<u32> = None;

        for line in output.lines() {
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
                    "width" => width = value.parse().ok(),
                    "height" => height = value.parse().ok(),
                    _ => {}
                }
            }
        }

        Self {
            size_bytes: None,
            resolution: Resolution::from_options(width, height),
            duration,
            codec,
            bitrate_kbps,
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
    pub fn print_summary(&self) {
        let total_resolutions: usize = self.resolutions.values().sum();
        let total_codecs: usize = self.codecs.values().sum();
        let total = total_resolutions
            .max(self.durations.len())
            .max(total_codecs)
            .max(self.bitrates_kbps.len())
            .max(self.file_sizes.len());

        if total < 2 {
            return;
        }

        println!();
        println!("{}", format!("Video Statistics ({total} files):").cyan().bold());

        self.print_resolution_stats();
        self.print_duration_stats();
        self.print_codec_stats();
        self.print_bitrate_stats();
        self.print_file_size_stats();
    }

    /// Print resolution statistics.
    fn print_resolution_stats(&self) {
        if self.resolutions.is_empty() {
            return;
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

        println!("  {}: {smallest} (smallest) — {biggest} (biggest)", "Resolution".bold(),);

        let mut sorted_resolutions: Vec<_> = self.resolutions.iter().collect();
        sorted_resolutions.sort_by_key(|(resolution, _)| resolution.pixel_count());
        for (resolution, count) in &sorted_resolutions {
            println!("    {resolution}: {count}");
        }
    }

    /// Print duration statistics.
    fn print_duration_stats(&self) {
        if self.durations.is_empty() {
            return;
        }

        let min_duration = self.durations.iter().copied().reduce(f64::min).expect("non-empty");
        let max_duration = self.durations.iter().copied().reduce(f64::max).expect("non-empty");
        let average = self.durations.iter().sum::<f64>() / self.durations.len() as f64;
        let median = compute_median_f64(&self.durations);

        println!(
            "  {}: {} (shortest) — {} (longest) | avg: {} | median: {}",
            "Duration".bold(),
            crate::format_duration_seconds(min_duration),
            crate::format_duration_seconds(max_duration),
            crate::format_duration_seconds(average),
            crate::format_duration_seconds(median),
        );
    }

    /// Print codec statistics.
    fn print_codec_stats(&self) {
        if self.codecs.is_empty() {
            return;
        }

        let mut sorted_codecs: Vec<_> = self.codecs.iter().collect();
        sorted_codecs.sort_by(|a, b| b.1.cmp(a.1));

        let codec_summary: Vec<String> = sorted_codecs
            .iter()
            .map(|(codec, count)| format!("{codec}: {count}"))
            .collect();
        println!("  {}: {}", "Codecs".bold(), codec_summary.join(", "));
    }

    /// Print bitrate statistics.
    fn print_bitrate_stats(&self) {
        if self.bitrates_kbps.is_empty() {
            return;
        }

        let min_bitrate = *self.bitrates_kbps.iter().min().expect("non-empty");
        let max_bitrate = *self.bitrates_kbps.iter().max().expect("non-empty");
        let average = self.bitrates_kbps.iter().sum::<u64>() as f64 / self.bitrates_kbps.len() as f64;
        let median = compute_median_u64(&self.bitrates_kbps);

        println!(
            "  {}: {:.2} Mbps (min) — {:.2} Mbps (max) | avg: {:.2} Mbps | median: {:.2} Mbps",
            "Bitrate".bold(),
            min_bitrate as f64 / 1000.0,
            max_bitrate as f64 / 1000.0,
            average / 1000.0,
            median as f64 / 1000.0,
        );
    }

    /// Print file size statistics.
    fn print_file_size_stats(&self) {
        if self.file_sizes.is_empty() {
            return;
        }

        let min_size = *self.file_sizes.iter().min().expect("non-empty");
        let max_size = *self.file_sizes.iter().max().expect("non-empty");
        let average = self.file_sizes.iter().sum::<u64>() as f64 / self.file_sizes.len() as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let average_u64 = average as u64;
        let median = compute_median_u64(&self.file_sizes);
        let total: u64 = self.file_sizes.iter().sum();

        println!(
            "  {}: {} (smallest) — {} (biggest) | avg: {} | median: {} | total: {}",
            "File size".bold(),
            crate::format_size(min_size),
            crate::format_size(max_size),
            crate::format_size(average_u64),
            crate::format_size(median),
            crate::format_size(total),
        );
    }
}

impl Default for VideoStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the median of a slice of `f64` values.
fn compute_median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let length = sorted.len();
    if length.is_multiple_of(2) {
        f64::midpoint(sorted[length / 2 - 1], sorted[length / 2])
    } else {
        sorted[length / 2]
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
    if length.is_multiple_of(2) {
        u64::midpoint(sorted[length / 2 - 1], sorted[length / 2])
    } else {
        sorted[length / 2]
    }
}

#[cfg(test)]
mod test_video_info {
    use super::*;

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
        assert!(stats.durations.is_empty());
        assert!(stats.codecs.is_empty());
        assert!(stats.bitrates_kbps.is_empty());
        assert!(stats.file_sizes.is_empty());
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
        assert!(stats.durations.is_empty());
        assert!(stats.codecs.is_empty());
        assert!(stats.bitrates_kbps.is_empty());
        assert!(stats.file_sizes.is_empty());
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
}

#[cfg(test)]
mod test_parse_ffprobe_output {
    use super::*;

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
}
