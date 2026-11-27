//! Options controlling a video convert run.
//!
//! Defines the input paths, file filters, analysis limits, and processing switches of one run,
//! the order in which files are processed, and the known video extensions and movie mode languages.

use std::path::PathBuf;
use std::str::FromStr;

use serde::Deserialize;

use crate::bitrate_limit::MinimumBitrate;
use crate::database::PendingFileFilter;

/// Audio languages preserved by movie mode.
pub const MOVIE_AUDIO_LANGUAGES: &[&str] = &["eng", "fin", "fra", "fre", "jpn", "swe", "nob", "nor"];

/// Subtitle languages preserved by movie mode.
pub const MOVIE_SUBTITLE_LANGUAGES: &[&str] = &["eng", "fin", "swe"];

/// Output container used when movie mode does not preserve MKV.
pub const TARGET_EXTENSION: &str = "mp4";

/// Video extensions processed by default.
pub const DEFAULT_EXTENSIONS: &[&str] = &["mp4", "mkv"];

/// Every known video extension except MP4.
pub const OTHER_EXTENSIONS: &[&str] = &["mkv", "avi", "mov", "wmv", "flv", "m4v", "webm", "ts", "mpg", "mpeg"];

/// Every known video extension, shared with `cli-tools-media`.
pub const ALL_EXTENSIONS: &[&str] = cli_tools_media::VIDEO_EXTENSIONS;

/// Hardware HEVC encoder selection.
///
/// `Auto` selects Apple Silicon `hevc_videotoolbox` on `aarch64` macOS and requires `hevc_nvenc` everywhere else.
/// Software encoding is never used as a fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HardwareEncoder {
    /// Auto-select hardware encoder for the host platform (`videotoolbox` on Apple Silicon, `nvenc` elsewhere).
    #[default]
    Auto,
    /// Require NVIDIA NVENC (`hevc_nvenc`).
    Nvenc,
    /// Use Apple Silicon `VideoToolbox` (`hevc_videotoolbox`).
    #[serde(alias = "video_toolbox", alias = "video-toolbox")]
    VideoToolbox,
}

/// Concrete hardware encoder chosen for an ffmpeg invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedEncoder {
    /// NVIDIA NVENC (`hevc_nvenc`).
    Nvenc,
    /// Apple Silicon `VideoToolbox` (`hevc_videotoolbox`).
    VideoToolbox,
}

/// Sort order options for video files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Sort by bitrate (highest first)
    #[default]
    Bitrate,
    /// Sort by file size (largest first)
    Size,
    /// Sort by file size (smallest first)
    SizeAsc,
    /// Sort by duration (longest first)
    Duration,
    /// Sort by duration (shortest first)
    DurationAsc,
    /// Sort by resolution (highest first)
    Resolution,
    /// Sort by resolution (lowest first)
    ResolutionAsc,
    /// Sort by potential savings (bitrate / fps * duration, highest first)
    Impact,
    /// Sort alphabetically by file name
    Name,
}

/// Settings for one conversion run.
#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// Input files and directories.
    pub paths: Vec<PathBuf>,
    /// Whether `paths` were explicitly provided on the command line (used to scope `--from-db`).
    pub explicit_paths: bool,
    /// Lowercase extensions of the files to process.
    pub extensions: Vec<String>,
    /// Only process files whose name contains one of these patterns, when not empty.
    pub include: Vec<String>,
    /// Skip files whose name contains one of these patterns.
    pub exclude: Vec<String>,
    /// Minimum bitrate a file needs to be converted.
    pub min_bitrate: MinimumBitrate,
    /// Maximum bitrate in kbps a file may have to be converted.
    pub max_bitrate: Option<u64>,
    /// Minimum duration in seconds.
    pub min_duration: Option<f64>,
    /// Maximum duration in seconds.
    pub max_duration: Option<f64>,
    /// Skip files where either width or height is below this many pixels.
    pub min_resolution: Option<u32>,
    /// Preserve the MKV container, metadata, and selected stream languages, and mux subtitle sidecars.
    pub movie_mode: bool,
    /// Replace existing output files.
    pub overwrite: bool,
    /// Delete replaced files permanently instead of moving them to the trash.
    pub delete: bool,
    /// Delete a source file when its converted output already exists.
    pub delete_duplicates: bool,
    /// Report what would be done without running ffmpeg or changing any files.
    pub dry_run: bool,
    /// Recurse into subdirectories of the input directories.
    pub recurse: bool,
    /// Skip conversions.
    pub skip_convert: bool,
    /// Skip remuxes and subtitle muxes.
    pub skip_remux: bool,
    /// Process at most this many files.
    pub count: Option<usize>,
    /// Order in which files are processed.
    pub sort: SortOrder,
    /// Hardware HEVC encoder selection (`auto`, `nvenc`, or `videotoolbox`).
    pub encoder: HardwareEncoder,
}

