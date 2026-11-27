//! Moving one file with a rename, or with a verified copy that reports progress, and cross-device error detection.
//!
//! The verified copy itself comes from `cli_tools_common::move_file_by_copy`.
//! This module adds the recovery for renames that report an error after moving the file,
//! which happens on some network shares, and maps the result to the mover's errors.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use indicatif::ProgressBar;

use cli_tools_common::{OverwritePolicy, move_file_by_copy, print_yellow};

/// Errors that can occur when moving a single file.
#[derive(Debug)]
pub(super) enum MoveError {
    /// The move was aborted by the user (Ctrl+C during copy).
    Aborted,
    /// The move failed with an error.
    Failed(anyhow::Error),
}

/// Move a single file from source to destination.
///
/// Tries `fs::rename` first for same-device moves.
/// Otherwise copies the file to a temporary sibling of the destination,
/// verifies its size, publishes it, and deletes the source.
/// Copied bytes advance the progress bar, and setting `abort_flag` stops the copy between chunks.
pub(super) fn move_single_file(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    progress_bar: &ProgressBar,
    abort_flag: &AtomicBool,
) -> Result<(), MoveError> {
    match fs::rename(source, destination) {
        Ok(()) => {
            progress_bar.inc(expected_size);
            return Ok(());
        }
        Err(rename_error) => {
            // Some network file systems report a rename error even though the file was moved.
            if fs::symlink_metadata(source).is_err() {
                if let Ok(meta) = fs::symlink_metadata(destination) {
                    if meta.file_type().is_file() && meta.len() == expected_size {
                        progress_bar.inc(expected_size);
                        return Ok(());
                    }
                    return Err(MoveError::Failed(anyhow::anyhow!(
                        "Rename of {} -> {} reported an error but moved the file. \
                         Destination exists but size verification failed \
                         (expected {expected_size} bytes). Original error: {rename_error}",
                        source.display(),
                        destination.display(),
                    )));
                }
                return Err(MoveError::Failed(anyhow::anyhow!(
                    "File lost during rename of {} -> {}: source no longer exists \
                     and destination was not created. The file may have been moved \
                     or deleted by another process. Original error: {rename_error}",
                    source.display(),
                    destination.display(),
                )));
            }

            if !is_cross_device_error(&rename_error) {
                progress_bar.suspend(|| {
                    print_yellow!(
                        "Rename failed for {} -> {} (error: {rename_error}), falling back to copy",
                        source.display(),
                        destination.display(),
                    );
                });
            }
        }
    }

    move_by_verified_copy(source, destination, expected_size, progress_bar, abort_flag)
}

/// Move a file by copying it to a temporary sibling of the destination, then deleting the source.
///
/// The copy must have `expected_size` bytes before it is published to the destination.
/// Copied bytes advance the progress bar, and setting `abort_flag` stops the copy between chunks.
/// A failed or aborted copy leaves no destination file and keeps the source.
pub(super) fn move_by_verified_copy(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    progress_bar: &ProgressBar,
    abort_flag: &AtomicBool,
) -> Result<(), MoveError> {
    if abort_flag.load(Ordering::SeqCst) {
        return Err(MoveError::Aborted);
    }
    let verify_size = |_: &Path, copied: &Path| -> io::Result<()> {
        let copied_size = fs::metadata(copied)?.len();
        if copied_size == expected_size {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Size verification failed: expected {expected_size} bytes, got {copied_size} bytes. \
                 Original file preserved."
            )))
        }
    };
    let report_progress = |bytes: u64| {
        progress_bar.inc(bytes);
        !abort_flag.load(Ordering::SeqCst)
    };

    match move_file_by_copy(
        source,
        destination,
        OverwritePolicy::Error,
        Some(&verify_size),
        Some(&report_progress),
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::Interrupted && abort_flag.load(Ordering::SeqCst) => {
            Err(MoveError::Aborted)
        }
        Err(error) => Err(MoveError::Failed(anyhow::Error::new(error).context(format!(
            "Failed to copy {} -> {}",
            source.display(),
            destination.display()
        )))),
    }
}

/// Check whether an I/O error means the rename crossed devices.
///
/// Windows reports `ERROR_NOT_SAME_DEVICE` (17) and Unix reports `EXDEV` (18).
pub(super) fn is_cross_device_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(17) || error.raw_os_error() == Some(18)
}
