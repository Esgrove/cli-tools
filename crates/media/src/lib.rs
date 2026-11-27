//! Media helpers shared by the video and duplicate finding tools.
//!
//! Provides the canonical video extension list, video resolution parsing and labelling,
//! video metadata from ffprobe, content hashing for files,
//! and the optional `SQLite` backed scan cache (behind the `scan-cache` feature).

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod file_hash;
pub mod resolution;
#[cfg(feature = "scan-cache")]
pub mod scan_cache;
pub mod video_info;

use std::path::{Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

pub use resolution::Resolution;

/// Canonical lowercase video file extensions recognized across all media tools.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "m4v", "webm", "ts", "mpg", "mpeg",
];

/// Return whether `extension` (with or without a leading dot, case-insensitive) is a known video extension.
#[must_use]
pub fn is_video_extension(extension: &str) -> bool {
    let trimmed = extension.trim().trim_start_matches('.');
    VIDEO_EXTENSIONS.iter().any(|known| trimmed.eq_ignore_ascii_case(known))
}

/// Return whether `path` has a known video file extension.
#[must_use]
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(is_video_extension)
}

/// Discover regular video files under `root` silently, returning them in sorted order.
///
/// A single video file returns just that file, and a non-video file or missing path returns an error.
/// Hidden files, hidden or system directories, and symbolic links are skipped during directory walks.
///
/// # Errors
/// Returns an error if `root` does not exist or is a file without a recognized video extension.
pub fn discover_video_files(root: &Path, recurse: bool) -> Result<Vec<PathBuf>> {
    let mut video_files = Vec::new();

    if root.is_file() {
        if is_video_file(root) {
            video_files.push(root.to_path_buf());
        } else {
            anyhow::bail!("File '{}' is not a supported video file", root.display());
        }
    } else if root.is_dir() {
        let walker = if recurse {
            WalkDir::new(root)
        } else {
            WalkDir::new(root).max_depth(1)
        };
        for entry in walker
            .into_iter()
            .filter_entry(|entry| !cli_tools_common::should_skip_entry(entry))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.path().to_path_buf();
            if is_video_file(&path) {
                video_files.push(path);
            }
        }
    } else {
        anyhow::bail!("Path '{}' does not exist", root.display());
    }

    video_files.sort();
    Ok(video_files)
}

#[cfg(test)]
mod test_video_extensions {
    use super::*;

    #[test]
    fn recognizes_standard_video_extensions_case_insensitively() {
        for ext in VIDEO_EXTENSIONS {
            assert!(is_video_extension(ext));
            assert!(is_video_extension(&ext.to_ascii_uppercase()));
            assert!(is_video_extension(&format!(".{ext}")));
        }
        assert!(is_video_file(Path::new("/videos/Movie.MPEG")));
        assert!(!is_video_extension("webp"));
        assert!(!is_video_file(Path::new("/images/photo.jpg")));
    }
}

#[cfg(test)]
mod test_discover_video_files {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_sorted_videos_respecting_recursion_hidden_entries_and_errors() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("videos");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(root.join(".hidden_dir")).unwrap();
        fs::write(root.join("b.mkv"), "").unwrap();
        fs::write(root.join("a.mp4"), "").unwrap();
        fs::write(root.join("notes.txt"), "").unwrap();
        fs::write(root.join(".hidden.mp4"), "").unwrap();
        fs::write(root.join("nested/c.webm"), "").unwrap();
        fs::write(root.join(".hidden_dir/d.mp4"), "").unwrap();

        let shallow = discover_video_files(&root, false).unwrap();
        assert_eq!(shallow, vec![root.join("a.mp4"), root.join("b.mkv")]);

        let deep = discover_video_files(&root, true).unwrap();
        assert_eq!(
            deep,
            vec![root.join("a.mp4"), root.join("b.mkv"), root.join("nested/c.webm")]
        );

        assert_eq!(
            discover_video_files(&root.join("a.mp4"), false).unwrap(),
            vec![root.join("a.mp4")]
        );
        assert!(discover_video_files(&root.join("notes.txt"), false).is_err());
        assert!(discover_video_files(&root.join("missing"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinked_video_files_during_directory_walk() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let root = temp.path().join("videos");
        let outside = temp.path().join("outside.mp4");
        fs::create_dir(&root).unwrap();
        fs::write(&outside, "").unwrap();
        fs::write(root.join("real.mp4"), "").unwrap();
        symlink(&outside, root.join("linked.mp4")).unwrap();

        let found = discover_video_files(&root, true).unwrap();
        assert_eq!(found, vec![root.join("real.mp4")]);
    }
}