impl HardwareEncoder {
    /// Resolve `Auto` to the concrete hardware encoder for the current platform.
    #[must_use]
    pub const fn resolve(self) -> ResolvedEncoder {
        match self {
            Self::Auto => {
                #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
                {
                    ResolvedEncoder::VideoToolbox
                }
                #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
                {
                    ResolvedEncoder::Nvenc
                }
            }
            Self::Nvenc => ResolvedEncoder::Nvenc,
            Self::VideoToolbox => ResolvedEncoder::VideoToolbox,
        }
    }
}

impl FromStr for HardwareEncoder {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "nvenc" => Ok(Self::Nvenc),
            "videotoolbox" | "video_toolbox" | "video-toolbox" => Ok(Self::VideoToolbox),
            _ => Err(format!("Unknown encoder: {value}")),
        }
    }
}

impl std::fmt::Display for HardwareEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Nvenc => write!(f, "nvenc"),
            Self::VideoToolbox => write!(f, "videotoolbox"),
        }
    }
}

impl SortOrder {
    /// Returns the SQL ORDER BY clause for this sort order.
    #[must_use]
    pub const fn sql_order_clause(self) -> &'static str {
        match self {
            Self::Bitrate => "bitrate_kbps DESC",
            Self::Size => "size_bytes DESC",
            Self::SizeAsc => "size_bytes ASC",
            Self::Duration => "duration DESC",
            Self::DurationAsc => "duration ASC",
            Self::Resolution => "width * height DESC",
            Self::ResolutionAsc => "width * height ASC",
            Self::Impact => "(bitrate_kbps / frames_per_second) * duration DESC",
            Self::Name => "full_path ASC",
        }
    }
}

impl FromStr for SortOrder {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "bitrate" => Ok(Self::Bitrate),
            "size" => Ok(Self::Size),
            "size_asc" | "size-asc" => Ok(Self::SizeAsc),
            "duration" => Ok(Self::Duration),
            "duration_asc" | "duration-asc" => Ok(Self::DurationAsc),
            "resolution" => Ok(Self::Resolution),
            "resolution_asc" | "resolution-asc" => Ok(Self::ResolutionAsc),
            "impact" => Ok(Self::Impact),
            "name" => Ok(Self::Name),
            _ => Err(format!("Unknown sort order: {s}")),
        }
    }
}

impl std::fmt::Display for SortOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Bitrate => "bitrate",
            Self::Size => "size",
            Self::SizeAsc => "size-asc",
            Self::Duration => "duration",
            Self::DurationAsc => "duration-asc",
            Self::Resolution => "resolution",
            Self::ResolutionAsc => "resolution-asc",
            Self::Impact => "impact",
            Self::Name => "name",
        };
        write!(f, "{name}")
    }
}

impl ConvertOptions {
    /// Database query matching the files these options would process.
    #[must_use]
    pub fn pending_file_filter(&self) -> PendingFileFilter {
        PendingFileFilter {
            action: None,
            extensions: self.extensions.clone(),
            path_prefixes: if self.explicit_paths {
                self.paths.clone()
            } else {
                Vec::new()
            },
            min_bitrate: Some(self.min_bitrate),
            max_bitrate: self.max_bitrate,
            min_duration: self.min_duration,
            max_duration: self.max_duration,
            min_resolution: self.min_resolution,
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            limit: self.count,
            sort: Some(self.sort),
        }
    }
}

#[cfg(test)]
mod sort_order_tests {
    use super::*;

    fn parse(s: &str) -> Result<SortOrder, String> {
        <SortOrder as std::str::FromStr>::from_str(s)
    }

    #[test]
    fn from_str_parses_bitrate() {
        assert_eq!(parse("bitrate").expect("bitrate should parse"), SortOrder::Bitrate);
        assert_eq!(
            parse("BITRATE").expect("uppercase bitrate should parse"),
            SortOrder::Bitrate
        );
    }

    #[test]
    fn from_str_parses_size() {
        assert_eq!(parse("size").expect("size should parse"), SortOrder::Size);
        assert_eq!(
            parse("size_asc").expect("snake case size ascending should parse"),
            SortOrder::SizeAsc
        );
        assert_eq!(
            parse("size-asc").expect("kebab case size ascending should parse"),
            SortOrder::SizeAsc
        );
    }

