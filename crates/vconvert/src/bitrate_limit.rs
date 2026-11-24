//! Minimum bitrate limits for choosing which videos to convert.
//!
//! Defines the default thresholds by resolution tier and framerate, their config file overrides,
//! and the matching SQL expression used when filtering pending files from the database.

use std::fmt;

use serde::Deserialize;

/// Framerates above this are treated as high framerate.
const HIGH_FRAME_RATE_THRESHOLD: f64 = 30.5;

/// Largest equivalent height that still belongs to the 720p tier.
const HD_720_MAX_HEIGHT: u64 = 900;

/// Largest equivalent height that still belongs to the 1080p tier.
const HD_1080_MAX_HEIGHT: u64 = 1260;

/// SQL expression computing the same equivalent height as [`equivalent_height`].
const SQL_EQUIVALENT_HEIGHT: &str = "MAX(MIN(width, height), MAX(width, height) * 9 / 16)";

/// Resolution tier used to pick a default minimum bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionTier {
    /// 720p and below.
    Hd720,
    /// 1080p.
    Hd1080,
    /// 1440p and above.
    Qhd1440,
}

/// Minimum bitrate a file must have to be converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinimumBitrate {
    /// The same limit in kbps for every file.
    Fixed(u64),
    /// Limit depends on the file resolution and framerate.
    Tiered(BitrateTiers),
}

/// Minimum bitrate in kbps for each resolution tier and framerate class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateTiers {
    hd_720: u64,
    hd_720_high_fps: u64,
    hd_1080: u64,
    hd_1080_high_fps: u64,
    qhd_1440: u64,
    qhd_1440_high_fps: u64,
}

/// Optional per tier overrides from the `[video_convert.bitrate_tiers]` config table.
#[derive(Debug, Default, Deserialize)]
pub struct BitrateTiersConfig {
    #[serde(default, rename = "720p")]
    hd_720: Option<u64>,
    #[serde(default, rename = "720p_high_fps")]
    hd_720_high_fps: Option<u64>,
    #[serde(default, rename = "1080p")]
    hd_1080: Option<u64>,
    #[serde(default, rename = "1080p_high_fps")]
    hd_1080_high_fps: Option<u64>,
    #[serde(default, rename = "1440p")]
    qhd_1440: Option<u64>,
    #[serde(default, rename = "1440p_high_fps")]
    qhd_1440_high_fps: Option<u64>,
}

impl ResolutionTier {
    /// Classify a video by its dimensions, treating portrait and non 16:9 videos by their 16:9 equivalent.
    pub fn from_dimensions(width: u32, height: u32) -> Self {
        let equivalent_height = equivalent_height(width, height);
        if equivalent_height <= HD_720_MAX_HEIGHT {
            Self::Hd720
        } else if equivalent_height <= HD_1080_MAX_HEIGHT {
            Self::Hd1080
        } else {
            Self::Qhd1440
        }
    }
}

impl MinimumBitrate {
    /// Return the minimum bitrate in kbps for a video with the given properties.
    pub fn threshold(&self, width: u32, height: u32, frames_per_second: f64) -> u64 {
        match self {
            Self::Fixed(limit) => *limit,
            Self::Tiered(tiers) => tiers.threshold(
                ResolutionTier::from_dimensions(width, height),
                is_high_frame_rate(frames_per_second),
            ),
        }
    }

    /// Build a SQL expression giving the minimum bitrate for each `pending_files` row.
    ///
    /// Bitrate values are pushed to `parameters` and referenced by position.
    pub fn sql_threshold(&self, parameters: &mut Vec<Box<dyn rusqlite::ToSql>>) -> String {
        let mut push = |value: u64| {
            parameters.push(Box::new(value.cast_signed()));
            format!("?{}", parameters.len())
        };
        match self {
            Self::Fixed(limit) => push(*limit),
            Self::Tiered(tiers) => {
                let mut frame_rate_case = |high: u64, standard: u64| {
                    let high = push(high);
                    let standard = push(standard);
                    format!("CASE WHEN frames_per_second > {HIGH_FRAME_RATE_THRESHOLD} THEN {high} ELSE {standard} END")
                };
                let hd_720 = frame_rate_case(tiers.hd_720_high_fps, tiers.hd_720);
                let hd_1080 = frame_rate_case(tiers.hd_1080_high_fps, tiers.hd_1080);
                let qhd_1440 = frame_rate_case(tiers.qhd_1440_high_fps, tiers.qhd_1440);
                format!(
                    "(CASE WHEN {SQL_EQUIVALENT_HEIGHT} <= {HD_720_MAX_HEIGHT} THEN {hd_720} \
                     WHEN {SQL_EQUIVALENT_HEIGHT} <= {HD_1080_MAX_HEIGHT} THEN {hd_1080} \
                     ELSE {qhd_1440} END)"
                )
            }
        }
    }
}

