//! General helpers for the video conversion pipeline.
//!
//! Provides reusable path comparison, duration formatting, and disk space validation utilities.

use std::path::{Path, PathBuf};

use cli_tools::print_error;

use crate::types::ProcessableFile;

/// Minimum free disk space required before converting a file, as a multiple of the
/// original file size.
const MIN_DISK_SPACE_FACTOR: u64 = 2;

/// Return a copy of a path with its extension removed.
pub fn path_without_extension(path: &Path) -> PathBuf {
    let mut path = path.to_owned();
    path.set_extension("");
    path
}

/// Return a unique backup path beside the given output path.
pub fn backup_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = cli_tools::path_to_file_stem_string(output);
    let extension = cli_tools::path_to_file_extension_string(output);
    let backup_stem = format!("{stem}.vconvert-backup");
    let backup_filename = format!("{backup_stem}.{extension}");
    cli_tools::get_unique_path(parent, &backup_filename, &backup_stem, &extension)
}

/// Return a unique temporary path beside the given output path.
pub fn temporary_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = cli_tools::path_to_file_stem_string(output);
    let extension = cli_tools::path_to_file_extension_string(output);
    let temporary_stem = format!("{stem}.vconvert-tmp");
    let temporary_filename = format!("{temporary_stem}.{extension}");
    cli_tools::get_unique_path(parent, &temporary_filename, &temporary_stem, &extension)
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

/// Check that the output volume has enough free space for the given file.
///
/// Requires at least `MIN_DISK_SPACE_FACTOR` times the original file size to be free.
/// Prints an out-of-disk-space error and returns `false` when the space is insufficient.
/// If the available space cannot be determined, the check passes.
pub fn has_enough_disk_space(file: &ProcessableFile) -> bool {
    let original_size = file.info.size_bytes;
    let required = original_size.saturating_mul(MIN_DISK_SPACE_FACTOR);
    let Some(available) = cli_tools::available_disk_space(&file.output_path) else {
        return true;
    };

    if available < required {
        print_error!(
            "Out of disk space: converting {} needs {} free but only {} is available",
            cli_tools::path_to_string_relative(&file.file.path),
            cli_tools::format_size(required),
            cli_tools::format_size(available),
        );
        return false;
    }

    true
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
mod test_path_without_extension {
    use super::*;

    #[test]
    fn removes_only_the_final_extension() {
        let path = Path::new("library").join("movie.release.mkv");

        assert_eq!(
            path_without_extension(&path),
            Path::new("library").join("movie.release")
        );
    }

    #[test]
    fn leaves_extensionless_path_unchanged() {
        let path = Path::new("library").join("README");

        assert_eq!(path_without_extension(&path), path);
    }
}

#[cfg(test)]
mod test_unique_output_paths {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn backup_path_uses_backup_suffix_beside_output() {
        let directory = tempdir().expect("temporary directory should be created");
        let output = directory.path().join("movie.mp4");

        assert_eq!(
            backup_output_path(&output),
            directory.path().join("movie.vconvert-backup.mp4")
        );
    }

    #[test]
    fn backup_path_increments_suffix_when_candidate_exists() {
        let directory = tempdir().expect("temporary directory should be created");
        let output = directory.path().join("movie.mp4");
        std::fs::write(directory.path().join("movie.vconvert-backup.mp4"), [])
            .expect("conflicting backup should be created");

        assert_eq!(
            backup_output_path(&output),
            directory.path().join("movie.vconvert-backup.1.mp4")
        );
    }

    #[test]
    fn temporary_path_uses_temporary_suffix_beside_output() {
        let directory = tempdir().expect("temporary directory should be created");
        let output = directory.path().join("movie.mp4");

        assert_eq!(
            temporary_output_path(&output),
            directory.path().join("movie.vconvert-tmp.mp4")
        );
    }
}

#[cfg(test)]
mod test_path_identity {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn identical_nonexistent_paths_refer_to_same_file() {
        let path = Path::new("missing-video.mkv");

        assert!(paths_refer_to_same_file(path, path));
    }

    #[test]
    fn distinct_nonexistent_paths_do_not_refer_to_same_file() {
        assert!(!paths_refer_to_same_file(
            Path::new("missing-left.mkv"),
            Path::new("missing-right.mkv"),
        ));
    }

    #[test]
    fn canonical_aliases_refer_to_same_existing_file() {
        let directory = tempdir().expect("temporary directory should be created");
        let nested_directory = directory.path().join("nested");
        std::fs::create_dir(&nested_directory).expect("nested directory should be created");
        let file = directory.path().join("movie.mkv");
        std::fs::write(&file, []).expect("video fixture should be created");
        let alias = nested_directory.join("..").join("movie.mkv");

        assert!(paths_refer_to_same_file(&file, &alias));
    }
}

#[cfg(test)]
mod test_duration_difference_ratio {
    use super::*;

    #[test]
    fn calculates_absolute_ratio_for_longer_and_shorter_targets() {
        assert!((duration_difference_ratio(100.0, 125.0) - 0.25).abs() < f64::EPSILON);
        assert!((duration_difference_ratio(100.0, 75.0) - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn non_positive_source_duration_returns_full_difference() {
        assert!((duration_difference_ratio(0.0, 10.0) - 1.0).abs() < f64::EPSILON);
        assert!((duration_difference_ratio(-10.0, 10.0) - 1.0).abs() < f64::EPSILON);
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
