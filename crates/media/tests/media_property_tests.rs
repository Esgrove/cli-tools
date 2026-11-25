//! Property tests for resolution labels, dimension regexes, and ffprobe resolution parsing.
//!
//! Dimensions are drawn both around the common video sizes, where the exact and fuzzy label tables apply,
//! and from the whole range, where the label falls back to the raw dimensions.
//! Set `PROPTEST_CASES` to run more cases than the default.

use cli_tools_media::Resolution;
use cli_tools_media::video_info::parse_display_resolution;
use proptest::prelude::*;

/// Common video edge lengths that the label tables know about.
const COMMON_SIZES: &[u32] = &[
    360, 480, 540, 544, 576, 600, 640, 720, 800, 960, 1080, 1280, 1440, 1920, 2160, 2560, 3840,
];

/// An edge length near a common size or anywhere in range.
fn edge() -> impl Strategy<Value = u32> {
    prop_oneof![
        3 => (prop::sample::select(COMMON_SIZES), -40..=40_i64)
            .prop_map(|(size, offset)| u32::try_from(i64::from(size) + offset).unwrap_or(size)),
        1 => 1..20_000_u32,
    ]
}

/// Default config, honouring `PROPTEST_CASES`, without failure files.
///
/// Integration tests have no `lib.rs` next to them for proptest to store regressions beside,
/// and a shrunk failure is turned into a unit test in the owning crate instead.
fn config_without_persistence() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config_without_persistence())]

    #[test]
    fn portrait_label_is_the_vertical_landscape_label(first in edge(), second in edge()) {
        let (short, long) = (first.min(second), first.max(second));
        if short == long {
            return Ok(());
        }
        let portrait = Resolution::new(short, long).label();
        let landscape = Resolution::new(long, short).label();
        prop_assert!(portrait.starts_with("Vertical."), "{}", portrait);
        prop_assert!(!landscape.starts_with("Vertical."), "{}", landscape);
        if landscape.ends_with('p') {
            let expected = format!("Vertical.{landscape}");
            prop_assert_eq!(portrait.as_ref(), expected.as_str());
        } else {
            let expected_portrait = format!("Vertical.{short}x{long}");
            let expected_landscape = format!("{long}x{short}");
            prop_assert_eq!(portrait.as_ref(), expected_portrait.as_str());
            prop_assert_eq!(landscape.as_ref(), expected_landscape.as_str());
        }
    }

    #[test]
    fn dimension_regex_matches_both_orientations_only(width in edge(), height in edge(), digit in 1..=9_u32) {
        let resolution = Resolution::new(width, height);
        let regex = resolution.dimension_regex().expect("the pattern should compile");
        let matching = [
            format!("video.{width}x{height}.mp4"),
            format!("video.{height}x{width}.mp4"),
            format!("video.Vertical.{width}x{height}.mp4"),
            format!("VIDEO.{width}X{height}.MP4"),
        ];
        for name in &matching {
            prop_assert!(regex.is_match(name), "{}", name);
        }
        let longer = format!("video.{digit}{width}x{height}{digit}.mp4");
        prop_assert!(!regex.is_match(&longer), "{}", longer);
    }

    #[test]
    fn display_resolution_parses_in_any_key_order(width in 1..10_000_u32, height in 1..10_000_u32, extra in "x_[a-z_]{0,8}=[0-9a-z]{0,6}") {
        let forward = format!("width={width}\nheight={height}\n{extra}\n");
        let backward = format!("{extra}\n  height={height}\nwidth={width}");
        let expected = Some(Resolution::new(width, height));
        prop_assert_eq!(parse_display_resolution(&forward), expected);
        prop_assert_eq!(parse_display_resolution(&backward), expected);
    }

    #[test]
    fn quarter_turns_swap_the_dimensions(width in 1..10_000_u32, height in 1..10_000_u32, turns in -4..=4_i32, tag: bool) {
        let degrees = turns * 90;
        let key = if tag { "TAG:rotate" } else { "rotation" };
        let output = format!("width={width}\nheight={height}\n{key}={degrees}\n");
        let expected = if turns % 2 == 0 {
            Resolution::new(width, height)
        } else {
            Resolution::new(height, width)
        };
        prop_assert_eq!(parse_display_resolution(&output), Some(expected));
    }

    #[test]
    fn display_resolution_never_panics(output in "\\PC{0,120}") {
        let _ = parse_display_resolution(&output);
    }
}
