use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use colored::Colorize;

const RESOLUTION_TOLERANCE: f32 = 0.025;
const KNOWN_RESOLUTIONS: &[(u32, u32)] = &[
    (640, 480),
    (720, 480),
    (720, 540),
    (720, 544),
    (720, 576),
    (800, 600),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const FUZZY_RESOLUTIONS: [ResolutionMatch; KNOWN_RESOLUTIONS.len()] = precalculate_fuzzy_resolutions();

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
pub struct Resolution {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Copy, Clone, Debug)]
struct ResolutionMatch {
    label_height: u32,
    width_range: (u32, u32),
    height_range: (u32, u32),
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
pub struct FFProbeResult {
    pub(crate) file: PathBuf,
    pub(crate) resolution: Resolution,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.width < self.height {
            write!(f, "Vertical.{}x{}", self.width, self.height)
        } else {
            write!(f, "{}x{}", self.width, self.height)
        }
    }
}

impl fmt::Display for ResolutionMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}p: width {:?}, height {:?}",
            self.label_height, self.width_range, self.height_range
        )
    }
}

impl FFProbeResult {
    pub(crate) fn delete(&self, dryrun: bool) -> anyhow::Result<()> {
        let path_str = cli_tools::path_to_string_relative(&self.file);
        println!(
            "{:>4}x{:<4}   {}",
            self.resolution.width,
            self.resolution.height,
            path_str.red()
        );
        if !dryrun {
            cli_tools::trash_or_delete(&self.file)?;
        }
        Ok(())
    }

    pub(crate) fn rename(&self, new_path: &Path, overwrite: bool, dryrun: bool) -> anyhow::Result<()> {
        self.print_rename(new_path);
        if new_path.exists() && !overwrite {
            anyhow::bail!("File already exists: {}", cli_tools::path_to_string(new_path));
        }
        if !dryrun {
            std::fs::rename(&self.file, new_path)?;
        }
        Ok(())
    }

    /// Returns `Some(new_path)` if file needs renaming, `None` if already up-to-date.
    pub(crate) fn new_path_if_needed(&self) -> anyhow::Result<Option<PathBuf>> {
        let label = self.resolution.label();
        let (mut name, extension) = cli_tools::get_normalized_file_name_and_extension(&self.file)?;
        if name.contains(&*label) {
            Ok(None)
        } else {
            let full_resolution = self.resolution.to_string();
            if name.contains(&full_resolution) {
                name = name.replace(&full_resolution, "");
            }
            let new_file_name = format!("{name}.{label}.{extension}").replace("..", ".");
            let new_path = self.file.with_file_name(&new_file_name);
            Ok(Some(new_path))
        }
    }

    fn print_rename(&self, new_path: &Path) {
        let (_, new_path_colored) = cli_tools::color_diff(
            &cli_tools::path_to_string_relative(&self.file),
            &cli_tools::path_to_string_relative(new_path),
            false,
        );
        println!(
            "{:>4}x{:<4}   {:>18}   {}",
            self.resolution.width,
            self.resolution.height,
            self.resolution.label(),
            new_path_colored
        );
    }
}

impl Resolution {
    /// Returns true if width or height is smaller than the given limit.
    pub(crate) const fn is_smaller_than(&self, limit: u32) -> bool {
        self.width < limit || self.height < limit
    }

    fn label(&self) -> Cow<'static, str> {
        if self.width < self.height {
            // Vertical video
            match (self.width, self.height) {
                (480, 640 | 720) => Cow::Borrowed("Vertical.480p"),
                (540, 720) => Cow::Borrowed("Vertical.540p"),
                (544, 720) => Cow::Borrowed("Vertical.544p"),
                (576, 720) => Cow::Borrowed("Vertical.576p"),
                (600, 800) => Cow::Borrowed("Vertical.600p"),
                (720, 1280) => Cow::Borrowed("Vertical.720p"),
                (1080, 1920) => Cow::Borrowed("Vertical.1080p"),
                (1440, 2560) => Cow::Borrowed("Vertical.1440p"),
                (2160, 3840) => Cow::Borrowed("Vertical.2160p"),
                _ => self.label_fuzzy_vertical(),
            }
        } else {
            // Horizontal video
            match (self.width, self.height) {
                (640 | 720, 480) => Cow::Borrowed("480p"),
                (720, 540) => Cow::Borrowed("540p"),
                (720, 544) => Cow::Borrowed("544p"),
                (720, 576) => Cow::Borrowed("576p"),
                (800, 600) => Cow::Borrowed("600p"),
                (1280, 720) => Cow::Borrowed("720p"),
                (1920, 1080) => Cow::Borrowed("1080p"),
                (2560, 1440) => Cow::Borrowed("1440p"),
                (3840, 2160) => Cow::Borrowed("2160p"),
                _ => self.label_fuzzy_horizontal(),
            }
        }
    }

    fn label_fuzzy_vertical(&self) -> Cow<'static, str> {
        for res in &FUZZY_RESOLUTIONS {
            if self.height >= res.width_range.0
                && self.height <= res.width_range.1
                && self.width >= res.height_range.0
                && self.width <= res.height_range.1
            {
                return Cow::Owned(format!("Vertical.{}p", res.label_height));
            }
        }
        // fall back to full resolution label
        Cow::Owned(self.to_string())
    }

    fn label_fuzzy_horizontal(&self) -> Cow<'static, str> {
        for res in &FUZZY_RESOLUTIONS {
            if self.width >= res.width_range.0
                && self.width <= res.width_range.1
                && self.height >= res.height_range.0
                && self.height <= res.height_range.1
            {
                return Cow::Owned(format!("{}p", res.label_height));
            }
        }
        // fall back to full resolution label
        Cow::Owned(self.to_string())
    }
}

