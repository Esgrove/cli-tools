use std::borrow::Cow;
use std::fmt;

const RESOLUTION_TOLERANCE: f32 = 0.025;
const KNOWN_RESOLUTIONS: &[(u32, u32)] = &[
    (640, 480),
    (720, 480),
    (720, 540),
    (960, 540),
    (720, 544),
    (720, 576),
    (800, 600),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const FUZZY_RESOLUTIONS: [ResolutionMatch; KNOWN_RESOLUTIONS.len()] = precalculate_fuzzy_resolutions();

/// A fuzzy resolution match with tolerance ranges for width and height.
///
/// Used to classify resolutions that are close to a known standard
/// (e.g. 1918x1078 matches 1080p within tolerance).
#[derive(Copy, Clone, Debug)]
struct ResolutionMatch {
    /// The height value used in the label (e.g. 1080 for "1080p").
    label_height: u32,
    /// Acceptable width range `(min, max)` for this resolution.
    width_range: (u32, u32),
    /// Acceptable height range `(min, max)` for this resolution.
    height_range: (u32, u32),
}

/// Video resolution represented as width and height in pixels.
#[derive(Debug, Clone, Copy, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct Resolution {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl fmt::Display for ResolutionMatch {
    /// Format as `<height>p: width <range>, height <range>` for debug output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}p: width {:?}, height {:?}",
            self.label_height, self.width_range, self.height_range
        )
    }
}

impl Resolution {
    /// Create a new resolution with the given width and height in pixels.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Create a resolution from optional width and height values.
    /// Returns `None` if either value is missing.
    #[must_use]
    pub const fn from_options(width: Option<u32>, height: Option<u32>) -> Option<Self> {
        match (width, height) {
            (Some(width), Some(height)) => Some(Self::new(width, height)),
            _ => None,
        }
    }

    /// Return the total number of pixels (width × height).
    #[must_use]
    pub const fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Returns true if the resolution is landscape (width >= height).
    #[must_use]
    pub const fn is_landscape(&self) -> bool {
        self.width >= self.height
    }

    /// Return the aspect ratio as width divided by height.
    #[must_use]
    pub fn aspect_ratio(&self) -> f64 {
        if self.height == 0 {
            return 0.0;
        }
        f64::from(self.width) / f64::from(self.height)
    }

    /// Returns true if width or height is smaller than the given limit.
    #[must_use]
    pub const fn is_smaller_than(&self, limit: u32) -> bool {
        self.width < limit || self.height < limit
    }

    /// Return a labeled string, prefixed with `Vertical.` for portrait resolutions.
    ///
    /// Landscape and square resolutions return `WIDTHxHEIGHT`.
    /// Portrait resolutions return `Vertical.WIDTHxHEIGHT`.
    ///
    /// This is the simple dimension-based format without fuzzy resolution matching.
    /// Use [`label()`](Self::label) for standard resolution labels like `1080p`.
    #[must_use]
    pub fn to_labeled_string(&self) -> String {
        if self.is_landscape() {
            self.to_string()
        } else {
            format!("Vertical.{self}")
        }
    }

    /// Return a human-readable resolution label like `1080p` or `Vertical.720p`.
    ///
    /// Tries exact matches first, then falls back to fuzzy matching within tolerance.
    /// If no known resolution matches, returns the full `WIDTHxHEIGHT` string
    /// (prefixed with `Vertical.` for portrait resolutions).
    #[must_use]
    pub fn label(&self) -> Cow<'static, str> {
        if self.width < self.height {
            // Vertical video
            match (self.width, self.height) {
                (480, 640 | 720) => Cow::Borrowed("Vertical.480p"),
                (540, 720 | 960) => Cow::Borrowed("Vertical.540p"),
                (544, 720) => Cow::Borrowed("Vertical.544p"),
                (576, 720) => Cow::Borrowed("Vertical.576p"),
                (600, 800) => Cow::Borrowed("Vertical.600p"),
                (720, 1280) => Cow::Borrowed("Vertical.720p"),
                (1080, 1920) => Cow::Borrowed("Vertical.1080p"),
                (1440, 2560) => Cow::Borrowed("Vertical.1440p"),
                (2160, 3840) => Cow::Borrowed("Vertical.2160p"),
                _ => label_fuzzy_vertical(*self),
            }
        } else {
            // Horizontal video
            match (self.width, self.height) {
                (640 | 720, 480) => Cow::Borrowed("480p"),
                (720 | 960, 540) => Cow::Borrowed("540p"),
                (720, 544) => Cow::Borrowed("544p"),
                (720, 576) => Cow::Borrowed("576p"),
                (800, 600) => Cow::Borrowed("600p"),
                (1280, 720) => Cow::Borrowed("720p"),
                (1920, 1080) => Cow::Borrowed("1080p"),
                (2560, 1440) => Cow::Borrowed("1440p"),
                (3840, 2160) => Cow::Borrowed("2160p"),
                _ => label_fuzzy_horizontal(*self),
            }
        }
    }
}

