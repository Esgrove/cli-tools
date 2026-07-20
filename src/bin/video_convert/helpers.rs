//! General helpers for the video conversion pipeline.
//!
//! Provides reusable path comparison and duration formatting utilities that do not depend on conversion orchestration.

use std::path::{Path, PathBuf};

/// Return a copy of a path with its extension removed.
pub fn path_without_extension(path: &Path) -> PathBuf {
    let mut path = path.to_owned();
    path.set_extension("");
    path
}

/// Return true when two paths resolve to the same filesystem entry.
pub fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    let Ok(left) = std::fs::canonicalize(left) else {
        return false;
    };
    let Ok(right) = std::fs::canonicalize(right) else {
        return false;
    };

    left == right
}

/// Return the absolute duration difference divided by the source duration.
pub fn duration_difference_ratio(source_duration: f64, target_duration: f64) -> f64 {
    if source_duration > 0.0 {
        (target_duration - source_duration).abs() / source_duration
    } else {
        1.0
    }
}

/// Format matching source and target durations for human-readable output.
pub fn format_duplicate_duration_match(source_duration: f64, target_duration: f64) -> String {
    let source_tenths = (source_duration * 10.0).round() as i64;
    let target_tenths = (target_duration * 10.0).round() as i64;
    if source_tenths == target_tenths {
        "duration match".to_string()
    } else {
        let duration_ratio = duration_difference_ratio(source_duration, target_duration);
        format!(
            "duration match: {source_duration:.1}s vs {target_duration:.1}s ({:.3}% difference)",
            duration_ratio * 100.0
        )
    }
}

#[cfg(test)]
mod test_duration_formatting {
    use super::*;

    #[test]
    fn duplicate_duration_match_hides_equal_rounded_durations() {
        let message = format_duplicate_duration_match(5414.84, 5414.83);

        assert_eq!(message, "duration match");
    }

    #[test]
    fn duplicate_duration_match_reports_percentage_when_rounded_duration_differs() {
        let message = format_duplicate_duration_match(100.0, 100.2);

        assert_eq!(message, "duration match: 100.0s vs 100.2s (0.200% difference)");
    }
}