pub fn print_fuzzy_resolution_ranges() {
    println!("Fuzzy resolution ranges:");
    for res in &FUZZY_RESOLUTIONS {
        println!("  {res}");
    }
}

const fn precalculate_fuzzy_resolutions() -> [ResolutionMatch; KNOWN_RESOLUTIONS.len()] {
    let mut out = [ResolutionMatch {
        label_height: 0,
        width_range: (0, 0),
        height_range: (0, 0),
    }; KNOWN_RESOLUTIONS.len()];
    let mut i = 0;
    while i < KNOWN_RESOLUTIONS.len() {
        let (w, h) = KNOWN_RESOLUTIONS[i];
        out[i] = ResolutionMatch {
            label_height: h,
            width_range: compute_bounds(w),
            height_range: compute_bounds(h),
        };
        i += 1;
    }
    out
}

const fn compute_bounds(res: u32) -> (u32, u32) {
    let tolerance = (res as f32 * RESOLUTION_TOLERANCE) as u32;
    let min = res.saturating_sub(tolerance);
    let max = res.saturating_add(tolerance);
    (min, max)
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn exact_matches() {
        assert_eq!(
            Resolution {
                width: 1280,
                height: 720
            }
            .label(),
            "720p"
        );
        assert_eq!(
            Resolution {
                width: 1920,
                height: 1080
            }
            .label(),
            "1080p"
        );
        assert_eq!(
            Resolution {
                width: 2560,
                height: 1440
            }
            .label(),
            "1440p"
        );
        assert_eq!(
            Resolution {
                width: 3840,
                height: 2160
            }
            .label(),
            "2160p"
        );
    }

    #[test]
    fn exact_matches_vertical() {
        assert_eq!(
            Resolution {
                width: 720,
                height: 1280
            }
            .label(),
            "Vertical.720p"
        );
        assert_eq!(
            Resolution {
                width: 1080,
                height: 1920
            }
            .label(),
            "Vertical.1080p"
        );
        assert_eq!(
            Resolution {
                width: 1440,
                height: 2560
            }
            .label(),
            "Vertical.1440p"
        );
        assert_eq!(
            Resolution {
                width: 2160,
                height: 3840
            }
            .label(),
            "Vertical.2160p"
        );
    }

    #[test]
    fn approximate_matches() {
        assert_eq!(
            Resolution {
                width: 1920,
                height: 1078
            }
            .label(),
            "1080p"
        );
        assert_eq!(
            Resolution {
                width: 1278,
                height: 716
            }
            .label(),
            "720p"
        );
        assert_eq!(
            Resolution {
                width: 2540,
                height: 1442
            }
            .label(),
            "1440p"
        );
        assert_eq!(
            Resolution {
                width: 1442,
                height: 2540
            }
            .label(),
            "Vertical.1440p"
        );
        assert_eq!(
            Resolution {
                width: 3820,
                height: 2162
            }
            .label(),
            "2160p"
        );
        assert_eq!(
            Resolution {
                width: 1260,
                height: 710
            }
            .label(),
            "720p"
        );
    }

    #[test]
    fn out_of_range() {
        assert_eq!(
            Resolution {
                width: 1024,
                height: 768
            }
            .label(),
            "1024x768"
        );
        assert_eq!(
            Resolution {
                width: 3000,
                height: 2000
            }
            .label(),
            "3000x2000"
        );
    }

    #[test]
    fn lower_bound_tolerance() {
        assert_eq!(
            Resolution {
                width: 1267,
                height: 713
            }
            .label(),
            "720p"
        );
    }

    #[test]
    fn upper_bound_tolerance() {
        assert_eq!(
            Resolution {
                width: 1292,
                height: 727
            }
            .label(),
            "720p"
        );
    }

    #[test]
    fn beyond_tolerance() {
        assert_eq!(
            Resolution {
                width: 1250,
                height: 790
            }
            .label(),
            "1250x790"
        );
    }

    #[test]
    fn exact_matches_480p() {
        assert_eq!(
            Resolution {
                width: 640,
                height: 480
            }
            .label(),
            "480p"
        );
        assert_eq!(
            Resolution {
                width: 720,
                height: 480
            }
            .label(),
            "480p"
        );
    }

    #[test]
    fn exact_matches_540p() {
        assert_eq!(
            Resolution {
                width: 720,
                height: 540
            }
            .label(),
            "540p"
        );
    }

    #[test]
    fn exact_matches_544p() {
        assert_eq!(
            Resolution {
                width: 720,
                height: 544
            }
            .label(),
            "544p"
        );
    }

    #[test]
    fn exact_matches_576p() {
        assert_eq!(
            Resolution {
                width: 720,
                height: 576
            }
            .label(),
            "576p"
        );
    }

    #[test]
    fn exact_matches_600p() {
        assert_eq!(
            Resolution {
                width: 800,
                height: 600
            }
            .label(),
            "600p"
        );
    }

    #[test]
    fn exact_matches_vertical_480p() {
        assert_eq!(
            Resolution {
                width: 480,
                height: 640
            }
            .label(),
            "Vertical.480p"
        );
        assert_eq!(
            Resolution {
                width: 480,
                height: 720
            }
            .label(),
            "Vertical.480p"
        );
    }

    #[test]
    fn exact_matches_vertical_540p() {
        assert_eq!(
            Resolution {
                width: 540,
                height: 720
            }
            .label(),
            "Vertical.540p"
        );
    }

    #[test]
    fn exact_matches_vertical_576p() {
        assert_eq!(
            Resolution {
                width: 576,
                height: 720
            }
            .label(),
            "Vertical.576p"
        );
    }

    #[test]
    fn exact_matches_vertical_600p() {
        assert_eq!(
            Resolution {
                width: 600,
                height: 800
            }
            .label(),
            "Vertical.600p"
        );
    }

    #[test]
    fn fuzzy_matches_horizontal_near_boundaries() {
        // Just inside lower tolerance for 1080p
        assert_eq!(
            Resolution {
                width: 1872,
                height: 1053
            }
            .label(),
            "1080p"
        );
        // Just inside upper tolerance for 1080p
        assert_eq!(
            Resolution {
                width: 1968,
                height: 1107
            }
            .label(),
            "1080p"
        );
    }

    #[test]
    fn fuzzy_matches_vertical_near_boundaries() {
        // Fuzzy vertical 1080p
        assert_eq!(
            Resolution {
                width: 1078,
                height: 1918
            }
            .label(),
            "Vertical.1080p"
        );
    }

    #[test]
    fn fuzzy_matches_4k_variations() {
        assert_eq!(
            Resolution {
                width: 3800,
                height: 2140
            }
            .label(),
            "2160p"
        );
        assert_eq!(
            Resolution {
                width: 3860,
                height: 2170
            }
            .label(),
            "2160p"
        );
    }

    #[test]
    fn out_of_range_small_resolutions() {
        assert_eq!(
            Resolution {
                width: 320,
                height: 240
            }
            .label(),
            "320x240"
        );
        assert_eq!(
            Resolution {
                width: 160,
                height: 120
            }
            .label(),
            "160x120"
        );
    }

    #[test]
    fn out_of_range_unusual_aspect_ratios() {
        // Ultra-wide
        assert_eq!(
            Resolution {
                width: 2560,
                height: 1080
            }
            .label(),
            "2560x1080"
        );
        // Very tall
        assert_eq!(
            Resolution {
                width: 500,
                height: 2000
            }
            .label(),
            "Vertical.500x2000"
        );
    }

    #[test]
    fn out_of_range_between_known_resolutions() {
        // Between 720p and 1080p
        assert_eq!(
            Resolution {
                width: 1600,
                height: 900
            }
            .label(),
            "1600x900"
        );
    }

    #[test]
    fn label_544p_vertical() {
        assert_eq!(
            Resolution {
                width: 544,
                height: 720
            }
            .label(),
            "Vertical.544p"
        );
    }

    #[test]
    fn label_unknown_resolution_horizontal() {
        let res = Resolution {
            width: 1234,
            height: 567,
        };
        assert_eq!(res.label(), "1234x567");
    }

    #[test]
    fn label_unknown_resolution_vertical() {
        let res = Resolution {
            width: 567,
            height: 1234,
        };
        assert_eq!(res.label(), "Vertical.567x1234");
    }

    #[test]
    fn label_square_treated_as_horizontal() {
        let res = Resolution {
            width: 500,
            height: 500,
        };
        // Square (width == height) should be treated as horizontal
        assert_eq!(res.label(), "500x500");
        assert!(!res.label().contains("Vertical"));
    }

    #[test]
    fn label_8k_horizontal() {
        let res = Resolution {
            width: 7680,
            height: 4320,
        };
        // 8K is not a known resolution, should return full dimensions
        assert_eq!(res.label(), "7680x4320");
    }

    #[test]
    fn label_8k_vertical() {
        let res = Resolution {
            width: 4320,
            height: 7680,
        };
        assert_eq!(res.label(), "Vertical.4320x7680");
    }

    #[test]
    fn label_ultrawide_1440p() {
        // Ultrawide 1440p (3440x1440) should not match standard 1440p
        let res = Resolution {
            width: 3440,
            height: 1440,
        };
        assert_eq!(res.label(), "3440x1440");
    }

    #[test]
    fn label_near_720p_within_tolerance() {
        // Within 2.5% tolerance of 720p
        let res = Resolution {
            width: 1270,
            height: 715,
        };
        assert_eq!(res.label(), "720p");
    }

    #[test]
    fn label_near_720p_outside_tolerance() {
        // Outside 2.5% tolerance of 720p
        let res = Resolution {
            width: 1200,
            height: 680,
        };
        assert_eq!(res.label(), "1200x680");
    }

    #[test]
    fn label_1080p_slightly_cropped() {
        // Common scenario: 1920x1072 (slightly cropped 1080p)
        let res = Resolution {
            width: 1920,
            height: 1072,
        };
        assert_eq!(res.label(), "1080p");
    }

    #[test]
    fn label_4k_dci() {
        // DCI 4K (4096x2160) should not match UHD 4K
        let res = Resolution {
            width: 4096,
            height: 2160,
        };
        // Width is outside tolerance for 3840
        assert_eq!(res.label(), "4096x2160");
    }

    #[test]
    fn label_vertical_1080p_slightly_off() {
        let res = Resolution {
            width: 1078,
            height: 1918,
        };
        assert_eq!(res.label(), "Vertical.1080p");
    }

    #[test]
    fn label_1440p_exact() {
        let res = Resolution {
            width: 2560,
            height: 1440,
        };
        assert_eq!(res.label(), "1440p");
    }

    #[test]
    fn label_vertical_1440p_exact() {
        let res = Resolution {
            width: 1440,
            height: 2560,
        };
        assert_eq!(res.label(), "Vertical.1440p");
    }

    #[test]
    fn label_sd_resolutions() {
        assert_eq!(
            Resolution {
                width: 640,
                height: 480
            }
            .label(),
            "480p"
        );
        assert_eq!(
            Resolution {
                width: 720,
                height: 576
            }
            .label(),
            "576p"
        );
    }

    #[test]
    fn label_very_small_resolution() {
        let res = Resolution { width: 64, height: 64 };
        assert_eq!(res.label(), "64x64");
    }

    #[test]
    fn label_single_pixel() {
        let res = Resolution { width: 1, height: 1 };
        assert_eq!(res.label(), "1x1");
    }

    #[test]
    fn label_zero_dimensions() {
        let res = Resolution { width: 0, height: 0 };
        assert_eq!(res.label(), "0x0");
    }
}