impl BitrateTiers {
    /// Return the minimum bitrate for a resolution tier and framerate class.
    pub const fn threshold(&self, tier: ResolutionTier, high_frame_rate: bool) -> u64 {
        match (tier, high_frame_rate) {
            (ResolutionTier::Hd720, false) => self.hd_720,
            (ResolutionTier::Hd720, true) => self.hd_720_high_fps,
            (ResolutionTier::Hd1080, false) => self.hd_1080,
            (ResolutionTier::Hd1080, true) => self.hd_1080_high_fps,
            (ResolutionTier::Qhd1440, false) => self.qhd_1440,
            (ResolutionTier::Qhd1440, true) => self.qhd_1440_high_fps,
        }
    }
}

impl BitrateTiersConfig {
    /// Resolve configured overrides on top of the default tiers.
    pub fn resolve(&self) -> BitrateTiers {
        let defaults = BitrateTiers::default();
        BitrateTiers {
            hd_720: self.hd_720.unwrap_or(defaults.hd_720),
            hd_720_high_fps: self.hd_720_high_fps.unwrap_or(defaults.hd_720_high_fps),
            hd_1080: self.hd_1080.unwrap_or(defaults.hd_1080),
            hd_1080_high_fps: self.hd_1080_high_fps.unwrap_or(defaults.hd_1080_high_fps),
            qhd_1440: self.qhd_1440.unwrap_or(defaults.qhd_1440),
            qhd_1440_high_fps: self.qhd_1440_high_fps.unwrap_or(defaults.qhd_1440_high_fps),
        }
    }
}

impl Default for MinimumBitrate {
    fn default() -> Self {
        Self::Tiered(BitrateTiers::default())
    }
}

impl Default for BitrateTiers {
    fn default() -> Self {
        Self {
            hd_720: 4000,
            hd_720_high_fps: 6000,
            hd_1080: 8000,
            hd_1080_high_fps: 12000,
            qhd_1440: 16000,
            qhd_1440_high_fps: 24000,
        }
    }
}

impl fmt::Display for MinimumBitrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(limit) => write!(f, "{limit} kbps"),
            Self::Tiered(tiers) => write!(
                f,
                "720p {}/{}, 1080p {}/{}, 1440p+ {}/{} kbps (up to 30 fps / above 30 fps)",
                tiers.hd_720,
                tiers.hd_720_high_fps,
                tiers.hd_1080,
                tiers.hd_1080_high_fps,
                tiers.qhd_1440,
                tiers.qhd_1440_high_fps
            ),
        }
    }
}

/// Height of the 16:9 frame the video fits in, so portrait and ultrawide videos land in the expected tier.
fn equivalent_height(width: u32, height: u32) -> u64 {
    let shorter = u64::from(width.min(height));
    let longer = u64::from(width.max(height));
    shorter.max(longer * 9 / 16)
}

/// Return whether the framerate counts as high framerate.
fn is_high_frame_rate(frames_per_second: f64) -> bool {
    frames_per_second > HIGH_FRAME_RATE_THRESHOLD
}

#[cfg(test)]
mod test_resolution_tier {
    use super::*;

    #[test]
    fn classifies_standard_resolutions() {
        assert_eq!(ResolutionTier::from_dimensions(640, 480), ResolutionTier::Hd720);
        assert_eq!(ResolutionTier::from_dimensions(720, 576), ResolutionTier::Hd720);
        assert_eq!(ResolutionTier::from_dimensions(1280, 720), ResolutionTier::Hd720);
        assert_eq!(ResolutionTier::from_dimensions(1920, 1080), ResolutionTier::Hd1080);
        assert_eq!(ResolutionTier::from_dimensions(2560, 1440), ResolutionTier::Qhd1440);
        assert_eq!(ResolutionTier::from_dimensions(3840, 2160), ResolutionTier::Qhd1440);
    }

