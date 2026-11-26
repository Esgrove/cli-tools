//! General helpers for the video conversion pipeline.
//!
//! Provides reusable path comparison, duration formatting, and disk space validation utilities.

use std::path::{Path, PathBuf};

use crate::observer::DiskSpaceShortage;
use crate::types::ProcessableFile;

/// Minimum free disk space required before converting a file, as a multiple of the original file size.
const MIN_DISK_SPACE_FACTOR: u64 = 2;

/// Return a copy of a path with its extension removed.
#[must_use]
pub fn path_without_extension(path: &Path) -> PathBuf {
    let mut path = path.to_owned();
    path.set_extension("");
    path
}

/// Return a unique backup path beside the given output path.
#[must_use]
pub fn backup_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = cli_tools_common::path_to_file_stem_string(output);
    let extension = cli_tools_common::path_to_file_extension_string(output);
    let backup_stem = format!("{stem}.vconvert-backup");
    let backup_filename = format!("{backup_stem}.{extension}");
    cli_tools_common::get_unique_path(parent, &backup_filename, &backup_stem, &extension)
}

/// Return a unique temporary path beside the given output path.
#[must_use]
pub fn temporary_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = cli_tools_common::path_to_file_stem_string(output);
    let extension = cli_tools_common::path_to_file_extension_string(output);
    let temporary_stem = format!("{stem}.vconvert-tmp");
    let temporary_filename = format!("{temporary_stem}.{extension}");
    cli_tools_common::get_unique_path(parent, &temporary_filename, &temporary_stem, &extension)
}

/// Return how much free space is missing at the output location of the given file.
///
/// Requires at least `MIN_DISK_SPACE_FACTOR` times the original file size to be free.
/// Returns `None` when there is enough space, or when the available space cannot be determined.
#[must_use]
pub fn disk_space_shortage(file: &ProcessableFile) -> Option<DiskSpaceShortage> {
    let required = file.info.size_bytes.saturating_mul(MIN_DISK_SPACE_FACTOR);
    let available = cli_tools_common::available_disk_space(&file.output_path)?;
    (available < required).then_some(DiskSpaceShortage { required, available })
}

/// Return the absolute duration difference divided by the source duration.
#[must_use]
pub fn duration_difference_ratio(source_duration: f64, target_duration: f64) -> f64 {
    if source_duration > 0.0 {
        (target_duration - source_duration).abs() / source_duration
    } else {
        1.0
    }
}

/// Format matching source and target durations for human-readable output.
#[must_use]
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

/// Remove a partial output left by a failed ffmpeg run.
///
/// A missing file is fine, since ffmpeg may fail before creating it.
///
/// # Errors
/// Returns the error for any other failure,
/// because a leftover output would later be taken for a finished conversion.
pub fn remove_partial_output(output: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(output) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod test_remove_partial_output {
    use super::*;

    #[test]
    fn removes_existing_output() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let output = directory.path().join("partial.mp4");
        std::fs::write(&output, b"partial").expect("Failed to create partial output");

        remove_partial_output(&output).expect("Failed to remove partial output");

        assert!(!output.exists());
    }

    #[test]
    fn missing_output_is_ignored() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let output = directory.path().join("never-created.mp4");

        remove_partial_output(&output).expect("A missing output should be ignored");

        assert!(!output.exists());
    }

    #[test]
    fn directory_in_place_of_output_is_left_alone() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let output = directory.path().join("output.mp4");
        std::fs::create_dir(&output).expect("Failed to create directory");

        assert!(remove_partial_output(&output).is_err());
        assert!(output.is_dir());
    }
}

#[cfg(test)]
mod test_disk_space_shortage {
    use super::*;
    use crate::types::{VideoFile, VideoInfo};

    fn file_of_size(path: &Path, size_bytes: u64) -> ProcessableFile {
        let info = VideoInfo {
            codec: "h264".to_string(),
            bitrate_kbps: 8_000,
            size_bytes,
            duration: 60.0,
            width: 1920,
            height: 1080,
            frames_per_second: 24.0,
            bit_depth: 8,
            warning: None,
        };
        ProcessableFile::for_mode(VideoFile::new(path, size_bytes), info, Vec::new(), false)
    }

    #[test]
    fn a_small_file_has_enough_space() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");

        assert_eq!(
            disk_space_shortage(&file_of_size(&directory.path().join("small.mkv"), 1)),
            None
        );
    }

    #[test]
    fn a_file_larger_than_the_disk_reports_the_shortage_when_the_free_space_is_known() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let file = file_of_size(&directory.path().join("huge.mkv"), u64::MAX / 4);
        let required = u64::MAX / 4 * MIN_DISK_SPACE_FACTOR;

        let shortage = disk_space_shortage(&file);
        if cli_tools_common::available_disk_space(&file.output_path).is_some() {
            let shortage = shortage.expect("shortage should be reported when free space is known");
            assert_eq!(shortage.required, required);
            assert!(shortage.available < required);
        } else {
            assert_eq!(shortage, None);
        }
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
    use std::path::Path;

    use cli_tools_common::paths_refer_to_same_file;
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