#[cfg(test)]
mod resolution_new_tests {
    use super::*;

    #[test]
    fn new_creates_resolution_with_correct_dimensions() {
        let res = Resolution::new(1920, 1080);
        assert_eq!(res.width, 1920);
        assert_eq!(res.height, 1080);
    }

    #[test]
    fn new_with_zero_dimensions() {
        let res = Resolution::new(0, 0);
        assert_eq!(res.width, 0);
        assert_eq!(res.height, 0);
    }

    #[test]
    fn new_with_max_u32() {
        let res = Resolution::new(u32::MAX, u32::MAX);
        assert_eq!(res.width, u32::MAX);
        assert_eq!(res.height, u32::MAX);
    }

    #[test]
    fn new_with_asymmetric_dimensions() {
        let res = Resolution::new(7680, 4320);
        assert_eq!(res.width, 7680);
        assert_eq!(res.height, 4320);
    }
}

#[cfg(test)]
mod resolution_struct_tests {
    use super::*;

    #[test]
    fn display_horizontal_resolution() {
        let res = Resolution {
            width: 1920,
            height: 1080,
        };
        assert_eq!(format!("{res}"), "1920x1080");
    }

    #[test]
    fn display_vertical_resolution() {
        let res = Resolution {
            width: 1080,
            height: 1920,
        };
        assert_eq!(format!("{res}"), "Vertical.1080x1920");
    }