    #[test]
    fn classifies_portrait_by_shorter_side() {
        assert_eq!(ResolutionTier::from_dimensions(720, 1280), ResolutionTier::Hd720);
        assert_eq!(ResolutionTier::from_dimensions(1080, 1920), ResolutionTier::Hd1080);
    }

    #[test]
    fn classifies_ultrawide_by_width() {
        assert_eq!(ResolutionTier::from_dimensions(1920, 800), ResolutionTier::Hd1080);
        assert_eq!(ResolutionTier::from_dimensions(3840, 1600), ResolutionTier::Qhd1440);
    }

    #[test]
    fn classifies_anamorphic_by_height() {
        assert_eq!(ResolutionTier::from_dimensions(1440, 1080), ResolutionTier::Hd1080);
    }

    #[test]
    fn tier_boundaries_are_inclusive() {
        assert_eq!(ResolutionTier::from_dimensions(1600, 900), ResolutionTier::Hd720);
        assert_eq!(ResolutionTier::from_dimensions(1602, 901), ResolutionTier::Hd1080);
        assert_eq!(ResolutionTier::from_dimensions(2240, 1260), ResolutionTier::Hd1080);
        assert_eq!(ResolutionTier::from_dimensions(2242, 1261), ResolutionTier::Qhd1440);
    }

    #[test]
    fn zero_dimensions_use_lowest_tier() {
        assert_eq!(ResolutionTier::from_dimensions(0, 0), ResolutionTier::Hd720);
    }
}

#[cfg(test)]
mod test_minimum_bitrate_threshold {
    use super::*;

    #[test]
    fn fixed_ignores_resolution_and_frame_rate() {
        let limit = MinimumBitrate::Fixed(7000);
        assert_eq!(limit.threshold(1280, 720, 24.0), 7000);
        assert_eq!(limit.threshold(3840, 2160, 60.0), 7000);
    }

    #[test]
    fn default_tiers_by_resolution_and_frame_rate() {
        let limit = MinimumBitrate::default();
        assert_eq!(limit.threshold(1280, 720, 29.97), 4000);
        assert_eq!(limit.threshold(1280, 720, 59.94), 6000);
        assert_eq!(limit.threshold(1920, 1080, 23.976), 8000);
        assert_eq!(limit.threshold(1920, 1080, 60.0), 12000);
        assert_eq!(limit.threshold(2560, 1440, 30.0), 16000);
        assert_eq!(limit.threshold(3840, 2160, 50.0), 24000);
    }

    #[test]
    fn thirty_fps_is_standard_frame_rate() {
        assert!(!is_high_frame_rate(30.0));
        assert!(!is_high_frame_rate(29.97));
        assert!(is_high_frame_rate(48.0));
        assert!(is_high_frame_rate(50.0));
    }
}

#[cfg(test)]
mod test_bitrate_tiers_config {
    use super::*;

    #[test]
    fn empty_config_resolves_to_defaults() {
        assert_eq!(BitrateTiersConfig::default().resolve(), BitrateTiers::default());
    }

    #[test]
    fn partial_config_overrides_only_given_tiers() {
        let config: BitrateTiersConfig = toml::from_str(
            r#"
"720p" = 3000
1080p_high_fps = 10000
"#,
        )
        .expect("bitrate tiers should parse");
        let tiers = config.resolve();
        assert_eq!(tiers.threshold(ResolutionTier::Hd720, false), 3000);
        assert_eq!(tiers.threshold(ResolutionTier::Hd720, true), 6000);
        assert_eq!(tiers.threshold(ResolutionTier::Hd1080, false), 8000);
        assert_eq!(tiers.threshold(ResolutionTier::Hd1080, true), 10000);
        assert_eq!(tiers.threshold(ResolutionTier::Qhd1440, false), 16000);
    }
}

#[cfg(test)]
mod test_minimum_bitrate_display {
    use super::*;

    #[test]
    fn displays_fixed_limit() {
        assert_eq!(MinimumBitrate::Fixed(8000).to_string(), "8000 kbps");
    }

    #[test]
    fn displays_all_tiers() {
        assert_eq!(
            MinimumBitrate::default().to_string(),
            "720p 4000/6000, 1080p 8000/12000, 1440p+ 16000/24000 kbps (up to 30 fps / above 30 fps)"
        );
    }
}
