use std::path::{Path, PathBuf};

use colored::Colorize;
use regex::Regex;

use cli_tools::Resolution;
use cli_tools::dot_rename::remove_extra_dots;

/// Result from running ffprobe on a video file.
#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
pub struct FFProbeResult {
    /// Path to the video file.
    pub(crate) file: PathBuf,
    /// Detected video resolution.
    pub(crate) resolution: Resolution,
}

impl FFProbeResult {
    /// Delete the video file by moving it to trash (or permanently if trash is unavailable).
    ///
    /// Prints the file resolution and path. In dryrun mode, only prints without deleting.
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

    /// Rename the video file to include its resolution label.
    ///
    /// Fails if the target path already exists and `overwrite` is false.
    /// In dryrun mode, only prints the rename without performing it.
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
    ///
    /// Handles three cases:
    /// 1. File has no label and no full resolution → adds the label
    /// 2. File has a full resolution pattern (e.g. `1080x1920`) → replaces with label
    /// 3. File has both a full resolution and a label (duplicate) → removes the full resolution
    pub(crate) fn new_path_if_needed(&self) -> anyhow::Result<Option<PathBuf>> {
        let label = self.resolution.label();
        let (name, extension) = cli_tools::get_normalized_file_name_and_extension(&self.file)?;

        // Remove existing full resolution patterns (WxH or HxW) with optional
        // case-insensitive "vertical" prefix and optional dot separator.
        // Word boundaries (\b) prevent partial matches (e.g. "21920x1080" won't match "1920x1080")
        // because all resolution characters (digits and 'x') are word characters,
        // so \b requires a non-word character (like '.') or string edge at each end.
        // For example: "1080x1920", "1920x1080", "vertical.1080x1920", "Vertical1920x1080"
        let (width, height) = (self.resolution.width, self.resolution.height);
        let pattern = if width == height {
            format!(r"(?i)\b(?:vertical\.?)?{width}x{height}\b")
        } else {
            format!(r"(?i)\b(?:vertical\.?)?(?:{width}x{height}|{height}x{width})\b")
        };
        let re = Regex::new(&pattern)?;
        let cleaned_name = re.replace_all(&name, "").into_owned();

        if cleaned_name.contains(&*label) {
            // Label already present in the cleaned name
            if cleaned_name == name {
                // No full resolution was removed, file is already correct
                Ok(None)
            } else {
                // Full resolution was removed but label already exists — fix duplicate
                let mut new_file_name = format!("{cleaned_name}.{extension}");
                remove_extra_dots(&mut new_file_name);
                let new_path = self.file.with_file_name(&new_file_name);
                Ok(Some(new_path))
            }
        } else {
            // Label not present, add it after removing any existing full resolution
            let mut new_file_name = format!("{cleaned_name}.{label}.{extension}");
            remove_extra_dots(&mut new_file_name);
            let new_path = self.file.with_file_name(&new_file_name);
            Ok(Some(new_path))
        }
    }

