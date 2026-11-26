//! Progress reporting for video convert runs.
//!
//! The library prints nothing.
//! Every step of a run that a user may want to see is reported to a `ConvertObserver`,
//! together with the plain data describing it, so the caller decides what to show and how.

use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use crate::convert::DuplicateOutcome;
use crate::stats::AnalysisStats;
use crate::types::{ProcessResult, ProcessableFile, SkipReason};

/// The per-file operation being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Remux a HEVC or AV1 file to MP4 without re-encoding.
    Remux,
    /// Mux external subtitles and filter movie streams without re-encoding.
    SubtitleMux,
    /// Convert the video stream to HEVC.
    Convert,
}

/// A fallback attempt after an ffmpeg run did not produce a usable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// The remux failed, so it is retried with the audio transcoded to AAC.
    AacAudio { status: ExitStatus },
    /// The conversion with CUDA filters failed, so it is retried with CPU filtering.
    CpuFilters,
    /// The output was larger than the input, so the file is converted again at a lower quality.
    LowerQuality {
        output_size: u64,
        input_size: u64,
        quality_level: u8,
    },
}

/// Why a subtitle sidecar was not assigned to a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleMatchProblem {
    /// No video in the same directory matches the subtitle name.
    NoMatchingVideo,
    /// Several videos match the subtitle name equally well.
    Ambiguous,
}

/// How a replaced file is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMethod {
    /// The file is deleted permanently.
    Delete,
    /// The file is moved to the trash.
    Trash,
}

/// Position of a file within the files processed in one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePosition {
    /// One-based index of the file.
    pub index: usize,
    /// Number of files in the run.
    pub total: usize,
}

/// Free disk space that is missing before a file can be processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpaceShortage {
    /// Bytes that need to be free.
    pub required: u64,
    /// Bytes that are free.
    pub available: u64,
}

/// Receives the progress of a conversion run.
///
/// Every method has an empty default, so an implementation only handles the events it needs.
/// Analysis runs ffprobe on several threads, so implementations must be `Sync`.
#[allow(unused_variables)]
pub trait ConvertObserver: Sync {
    /// Input files were gathered from the input paths.
    fn files_gathered(&self, file_count: usize, duration: Duration) {}

    /// A subtitle sidecar was not assigned to any video.
    fn subtitle_not_matched(&self, subtitle: &Path, problem: SubtitleMatchProblem) {}

    /// Analysis of the gathered files is starting.
    fn analysis_started(&self, file_count: usize) {}

    /// The scan cache was checked, and the misses will be probed with ffprobe.
    fn scan_cache_checked(&self, hits: usize, misses: usize) {}

    /// Probing the scan cache misses with ffprobe is starting.
    fn probing_started(&self, file_count: usize) {}

    /// One file was probed. Called from several threads.
    fn file_probed(&self) {}

    /// All scan cache misses were probed.
    fn probing_finished(&self) {}

    /// Writing new probe results to the scan cache failed.
    fn scan_cache_write_failed(&self, error: &anyhow::Error) {}

    /// A file needs no processing.
    ///
    /// An existing output is reported as a skip when duplicates are kept,
    /// and through `duplicate_handled` when they are deleted.
    fn file_skipped(&self, path: &Path, reason: &SkipReason) {}

    /// A source file whose converted output already exists was handled.
    fn duplicate_handled(&self, source: &Path, target: &Path, outcome: &DuplicateOutcome) {}

    /// Analysis finished.
    fn analysis_finished(&self, stats: &AnalysisStats, file_count: usize, duration: Duration) {}

    /// A file is being renamed to carry its codec label.
    fn rename_started(&self, position: FilePosition, from: &Path, to: &Path, dry_run: bool) {}

    /// A rename was skipped because the new name already exists.
    fn rename_skipped_existing(&self, to: &Path) {}

    /// A rename failed.
    fn rename_failed(&self, from: &Path, error: &anyhow::Error) {}

    /// All renames finished.
    fn renames_finished(&self, renamed_count: usize, total_count: usize, duration: Duration) {}

    /// The files still to process were stored in the database.
    fn pending_files_saved(&self, updated_count: usize) {}

    /// Storing the files still to process in the database failed.
    fn pending_files_save_failed(&self, error: &anyhow::Error) {}

    /// Database rows for missing files and files changed since the scan were removed.
    fn database_cleaned(&self, missing_count: usize, changed_count: usize) {}

    /// Pending files matching the options were read from the database.
    fn pending_files_loaded(&self, file_count: usize) {}

    /// A pending file was dropped from the database because its output is blocked.
    fn pending_file_blocked(&self, path: &Path, reason: &SkipReason) {}

    /// Removing a file from the database failed.
    fn database_removal_failed(&self, path: &Path, error: &anyhow::Error) {}

    /// Processing a file is starting.
    ///
    /// The quality level is set for conversions only.
    fn file_started(
        &self,
        operation: Operation,
        position: FilePosition,
        file: &ProcessableFile,
        quality_level: Option<u8>,
    ) {
    }

    /// The ffmpeg command that a dry run would execute.
    fn dry_run_command(&self, command: &Command) {}

    /// A failed ffmpeg run is retried with fallback settings.
    fn retrying(&self, retry: Retry) {}

    /// Processing a file finished, successfully or not.
    fn file_finished(
        &self,
        operation: Operation,
        position: FilePosition,
        file: &ProcessableFile,
        result: &ProcessResult,
        duration: Duration,
    ) {
    }

    /// A file disappeared before it was processed, and it was dropped from the database.
    fn file_missing(&self, path: &Path) {}

    /// A file is currently locked by another live process and was skipped.
    fn file_locked(&self, path: &Path, holder_tool: &str, holder_pid: u32) {}

    /// Processing stopped at the file count limit.
    fn limit_reached(&self, limit: usize) {}

    /// Processing stopped because the output disk does not have enough free space.
    fn out_of_disk_space(&self, path: &Path, shortage: DiskSpaceShortage) {}

    /// A replaced file is about to be removed.
    fn deleting(&self, path: &Path, method: DeleteMethod) {}

    /// Removing a replaced or invalid file failed.
    fn delete_failed(&self, path: &Path, error: &anyhow::Error) {}

    /// Removing a partial ffmpeg output failed.
    ///
    /// A leftover output would later be taken for a finished conversion.
    fn partial_output_removal_failed(&self, path: &Path, error: &std::io::Error) {}
}

impl Operation {
    /// Lowercase name of the operation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Remux => "remux",
            Self::SubtitleMux => "subtitle-mux",
            Self::Convert => "convert",
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