impl fmt::Display for Resolution {
    /// Format the resolution as `WIDTHxHEIGHT`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Attempt fuzzy matching for vertical (portrait) video resolutions.
///
/// Falls back to the full labeled resolution string if no fuzzy match is found.
fn label_fuzzy_vertical(resolution: Resolution) -> Cow<'static, str> {
    for res in &FUZZY_RESOLUTIONS {
        if resolution.height >= res.width_range.0
            && resolution.height <= res.width_range.1
            && resolution.width >= res.height_range.0
            && resolution.width <= res.height_range.1
        {
            return Cow::Owned(format!("Vertical.{}p", res.label_height));
        }
    }
    // fall back to full resolution label
    Cow::Owned(resolution.to_labeled_string())
}

/// Attempt fuzzy matching for horizontal (landscape) video resolutions.
///
/// Falls back to the full labeled resolution string if no fuzzy match is found.
fn label_fuzzy_horizontal(resolution: Resolution) -> Cow<'static, str> {
    for res in &FUZZY_RESOLUTIONS {
        if resolution.width >= res.width_range.0
            && resolution.width <= res.width_range.1
            && resolution.height >= res.height_range.0
            && resolution.height <= res.height_range.1
        {
            return Cow::Owned(format!("{}p", res.label_height));
        }
    }
    // fall back to full resolution label
    Cow::Owned(resolution.to_labeled_string())
}

/// Print all precalculated fuzzy resolution ranges to stdout for debugging.
pub fn print_fuzzy_resolution_ranges() {
    println!("Fuzzy resolution ranges:");
    for res in &FUZZY_RESOLUTIONS {
        println!("  {res}");
    }
}

/// Precalculate fuzzy resolution match ranges for all known resolutions at compile time.
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