    /// Print a colored diff showing the old and new file paths after renaming.
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
        assert!(new_path.to_string_lossy().contains("1080p"));
        assert!(!new_path.to_string_lossy().contains("1920x1080"));
    }

    #[test]
    fn new_path_if_needed_replaces_vertical_bare_resolution() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.1080x1920.mp4");
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
        assert!(
            new_path.to_string_lossy().contains("Vertical.1080p"),
            "Expected 'Vertical.1080p' in: {}",
            new_path.display()
        );
        assert!(
            !new_path.to_string_lossy().contains("1080x1920"),
            "Should not contain '1080x1920' in: {}",
            new_path.display()
        );
    }

    #[test]
    fn new_path_if_needed_replaces_vertical_dot_prefix_resolution() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("filename.vertical.1080x1920.mp4");
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
        assert!(
            new_path.to_string_lossy().contains("Vertical.1080p"),
            "Expected 'Vertical.1080p' in: {}",
            new_path.display()
        );
        assert!(
            !new_path.to_string_lossy().contains("1080x1920"),
            "Should not contain '1080x1920' in: {}",
            new_path.display()
        );
        assert_eq!(
            new_path.file_name().unwrap().to_string_lossy(),
            "filename.Vertical.1080p.mp4"
        );
    }

    #[test]
    fn new_path_if_needed_replaces_vertical_no_dot_prefix_resolution() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("filename.vertical1080x1920.mp4");
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
        assert!(
            new_path.to_string_lossy().contains("Vertical.1080p"),
            "Expected 'Vertical.1080p' in: {}",
            new_path.display()
        );
        assert!(
            !new_path.to_string_lossy().contains("vertical1080x1920"),
            "Should not contain 'vertical1080x1920' in: {}",
            new_path.display()
        );
    }

    #[test]
    fn new_path_if_needed_replaces_capitalized_vertical_prefix() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("filename.Vertical.1080x1920.mp4");
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
        assert_eq!(
            new_path.file_name().unwrap().to_string_lossy(),
            "filename.Vertical.1080p.mp4"
        );
    }

    #[test]
    fn new_path_if_needed_replaces_swapped_dimensions() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        // Filename has HxW but detected resolution is WxH (horizontal)
        let file_path = temp_dir.path().join("video.1080x1920.mp4");
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
        assert!(
            new_path.to_string_lossy().contains("1080p"),
            "Expected '1080p' in: {}",
            new_path.display()
        );
        assert!(
            !new_path.to_string_lossy().contains("1080x1920"),
            "Should not contain '1080x1920' in: {}",
            new_path.display()
        );
    }

    #[test]
    fn new_path_if_needed_replaces_swapped_dimensions_vertical() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        // File has 1920x1080 but resolution is vertical (1080x1920)
        // The regex should still match because it checks both WxH and HxW
        let file_path = temp_dir.path().join("video.1920x1080.mp4");
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
        assert!(
            new_path.to_string_lossy().contains("Vertical.1080p"),
            "Expected 'Vertical.1080p' in: {}",
            new_path.display()
        );
        assert!(
            !new_path.to_string_lossy().contains("1920x1080"),
            "Should not contain '1920x1080' in: {}",
            new_path.display()
        );
    }

    #[test]
    fn new_path_if_needed_replaces_720x540_with_540p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.720x540.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 720,
                height: 540,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert_eq!(new_path.file_name().unwrap().to_string_lossy(), "video.540p.mp4");
    }

    #[test]
    fn new_path_if_needed_replaces_vertical_540x720_with_vertical_540p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.540x720.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 540,
                height: 720,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert_eq!(
            new_path.file_name().unwrap().to_string_lossy(),
            "video.Vertical.540p.mp4"
        );
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
    fn new_path_if_needed_fixes_duplicate_960x540_540p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.960x540.540p.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 960,
                height: 540,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert_eq!(new_path.file_name().unwrap().to_string_lossy(), "video.540p.mp4");
    }

    #[test]
    fn new_path_if_needed_fixes_duplicate_1920x1080_1080p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.1920x1080.1080p.mp4");
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
        assert_eq!(new_path.file_name().unwrap().to_string_lossy(), "video.1080p.mp4");
    }

    #[test]
    fn new_path_if_needed_fixes_duplicate_vertical_with_full_resolution_and_label() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.1080x1920.Vertical.1080p.mp4");
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
        assert_eq!(
            new_path.file_name().unwrap().to_string_lossy(),
            "video.Vertical.1080p.mp4"
        );
    }

    #[test]
    fn new_path_if_needed_fixes_duplicate_vertical_prefix_and_label() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.vertical.1080x1920.Vertical.1080p.mp4");
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
        assert_eq!(
            new_path.file_name().unwrap().to_string_lossy(),
            "video.Vertical.1080p.mp4"
        );
    }

    #[test]
    fn new_path_if_needed_no_partial_digit_match() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        // "21920x1080" should NOT match "1920x1080" because \b requires word boundary
        let file_path = temp_dir.path().join("video.21920x1080.mp4");
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
        // The full resolution "1920x1080" should NOT be removed from "21920x1080"
        // because word boundary prevents partial match
        assert!(
            new_path.to_string_lossy().contains("21920x1080"),
            "Should preserve '21920x1080' (no partial match), got: {}",
            new_path.display()
        );
        // But the label should still be added
        assert!(
            new_path.to_string_lossy().contains("1080p"),
            "Should add '1080p' label, got: {}",
            new_path.display()
        );
    }

    #[test]
    fn new_path_if_needed_no_partial_digit_match_trailing() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        // "1920x10800" should NOT match as "1920x1080"
        let file_path = temp_dir.path().join("video.1920x10800.mp4");
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
        // The "1920x10800" should NOT be removed since boundaries prevent partial match
        assert!(
            new_path.to_string_lossy().contains("1920x10800"),
            "Partial digit match should not be removed: {}",
            new_path.display()
        );
    }

    #[test]
    fn new_path_if_needed_960x540_replaces_with_540p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.960x540.mp4");
        std::fs::File::create(&file_path).expect("Failed to create file");

        let result = FFProbeResult {
            file: file_path,
            resolution: Resolution {
                width: 960,
                height: 540,
            },
        };

        let new_path = result.new_path_if_needed().unwrap();
        assert!(new_path.is_some());
        let new_path = new_path.unwrap();
        assert_eq!(new_path.file_name().unwrap().to_string_lossy(), "video.540p.mp4");
    }

    #[test]
    fn new_path_if_needed_fixes_duplicate_720x480_480p() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = temp_dir.path().join("video.720x480.480p.mp4");
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
        assert_eq!(new_path.file_name().unwrap().to_string_lossy(), "video.480p.mp4");
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
