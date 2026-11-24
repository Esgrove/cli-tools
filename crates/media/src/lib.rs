//! Media helpers shared by the video and duplicate finding tools.
//!
//! Provides video resolution parsing and labelling, video metadata from ffprobe,
//! content hashing for files, and the optional `SQLite` backed scan cache (behind the `scan-cache` feature).

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

pub mod file_hash;
pub mod resolution;
#[cfg(feature = "scan-cache")]
pub mod scan_cache;
pub mod video_info;

pub use resolution::Resolution;