    #[test]
    fn display_square_resolution() {
        let res = Resolution {
            width: 1080,
            height: 1080,
        };
        // Square is treated as horizontal (width >= height)
        assert_eq!(format!("{res}"), "1080x1080");
    }

    #[test]
    fn resolution_ordering() {
        let res_720p = Resolution {
            width: 1280,
            height: 720,
        };
        let res_1080p = Resolution {
            width: 1920,
            height: 1080,
        };
        assert!(res_720p < res_1080p);
    }

    #[test]
    fn resolution_equality() {
        let res1 = Resolution {
            width: 1920,
            height: 1080,
        };
        let res2 = Resolution {
            width: 1920,
            height: 1080,
        };
        assert_eq!(res1, res2);
    }

    #[test]
    fn resolution_inequality() {
        let res1 = Resolution {
            width: 1920,
            height: 1080,
        };
        let res2 = Resolution {
            width: 1920,
            height: 1079,
        };
        assert_ne!(res1, res2);
    }

    #[test]
    fn resolution_ordering_by_width_first() {
        let res1 = Resolution {
            width: 1280,
            height: 1080,
        };
        let res2 = Resolution {
            width: 1920,
            height: 720,
        };
        // Ordering is by width first, then height
        assert!(res1 < res2);
    }

    #[test]
    fn resolution_ordering_same_width_different_height() {
        let res1 = Resolution {
            width: 1920,
            height: 1080,
        };
        let res2 = Resolution {
            width: 1920,
            height: 1440,
        };
        assert!(res1 < res2);
    }