    #[test]
    fn from_str_parses_duration() {
        assert_eq!(parse("duration").expect("duration should parse"), SortOrder::Duration);
        assert_eq!(
            parse("duration_asc").expect("snake case duration ascending should parse"),
            SortOrder::DurationAsc
        );
        assert_eq!(
            parse("duration-asc").expect("kebab case duration ascending should parse"),
            SortOrder::DurationAsc
        );
    }

    #[test]
    fn from_str_parses_resolution() {
        assert_eq!(
            parse("resolution").expect("resolution should parse"),
            SortOrder::Resolution
        );
        assert_eq!(
            parse("resolution_asc").expect("snake case resolution ascending should parse"),
            SortOrder::ResolutionAsc
        );
        assert_eq!(
            parse("resolution-asc").expect("kebab case resolution ascending should parse"),
            SortOrder::ResolutionAsc
        );
    }

    #[test]
    fn from_str_parses_impact() {
        assert_eq!(parse("impact").expect("impact should parse"), SortOrder::Impact);
    }

    #[test]
    fn from_str_parses_name() {
        assert_eq!(parse("name").expect("name should parse"), SortOrder::Name);
    }

    #[test]
    fn from_str_returns_error_for_unknown() {
        let error = parse("unknown").expect_err("unknown sort order should fail");
        assert_eq!(error, "Unknown sort order: unknown");
    }

    #[test]
    fn display_formats_correctly() {
        assert_eq!(format!("{}", SortOrder::Bitrate), "bitrate");
        assert_eq!(format!("{}", SortOrder::Size), "size");
        assert_eq!(format!("{}", SortOrder::SizeAsc), "size-asc");
        assert_eq!(format!("{}", SortOrder::Duration), "duration");
        assert_eq!(format!("{}", SortOrder::DurationAsc), "duration-asc");
        assert_eq!(format!("{}", SortOrder::Resolution), "resolution");
        assert_eq!(format!("{}", SortOrder::ResolutionAsc), "resolution-asc");
        assert_eq!(format!("{}", SortOrder::Impact), "impact");
        assert_eq!(format!("{}", SortOrder::Name), "name");
    }

    #[test]
    fn sql_order_clause_returns_valid_sql() {
        assert_eq!(SortOrder::Bitrate.sql_order_clause(), "bitrate_kbps DESC");
        assert_eq!(SortOrder::Size.sql_order_clause(), "size_bytes DESC");
        assert_eq!(SortOrder::SizeAsc.sql_order_clause(), "size_bytes ASC");
        assert_eq!(SortOrder::Duration.sql_order_clause(), "duration DESC");
        assert_eq!(SortOrder::DurationAsc.sql_order_clause(), "duration ASC");
        assert_eq!(SortOrder::Resolution.sql_order_clause(), "width * height DESC");
        assert_eq!(SortOrder::ResolutionAsc.sql_order_clause(), "width * height ASC");
        assert_eq!(
            SortOrder::Impact.sql_order_clause(),
            "(bitrate_kbps / frames_per_second) * duration DESC"
        );
        assert_eq!(SortOrder::Name.sql_order_clause(), "full_path ASC");
    }

    #[test]
    fn default_is_bitrate() {
        assert_eq!(SortOrder::default(), SortOrder::Bitrate);
    }
}

#[cfg(test)]
mod test_pending_file_filter {
    use super::*;

    #[test]
    fn filter_carries_the_limits_count_and_sort_order() {
        let options = ConvertOptions {
            extensions: vec!["mkv".to_string()],
            min_bitrate: MinimumBitrate::Fixed(9000),
            max_bitrate: Some(50_000),
            min_duration: Some(60.0),
            max_duration: Some(7200.0),
            count: Some(12),
            sort: SortOrder::DurationAsc,
            ..ConvertOptions::default()
        };

        let filter = options.pending_file_filter();

        assert!(filter.action.is_none());
        assert_eq!(filter.extensions, ["mkv"]);
        assert_eq!(filter.min_bitrate, Some(MinimumBitrate::Fixed(9000)));
        assert_eq!(filter.max_bitrate, Some(50_000));
        assert_eq!(filter.min_duration, Some(60.0));
        assert_eq!(filter.max_duration, Some(7200.0));
        assert_eq!(filter.limit, Some(12));
        assert_eq!(filter.sort, Some(SortOrder::DurationAsc));
    }
}