/// Compute the lower and upper tolerance bounds for a resolution dimension.
///
/// Returns `(min, max)` where the bounds are `resolution ± (resolution * RESOLUTION_TOLERANCE)`.
const fn compute_bounds(res: u32) -> (u32, u32) {
    let tolerance = (res as f32 * RESOLUTION_TOLERANCE) as u32;
    let min = res.saturating_sub(tolerance);
    let max = res.saturating_add(tolerance);
    (min, max)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod test_resolution_new {
    use super::*;

    #[test]
    fn creates_resolution_with_correct_dimensions() {
        let resolution = Resolution::new(1920, 1080);
        assert_eq!(resolution.width, 1920);
        assert_eq!(resolution.height, 1080);
    }

    #[test]
    fn creates_with_zero_dimensions() {
        let resolution = Resolution::new(0, 0);
        assert_eq!(resolution.width, 0);
        assert_eq!(resolution.height, 0);
    }

    #[test]
    fn creates_with_max_u32() {
        let resolution = Resolution::new(u32::MAX, u32::MAX);
        assert_eq!(resolution.width, u32::MAX);
        assert_eq!(resolution.height, u32::MAX);
    }

    #[test]
    fn creates_with_asymmetric_dimensions() {
        let resolution = Resolution::new(7680, 4320);
        assert_eq!(resolution.width, 7680);
        assert_eq!(resolution.height, 4320);
    }
}

#[cfg(test)]
mod test_resolution_from_options {
    use super::*;

    #[test]
    fn both_present() {
        let resolution = Resolution::from_options(Some(1920), Some(1080));
        let resolution = resolution.expect("should have resolution");
        assert_eq!(resolution.width, 1920);
        assert_eq!(resolution.height, 1080);
    }

    #[test]
    fn width_missing() {
        assert!(Resolution::from_options(None, Some(1080)).is_none());
    }

    #[test]
    fn height_missing() {
        assert!(Resolution::from_options(Some(1920), None).is_none());
    }

    #[test]
    fn both_missing() {
        assert!(Resolution::from_options(None, None).is_none());
    }
}

#[cfg(test)]
mod test_resolution_pixel_count {
    use super::*;

    #[test]
    fn standard_1080p() {
        let resolution = Resolution::new(1920, 1080);
        assert_eq!(resolution.pixel_count(), 1920 * 1080);
    }

    #[test]
    fn zero_dimensions() {
        let resolution = Resolution::new(0, 0);
        assert_eq!(resolution.pixel_count(), 0);
    }

    #[test]
    fn single_pixel() {
        let resolution = Resolution::new(1, 1);
        assert_eq!(resolution.pixel_count(), 1);
    }

    #[test]
    fn large_resolution() {
        let resolution = Resolution::new(7680, 4320);
        assert_eq!(resolution.pixel_count(), 7680 * 4320);
    }
}

#[cfg(test)]
mod test_resolution_is_landscape {
    use super::*;

    #[test]
    fn landscape() {
        assert!(Resolution::new(1920, 1080).is_landscape());
    }

    #[test]
    fn portrait() {
        assert!(!Resolution::new(1080, 1920).is_landscape());
    }

    #[test]
    fn square() {
        assert!(Resolution::new(1080, 1080).is_landscape());
    }
}

#[cfg(test)]
mod test_resolution_aspect_ratio {
    use super::*;

    #[test]
    fn standard_16_9() {
        let ratio = Resolution::new(1920, 1080).aspect_ratio();
        assert!((ratio - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn square() {
        let ratio = Resolution::new(1080, 1080).aspect_ratio();
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_height() {
        assert!((Resolution::new(1920, 0).aspect_ratio() - 0.0).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod test_resolution_is_smaller_than {
    use super::*;

    #[test]
    fn width_below_limit() {
        let resolution = Resolution::new(400, 720);
        assert!(resolution.is_smaller_than(480));
    }

    #[test]
    fn height_below_limit() {
        let resolution = Resolution::new(720, 400);
        assert!(resolution.is_smaller_than(480));
    }

    #[test]
    fn both_below_limit() {
        let resolution = Resolution::new(320, 240);
        assert!(resolution.is_smaller_than(480));
    }

    #[test]
    fn both_above_limit() {
        let resolution = Resolution::new(1920, 1080);
        assert!(!resolution.is_smaller_than(480));
    }

    #[test]
    fn at_exact_limit() {
        let resolution = Resolution::new(480, 480);
        assert!(!resolution.is_smaller_than(480));
    }

    #[test]
    fn one_at_limit_one_below() {
        let resolution = Resolution::new(480, 479);
        assert!(resolution.is_smaller_than(480));
    }

    #[test]
    fn zero_limit() {
        let resolution = Resolution::new(100, 100);
        assert!(!resolution.is_smaller_than(0));
    }

    #[test]
    fn zero_resolution() {
        let resolution = Resolution::new(0, 0);
        assert!(resolution.is_smaller_than(1));
        assert!(!resolution.is_smaller_than(0));
    }

    #[test]
    fn large_limit() {
        let resolution = Resolution::new(3840, 2160);
        assert!(resolution.is_smaller_than(4000));
        assert!(!resolution.is_smaller_than(2160));
    }

    #[test]
    fn vertical_video_width_below() {
        let resolution = Resolution::new(360, 640);
        assert!(resolution.is_smaller_than(480));
    }

    #[test]
    fn vertical_video_both_above() {
        let resolution = Resolution::new(1080, 1920);
        assert!(!resolution.is_smaller_than(720));
    }

    #[test]
    fn max_u32_limit() {
        let resolution = Resolution::new(1920, 1080);
        assert!(resolution.is_smaller_than(u32::MAX));
    }

    #[test]
    fn one_dimension_at_limit() {
        let resolution = Resolution::new(720, 480);
        assert!(!resolution.is_smaller_than(480));
    }
}

#[cfg(test)]
mod test_resolution_display {
    use super::*;

    #[test]
    fn horizontal_resolution() {
        let resolution = Resolution::new(1920, 1080);
        assert_eq!(format!("{resolution}"), "1920x1080");
    }

    #[test]
    fn vertical_resolution() {
        let resolution = Resolution::new(1080, 1920);
        assert_eq!(format!("{resolution}"), "1080x1920");
    }

    #[test]
    fn square_resolution() {
        let resolution = Resolution::new(1080, 1080);
        assert_eq!(format!("{resolution}"), "1080x1080");
    }

    #[test]
    fn small_resolution() {
        let resolution = Resolution::new(320, 240);
        assert_eq!(format!("{resolution}"), "320x240");
    }

    #[test]
    fn resolution_4k() {
        let resolution = Resolution::new(3840, 2160);
        assert_eq!(format!("{resolution}"), "3840x2160");
    }

    #[test]
    fn resolution_8k() {
        let resolution = Resolution::new(7680, 4320);
        assert_eq!(format!("{resolution}"), "7680x4320");
    }

    #[test]
    fn ultrawide_resolution() {
        let resolution = Resolution::new(3440, 1440);
        assert_eq!(format!("{resolution}"), "3440x1440");
    }

    #[test]
    fn vertical_4k() {
        let resolution = Resolution::new(2160, 3840);
        assert_eq!(format!("{resolution}"), "2160x3840");
    }

    #[test]
    fn debug_format() {
        let resolution = Resolution::new(1920, 1080);
        let debug_str = format!("{resolution:?}");
        assert!(debug_str.contains("Resolution"));
        assert!(debug_str.contains("1920"));
        assert!(debug_str.contains("1080"));
    }
}

#[cfg(test)]
mod test_resolution_to_labeled_string {
    use super::*;

    #[test]
    fn horizontal_resolution() {
        let resolution = Resolution::new(1920, 1080);
        assert_eq!(resolution.to_labeled_string(), "1920x1080");
    }

    #[test]
    fn vertical_resolution() {
        let resolution = Resolution::new(1080, 1920);
        assert_eq!(resolution.to_labeled_string(), "Vertical.1080x1920");
    }

    #[test]
    fn square_resolution() {
        let resolution = Resolution::new(1080, 1080);
        assert_eq!(resolution.to_labeled_string(), "1080x1080");
    }

    #[test]
    fn small_resolution() {
        let resolution = Resolution::new(320, 240);
        assert_eq!(resolution.to_labeled_string(), "320x240");
    }

    #[test]
    fn vertical_4k() {
        let resolution = Resolution::new(2160, 3840);
        assert_eq!(resolution.to_labeled_string(), "Vertical.2160x3840");
    }
}

#[cfg(test)]
mod test_resolution_ordering {
    use super::*;

    #[test]
    fn lower_width_is_less() {
        let low = Resolution::new(1280, 720);
        let high = Resolution::new(1920, 1080);
        assert!(low < high);
    }

    #[test]
    fn same_width_lower_height_is_less() {
        let low = Resolution::new(1920, 1080);
        let high = Resolution::new(1920, 1440);
        assert!(low < high);
    }

    #[test]
    fn equal_resolutions() {
        let res1 = Resolution::new(1920, 1080);
        let res2 = Resolution::new(1920, 1080);
        assert_eq!(res1, res2);
    }

    #[test]
    fn inequality() {
        let res1 = Resolution::new(1920, 1080);
        let res2 = Resolution::new(1920, 1079);
        assert_ne!(res1, res2);
    }

    #[test]
    fn ordering_by_width_first() {
        let res1 = Resolution::new(1280, 1080);
        let res2 = Resolution::new(1920, 720);
        assert!(res1 < res2);
    }

    #[test]
    fn ordering_same_width_different_height() {
        let res1 = Resolution::new(1920, 1080);
        let res2 = Resolution::new(1920, 1440);
        assert!(res1 < res2);
    }

    #[test]
    fn ordering_vertical_vs_horizontal() {
        let vertical = Resolution::new(1080, 1920);
        let horizontal = Resolution::new(1920, 1080);
        assert!(vertical < horizontal);
    }
}

#[cfg(test)]
mod test_resolution_hash {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn same_resolutions_hash_equal() {
        let mut set = HashSet::new();
        set.insert(Resolution::new(1920, 1080));
        set.insert(Resolution::new(1920, 1080));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn different_resolutions_hash_different() {
        let mut set = HashSet::new();
        set.insert(Resolution::new(1920, 1080));
        set.insert(Resolution::new(1280, 720));
        assert_eq!(set.len(), 2);
    }
}

#[cfg(test)]
mod test_label_exact_matches {
    use super::*;

    #[test]
    fn exact_matches_horizontal() {
        assert_eq!(Resolution::new(1280, 720).label(), "720p");
        assert_eq!(Resolution::new(1920, 1080).label(), "1080p");
        assert_eq!(Resolution::new(2560, 1440).label(), "1440p");
        assert_eq!(Resolution::new(3840, 2160).label(), "2160p");
    }

    #[test]
    fn exact_matches_vertical() {
        assert_eq!(Resolution::new(720, 1280).label(), "Vertical.720p");
        assert_eq!(Resolution::new(1080, 1920).label(), "Vertical.1080p");
        assert_eq!(Resolution::new(1440, 2560).label(), "Vertical.1440p");
        assert_eq!(Resolution::new(2160, 3840).label(), "Vertical.2160p");
    }

    #[test]
    fn exact_matches_480p() {
        assert_eq!(Resolution::new(640, 480).label(), "480p");
        assert_eq!(Resolution::new(720, 480).label(), "480p");
    }

    #[test]
    fn exact_matches_540p() {
        assert_eq!(Resolution::new(720, 540).label(), "540p");
    }

    #[test]
    fn exact_matches_960x540() {
        assert_eq!(Resolution::new(960, 540).label(), "540p");
    }

    #[test]
    fn exact_matches_544p() {
        assert_eq!(Resolution::new(720, 544).label(), "544p");
    }

    #[test]
    fn exact_matches_576p() {
        assert_eq!(Resolution::new(720, 576).label(), "576p");
    }

    #[test]
    fn exact_matches_600p() {
        assert_eq!(Resolution::new(800, 600).label(), "600p");
    }

    #[test]
    fn exact_matches_vertical_480p() {
        assert_eq!(Resolution::new(480, 640).label(), "Vertical.480p");
        assert_eq!(Resolution::new(480, 720).label(), "Vertical.480p");
    }

    #[test]
    fn exact_matches_vertical_540p() {
        assert_eq!(Resolution::new(540, 720).label(), "Vertical.540p");
    }

    #[test]
    fn exact_matches_vertical_540x960() {
        assert_eq!(Resolution::new(540, 960).label(), "Vertical.540p");
    }

    #[test]
    fn exact_matches_vertical_544p() {
        assert_eq!(Resolution::new(544, 720).label(), "Vertical.544p");
    }

    #[test]
    fn exact_matches_vertical_576p() {
        assert_eq!(Resolution::new(576, 720).label(), "Vertical.576p");
    }

    #[test]
    fn exact_matches_vertical_600p() {
        assert_eq!(Resolution::new(600, 800).label(), "Vertical.600p");
    }

    #[test]
    fn sd_resolutions() {
        assert_eq!(Resolution::new(640, 480).label(), "480p");
        assert_eq!(Resolution::new(720, 576).label(), "576p");
    }
}

#[cfg(test)]
mod test_label_fuzzy_matches {
    use super::*;

    #[test]
    fn approximate_matches() {
        assert_eq!(Resolution::new(1920, 1078).label(), "1080p");
        assert_eq!(Resolution::new(1278, 716).label(), "720p");
        assert_eq!(Resolution::new(2540, 1442).label(), "1440p");
        assert_eq!(Resolution::new(1442, 2540).label(), "Vertical.1440p");
        assert_eq!(Resolution::new(3820, 2162).label(), "2160p");
        assert_eq!(Resolution::new(1260, 710).label(), "720p");
    }

    #[test]
    fn horizontal_near_boundaries() {
        // Just inside lower tolerance for 1080p
        assert_eq!(Resolution::new(1872, 1053).label(), "1080p");
        // Just inside upper tolerance for 1080p
        assert_eq!(Resolution::new(1968, 1107).label(), "1080p");
    }

    #[test]
    fn vertical_near_boundaries() {
        assert_eq!(Resolution::new(1078, 1918).label(), "Vertical.1080p");
    }

    #[test]
    fn fuzzy_matches_4k_variations() {
        assert_eq!(Resolution::new(3800, 2140).label(), "2160p");
        assert_eq!(Resolution::new(3860, 2170).label(), "2160p");
    }

    #[test]
    fn lower_bound_tolerance() {
        assert_eq!(Resolution::new(1267, 713).label(), "720p");
    }

    #[test]
    fn upper_bound_tolerance() {
        assert_eq!(Resolution::new(1292, 727).label(), "720p");
    }

    #[test]
    fn near_720p_within_tolerance() {
        assert_eq!(Resolution::new(1270, 715).label(), "720p");
    }

    #[test]
    fn slightly_cropped_1080p() {
        assert_eq!(Resolution::new(1920, 1072).label(), "1080p");
    }

    #[test]
    fn vertical_1080p_slightly_off() {
        assert_eq!(Resolution::new(1078, 1918).label(), "Vertical.1080p");
    }

    #[test]
    fn just_inside_tolerance_1080p() {
        assert_eq!(Resolution::new(1872, 1053).label(), "1080p");
    }

    #[test]
    fn just_outside_tolerance_1080p() {
        assert_eq!(Resolution::new(1800, 1000).label(), "1800x1000");
    }
}

#[cfg(test)]
mod test_label_out_of_range {
    use super::*;

    #[test]
    fn unknown_resolutions() {
        assert_eq!(Resolution::new(1024, 768).label(), "1024x768");
        assert_eq!(Resolution::new(3000, 2000).label(), "3000x2000");
    }

    #[test]
    fn beyond_tolerance() {
        assert_eq!(Resolution::new(1250, 790).label(), "1250x790");
    }

    #[test]
    fn near_720p_outside_tolerance() {
        assert_eq!(Resolution::new(1200, 680).label(), "1200x680");
    }

    #[test]
    fn small_resolutions() {
        assert_eq!(Resolution::new(320, 240).label(), "320x240");
        assert_eq!(Resolution::new(160, 120).label(), "160x120");
    }

    #[test]
    fn unusual_aspect_ratios() {
        assert_eq!(Resolution::new(2560, 1080).label(), "2560x1080");
        assert_eq!(Resolution::new(500, 2000).label(), "Vertical.500x2000");
    }

    #[test]
    fn between_known_resolutions() {
        assert_eq!(Resolution::new(1600, 900).label(), "1600x900");
    }

    #[test]
    fn unknown_horizontal() {
        assert_eq!(Resolution::new(1234, 567).label(), "1234x567");
    }

    #[test]
    fn unknown_vertical() {
        assert_eq!(Resolution::new(567, 1234).label(), "Vertical.567x1234");
    }

    #[test]
    fn square_treated_as_horizontal() {
        let resolution = Resolution::new(500, 500);
        assert_eq!(resolution.label(), "500x500");
        assert!(!resolution.label().contains("Vertical"));
    }

    #[test]
    fn resolution_8k_horizontal() {
        assert_eq!(Resolution::new(7680, 4320).label(), "7680x4320");
    }

    #[test]
    fn resolution_8k_vertical() {
        assert_eq!(Resolution::new(4320, 7680).label(), "Vertical.4320x7680");
    }

    #[test]
    fn ultrawide_1440p() {
        assert_eq!(Resolution::new(3440, 1440).label(), "3440x1440");
    }

    #[test]
    fn dci_4k() {
        assert_eq!(Resolution::new(4096, 2160).label(), "4096x2160");
    }

    #[test]
    fn exact_1440p() {
        assert_eq!(Resolution::new(2560, 1440).label(), "1440p");
    }

    #[test]
    fn exact_vertical_1440p() {
        assert_eq!(Resolution::new(1440, 2560).label(), "Vertical.1440p");
    }

    #[test]
    fn very_small_resolution() {
        assert_eq!(Resolution::new(64, 64).label(), "64x64");
    }

    #[test]
    fn single_pixel() {
        assert_eq!(Resolution::new(1, 1).label(), "1x1");
    }

    #[test]
    fn zero_dimensions() {
        assert_eq!(Resolution::new(0, 0).label(), "0x0");
    }
}

#[cfg(test)]
mod test_fuzzy_resolutions {
    use super::*;

    #[test]
    fn count_matches_known() {
        assert_eq!(FUZZY_RESOLUTIONS.len(), KNOWN_RESOLUTIONS.len());
    }

    #[test]
    fn fuzzy_resolution_1080p_bounds() {
        let res_1080p = FUZZY_RESOLUTIONS
            .iter()
            .find(|r| r.label_height == 1080)
            .expect("1080p should exist in fuzzy resolutions");

        assert!(res_1080p.width_range.0 < 1920);
        assert!(res_1080p.width_range.1 > 1920);
        assert!(res_1080p.height_range.0 < 1080);
        assert!(res_1080p.height_range.1 > 1080);
    }

    #[test]
    fn fuzzy_resolution_720p_bounds() {
        let res_720p = FUZZY_RESOLUTIONS
            .iter()
            .find(|r| r.label_height == 720)
            .expect("720p should exist in fuzzy resolutions");

        assert!(res_720p.width_range.0 < 1280);
        assert!(res_720p.width_range.1 > 1280);
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
    fn no_overlapping_height_ranges() {
        for (i, a) in FUZZY_RESOLUTIONS.iter().enumerate() {
            for b in &FUZZY_RESOLUTIONS[(i + 1)..] {
                if a.label_height != b.label_height {
                    let a_overlaps_b = a.height_range.0 <= b.height_range.1 && a.height_range.1 >= b.height_range.0;
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
            let resolution = Resolution::new(*width, *height);
            let label = resolution.label();
            assert!(
                label.ends_with('p'),
                "Known resolution {width}x{height} should have a standard label, got {label}"
            );
        }
    }
}

#[cfg(test)]
mod test_compute_bounds {
    use super::*;

    #[test]
    fn standard_resolution() {
        let bounds = compute_bounds(1080);
        assert_eq!(bounds.0, 1053);
        assert_eq!(bounds.1, 1107);
    }

    #[test]
    fn zero() {
        let bounds = compute_bounds(0);
        assert_eq!(bounds, (0, 0));
    }

    #[test]
    fn small_value() {
        let bounds = compute_bounds(100);
        assert_eq!(bounds.0, 98);
        assert_eq!(bounds.1, 102);
    }

    #[test]
    fn bounds_720p() {
        let bounds = compute_bounds(720);
        assert_eq!(bounds.0, 702);
        assert_eq!(bounds.1, 738);
    }

    #[test]
    fn bounds_4k_height() {
        let bounds = compute_bounds(2160);
        assert_eq!(bounds.0, 2106);
        assert_eq!(bounds.1, 2214);
    }

    #[test]
    fn bounds_4k_width() {
        let bounds = compute_bounds(3840);
        assert_eq!(bounds.0, 3744);
        assert_eq!(bounds.1, 3936);
    }

    #[test]
    fn bounds_very_small_value() {
        let bounds = compute_bounds(10);
        assert_eq!(bounds.0, 10);
        assert_eq!(bounds.1, 10);
    }

    #[test]
    fn bounds_one() {
        let bounds = compute_bounds(1);
        assert_eq!(bounds.0, 1);
        assert_eq!(bounds.1, 1);
    }

    #[test]
    fn bounds_large_value() {
        let bounds = compute_bounds(7680);
        assert_eq!(bounds.0, 7488);
        assert_eq!(bounds.1, 7872);
    }
}