    #[test]
    fn resolution_ordering_vertical_vs_horizontal() {
        let vertical = Resolution {
            width: 1080,
            height: 1920,
        };
        let horizontal = Resolution {
            width: 1920,
            height: 1080,
        };
        // Width is compared first, so vertical (1080) < horizontal (1920)
        assert!(vertical < horizontal);
    }

    #[test]
    fn display_small_resolution() {
        let res = Resolution {
            width: 320,
            height: 240,
        };
        assert_eq!(format!("{res}"), "320x240");
    }

    #[test]
    fn display_4k_resolution() {
        let res = Resolution {
            width: 3840,
            height: 2160,
        };
        assert_eq!(format!("{res}"), "3840x2160");
    }

    #[test]
    fn display_8k_resolution() {
        let res = Resolution {
            width: 7680,
            height: 4320,
        };
        assert_eq!(format!("{res}"), "7680x4320");
    }

    #[test]
    fn display_ultrawide_resolution() {
        let res = Resolution {
            width: 3440,
            height: 1440,
        };
        assert_eq!(format!("{res}"), "3440x1440");
    }

    #[test]
    fn display_vertical_4k() {
        let res = Resolution {
            width: 2160,
            height: 3840,
        };
        assert_eq!(format!("{res}"), "Vertical.2160x3840");
    }

    #[test]
    fn resolution_debug_format() {
        let res = Resolution {
            width: 1920,
            height: 1080,
        };
        let debug_str = format!("{res:?}");
        assert!(debug_str.contains("Resolution"));
        assert!(debug_str.contains("1920"));
        assert!(debug_str.contains("1080"));
    }
}

#[cfg(test)]
mod is_smaller_than_tests {
    use super::*;

