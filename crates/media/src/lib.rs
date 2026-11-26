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

use std::path::Path;

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
