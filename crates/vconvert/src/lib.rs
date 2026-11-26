//! Converting video files to HEVC in an MP4 container.
//!
//! Gathers video files, classifies them with ffprobe and a scan cache,
//! and renames, remuxes, muxes subtitles into, or converts each file with ffmpeg,
//! keeping the files still to process in a `SQLite` database between runs.
//! Nothing here reads command line arguments or prints.
//! Progress is reported to a `ConvertObserver`, so other projects decide how to show it.

pub mod bitrate_limit;
pub mod classification;
pub mod convert;
pub mod database;
pub mod ffmpeg;
pub mod helpers;
pub mod observer;
pub mod options;
pub mod stats;
pub mod types;

pub use bitrate_limit::{BitrateTiersConfig, MinimumBitrate};
pub use convert::{DuplicateOutcome, RunSummary, VideoConvert};
pub use database::{Database, PendingAction, PendingFileFilter};
pub use observer::{
    ConvertObserver, DeleteMethod, DiskSpaceShortage, FilePosition, Operation, Retry, SubtitleMatchProblem,
};
pub use options::{ConvertOptions, HardwareEncoder, ResolvedEncoder, SortOrder};
pub use stats::{AnalysisStats, ConversionStats, RunStats};
pub use types::{ProcessResult, ProcessableFile, ProcessingOutcome, SkipReason, SubtitleFile, VideoInfo};