    #[test]
    fn width_below_limit() {
        let res = Resolution {
            width: 400,
            height: 720,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn height_below_limit() {
        let res = Resolution {
            width: 720,
            height: 400,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn both_below_limit() {
        let res = Resolution {
            width: 320,
            height: 240,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn both_above_limit() {
        let res = Resolution {
            width: 1920,
            height: 1080,
        };
        assert!(!res.is_smaller_than(480));
    }

    #[test]
    fn at_exact_limit() {
        let res = Resolution {
            width: 480,
            height: 480,
        };
        assert!(!res.is_smaller_than(480));
    }

    #[test]
    fn one_at_limit_one_below() {
        let res = Resolution {
            width: 480,
            height: 479,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn zero_limit() {
        let res = Resolution {
            width: 100,
            height: 100,
        };
        assert!(!res.is_smaller_than(0));
    }

    #[test]
    fn zero_resolution() {
        let res = Resolution { width: 0, height: 0 };
        assert!(res.is_smaller_than(1));
        assert!(!res.is_smaller_than(0));
    }

    #[test]
    fn large_limit() {
        let res = Resolution {
            width: 3840,
            height: 2160,
        };
        assert!(res.is_smaller_than(4000));
        assert!(!res.is_smaller_than(2160));
    }

    #[test]
    fn vertical_video_width_below() {
        let res = Resolution {
            width: 360,
            height: 640,
        };
        assert!(res.is_smaller_than(480));
    }

    #[test]
    fn vertical_video_both_above() {
        let res = Resolution {
            width: 1080,
            height: 1920,
        };
        assert!(!res.is_smaller_than(720));
    }

    #[test]
    fn max_u32_limit() {
        let res = Resolution {
            width: 1920,
            height: 1080,
        };
        // Both dimensions are smaller than u32::MAX, so this returns true
        assert!(res.is_smaller_than(u32::MAX));
    }

    #[test]
    fn one_dimension_at_limit() {
        let res = Resolution {
            width: 720,
            height: 480,
        };
        // Width is at 720, height at 480 - neither below 480
        assert!(!res.is_smaller_than(480));
    }
}

#[cfg(test)]
mod fuzzy_resolution_tests {
    use super::*;

    #[test]
    fn fuzzy_resolutions_count_matches_known() {
        assert_eq!(FUZZY_RESOLUTIONS.len(), KNOWN_RESOLUTIONS.len());
    }

    #[test]
    fn fuzzy_resolution_1080p_bounds() {
        // Find the 1080p entry
        let res_1080p = FUZZY_RESOLUTIONS
            .iter()
            .find(|r| r.label_height == 1080)
            .expect("1080p should exist in fuzzy resolutions");

        // Width should be around 1920 with tolerance
        assert!(res_1080p.width_range.0 < 1920);
        assert!(res_1080p.width_range.1 > 1920);

        // Height should be around 1080 with tolerance
        assert!(res_1080p.height_range.0 < 1080);
        assert!(res_1080p.height_range.1 > 1080);
    }

    #[test]
    fn fuzzy_resolution_720p_bounds() {
        let res_720p = FUZZY_RESOLUTIONS
            .iter()
            .find(|r| r.label_height == 720)
            .expect("720p should exist in fuzzy resolutions");

        // Width should be around 1280 with tolerance
        assert!(res_720p.width_range.0 < 1280);
        assert!(res_720p.width_range.1 > 1280);

        // Height should be around 720 with tolerance
        assert!(res_720p.height_range.0 < 720);
        assert!(res_720p.height_range.1 > 720);
    }

    #[test]
    fn resolution_match_display() {
        let res_match = ResolutionMatch {
            label_height: 1080,
            width_range: (1872, 1968),
            height_range: (1053, 1107),
        };
        let display = format!("{res_match}");
        assert!(display.contains("1080p"));
        assert!(display.contains("width"));
        assert!(display.contains("height"));
    }

    #[test]
    fn fuzzy_resolution_480p_bounds() {
        let res_480p = FUZZY_RESOLUTIONS
            .iter()
            .find(|r| r.label_height == 480)
            .expect("480p should exist in fuzzy resolutions");

        assert!(res_480p.height_range.0 < 480);
        assert!(res_480p.height_range.1 > 480);
    }

    #[test]
    fn fuzzy_resolution_2160p_bounds() {
        let res_2160p = FUZZY_RESOLUTIONS
            .iter()
            .find(|r| r.label_height == 2160)
            .expect("2160p should exist in fuzzy resolutions");

        assert!(res_2160p.width_range.0 < 3840);
        assert!(res_2160p.width_range.1 > 3840);
        assert!(res_2160p.height_range.0 < 2160);
        assert!(res_2160p.height_range.1 > 2160);
    }

    #[test]
    fn fuzzy_resolutions_no_overlapping_height_ranges() {
        for i in 0..FUZZY_RESOLUTIONS.len() {
            for j in (i + 1)..FUZZY_RESOLUTIONS.len() {
                let a = &FUZZY_RESOLUTIONS[i];
                let b = &FUZZY_RESOLUTIONS[j];
                // Check height ranges don't overlap (unless they're for same height like 480p variants)
                if a.label_height != b.label_height {
                    let a_overlaps_b = a.height_range.0 <= b.height_range.1 && a.height_range.1 >= b.height_range.0;
                    // Some overlap is expected for close resolutions, just ensure they're different labels
                    if a_overlaps_b {
                        assert_ne!(
                            a.label_height, b.label_height,
                            "Different resolutions should have different labels"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn all_known_resolutions_have_fuzzy_match() {
        for (width, height) in KNOWN_RESOLUTIONS {
            let res = Resolution {
                width: *width,
                height: *height,
            };
            let label = res.label();
            assert!(
                label.ends_with('p'),
                "Known resolution {width}x{height} should have a standard label, got {label}"
            );
        }
    }

    #[test]
    fn fuzzy_match_just_inside_tolerance_1080p() {
        // 2.5% of 1920 = 48, so 1872-1968 is valid width range
        // 2.5% of 1080 = 27, so 1053-1107 is valid height range
        let res = Resolution {
            width: 1872,
            height: 1053,
        };
        assert_eq!(res.label(), "1080p");
    }

    #[test]
    fn fuzzy_match_just_outside_tolerance_1080p() {
        // Just outside the tolerance should not match
        let res = Resolution {
            width: 1800,
            height: 1000,
        };
        assert_eq!(res.label(), "1800x1000");
    }
}

#[cfg(test)]
mod compute_bounds_tests {
    use super::*;

    #[test]
    fn standard_resolution() {
        let bounds = compute_bounds(1080);
        // 2.5% tolerance = 27 pixels
        assert_eq!(bounds.0, 1053); // 1080 - 27
        assert_eq!(bounds.1, 1107); // 1080 + 27
    }

    #[test]
    fn zero() {
        let bounds = compute_bounds(0);
        assert_eq!(bounds, (0, 0));
    }

    #[test]
    fn small_value() {
        let bounds = compute_bounds(100);
        // 2.5% of 100 = 2.5, truncated to 2
        assert_eq!(bounds.0, 98);
        assert_eq!(bounds.1, 102);
    }

    #[test]
    fn bounds_720p() {
        let bounds = compute_bounds(720);
        // 2.5% of 720 = 18
        assert_eq!(bounds.0, 702);
        assert_eq!(bounds.1, 738);
    }

    #[test]
    fn bounds_4k_height() {
        let bounds = compute_bounds(2160);
        // 2.5% of 2160 = 54
        assert_eq!(bounds.0, 2106);
        assert_eq!(bounds.1, 2214);
    }

    #[test]
    fn bounds_4k_width() {
        let bounds = compute_bounds(3840);
        // 2.5% of 3840 = 96
        assert_eq!(bounds.0, 3744);
        assert_eq!(bounds.1, 3936);
    }

    #[test]
    fn bounds_very_small_value() {
        let bounds = compute_bounds(10);
        // 2.5% of 10 = 0.25, truncated to 0
        assert_eq!(bounds.0, 10);
        assert_eq!(bounds.1, 10);
    }

    #[test]
    fn bounds_one() {
        let bounds = compute_bounds(1);
        // 2.5% of 1 = 0.025, truncated to 0
        assert_eq!(bounds.0, 1);
        assert_eq!(bounds.1, 1);
    }

    #[test]
    fn bounds_large_value() {
        let bounds = compute_bounds(7680);
        // 2.5% of 7680 = 192
        assert_eq!(bounds.0, 7488);
        assert_eq!(bounds.1, 7872);
    }
}

#[cfg(test)]
mod ffprobe_result_tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn new_path_if_needed_no_label() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("1080p"));
    }

    #[test]
    fn new_path_if_needed_already_has_label() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.1080p.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_none());
    }

    #[test]
    fn new_path_if_needed_replaces_full_resolution() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.1920x1080.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        // Should replace "1920x1080" with "1080p"
        assert!(new_path.to_string_lossy().contains("1080p"));
        assert!(!new_path.to_string_lossy().contains("1920x1080"));
    }

    #[test]
    fn new_path_if_needed_vertical() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1080,
                height: 1920,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("Vertical.1080p"));
    }

    #[test]
    fn delete_dryrun() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("to_delete.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path.clone(),
            resolution: Resolution {
                width: 320,
                height: 240,
            },
        };

        // Dryrun should not delete the file
        result.delete(true).unwrap();
        assert!(file_path.exists());
    }

    #[test]
    fn delete_actual() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("to_delete.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path.clone(),
            resolution: Resolution {
                width: 320,
                height: 240,
            },
        };

        // Actual delete should remove the file
        result.delete(false).unwrap();
        assert!(!file_path.exists());
    }

    #[test]
    fn rename_dryrun() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("original.mp4");
        let new_path = temp_dir.path().join("renamed.1080p.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path.clone(),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        // Dryrun should not rename the file
        result.rename(&new_path, false, true).unwrap();
        assert!(file_path.exists());
        assert!(!new_path.exists());
    }

    #[test]
    fn rename_actual() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("original.mp4");
        let new_path = temp_dir.path().join("renamed.1080p.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path.clone(),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        // Actual rename should move the file
        result.rename(&new_path, false, false).unwrap();
        assert!(!file_path.exists());
        assert!(new_path.exists());
    }

    #[test]
    fn rename_no_overwrite() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("original.mp4");
        let new_path = temp_dir.path().join("existing.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");
        std::fs::File::create(&new_path).expect("Failed to create existing file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        // Should fail when target exists and overwrite is false
        let rename_result = result.rename(&new_path, false, false);
        assert!(rename_result.is_err());
    }

    #[test]
    fn rename_with_overwrite() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("original.mp4");
        let new_path = temp_dir.path().join("existing.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");
        std::fs::File::create(&new_path).expect("Failed to create existing file");

        let result = FFProbeResult {
            file: file_path.clone(),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        // Should succeed when overwrite is true
        result.rename(&new_path, true, false).unwrap();
        assert!(!file_path.exists());
        assert!(new_path.exists());
    }

    #[test]
    fn ordering_by_resolution() {
        let result1 = FFProbeResult {
            file: PathBuf::from("a.mp4"),
            resolution: Resolution {
                width: 1280,
                height: 720,
            },
        };
        let result2 = FFProbeResult {
            file: PathBuf::from("b.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        assert!(result1 < result2);
    }

    #[test]
    fn ordering_same_resolution_by_file() {
        let result1 = FFProbeResult {
            file: PathBuf::from("a.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        let result2 = FFProbeResult {
            file: PathBuf::from("b.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        assert!(result1 < result2);
    }

    #[test]
    fn equality() {
        let result1 = FFProbeResult {
            file: PathBuf::from("video.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        let result2 = FFProbeResult {
            file: PathBuf::from("video.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        assert_eq!(result1, result2);
    }

    #[test]
    fn inequality_different_file() {
        let result1 = FFProbeResult {
            file: PathBuf::from("video1.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        let result2 = FFProbeResult {
            file: PathBuf::from("video2.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        assert_ne!(result1, result2);
    }

    #[test]
    fn inequality_different_resolution() {
        let result1 = FFProbeResult {
            file: PathBuf::from("video.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        let result2 = FFProbeResult {
            file: PathBuf::from("video.mp4"),
            resolution: Resolution {
                width: 1280,
                height: 720,
            },
        };
        assert_ne!(result1, result2);
    }

    #[test]
    fn new_path_if_needed_with_dots_in_name() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.2024.01.15.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("1080p"));
    }

    #[test]
    fn new_path_if_needed_unknown_resolution() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1600,
                height: 900,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("1600x900"));
    }

    #[test]
    fn new_path_if_needed_720p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1280,
                height: 720,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("720p"));
    }

    #[test]
    fn new_path_if_needed_4k() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("2160p"));
    }

    #[test]
    fn new_path_if_needed_already_has_720p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.720p.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 1280,
                height: 720,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_none());
    }

    #[test]
    fn new_path_if_needed_replaces_720x480_with_480p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.720x480.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 720,
                height: 480,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert!(new_path.to_string_lossy().contains("480p"));
        assert!(!new_path.to_string_lossy().contains("720x480"));
    }

    #[test]
    fn ordering_multiple_results() {
        // FFProbeResult derives Ord which sorts by file path first, then resolution
        let mut results = [
            FFProbeResult {
                file: PathBuf::from("c.mp4"),
                resolution: Resolution {
                    width: 1920,
                    height: 1080,
                },
            },
            FFProbeResult {
                file: PathBuf::from("a.mp4"),
                resolution: Resolution {
                    width: 1280,
                    height: 720,
                },
            },
            FFProbeResult {
                file: PathBuf::from("b.mp4"),
                resolution: Resolution {
                    width: 3840,
                    height: 2160,
                },
            },
        ];

        results.sort();

        // Sorted alphabetically by file path
        assert_eq!(results[0].file, PathBuf::from("a.mp4"));
        assert_eq!(results[1].file, PathBuf::from("b.mp4"));
        assert_eq!(results[2].file, PathBuf::from("c.mp4"));
    }

    #[test]
    fn debug_format() {
        let result = FFProbeResult {
            file: PathBuf::from("video.mp4"),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        let debug_str = format!("{result:?}");
        assert!(debug_str.contains("FFProbeResult"));
        assert!(debug_str.contains("video.mp4"));
        assert!(debug_str.contains("1920"));
        assert!(debug_str.contains("1080"));
    }
}
