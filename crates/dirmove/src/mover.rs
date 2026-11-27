//! Reliable file moving helpers for `dirmove`.
//!
//! Moves use a fast `rename` first,
//! then fall back to chunked copy-verify-delete for cross-device moves.
//! Each step and the copied bytes are reported to a `MoveObserver`, so callers decide what to show.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use cli_tools_common::{TargetReservations, path_to_filename_string, unique_sibling_path};
use cli_tools_media::file_hash::hash_file;

/// Size of chunks used when copying files across devices.
const COPY_BUFFER_SIZE: usize = 4 * 1024 * 1024;

/// One step of moving a batch of files into a directory.
#[derive(Debug)]
pub enum MoveEvent<'a> {
    /// The missing target directory was created.
    CreatedDirectory(&'a Path),
    /// Moving started, with the total size of the files in bytes.
    Started {
        /// Total size of the files to move.
        total_bytes: u64,
    },
    /// Moving one file started.
    FileStarted {
        /// Zero based index of the file in the batch.
        index: usize,
        /// Number of files in the batch.
        count: usize,
        /// Name of the file in the target directory.
        name: &'a str,
    },
    /// The source path has no file name, so it was not moved.
    MissingFileName(&'a Path),
    /// The source was skipped because another file in the batch has the same target name.
    DuplicateName(&'a Path),
    /// The source was skipped because the target file exists and overwriting is disabled.
    SkippedExisting(&'a Path),
    /// The source already is the target file.
    AlreadyInPlace(&'a Path),
    /// The target is a directory, so overwriting cannot replace it with the file.
    DirectoryInTheWay(&'a Path),
    /// Renaming failed for a reason other than crossing devices, so the file is copied instead.
    RenameFailed {
        /// File being moved.
        source: &'a Path,
        /// Target path of the move.
        destination: &'a Path,
        /// Error from the rename.
        error: &'a io::Error,
    },
    /// A file was copied across devices.
    Copied,
    /// The copy matched the source BLAKE3 hash.
    HashVerified(&'a blake3::Hash),
    /// A file with this name was moved.
    Moved(&'a str),
    /// Moving the source failed.
    Failed {
        /// File that could not be moved.
        source: &'a Path,
        /// Why the move failed.
        error: &'a anyhow::Error,
    },
    /// Every file in the batch was handled, with the outcome of each.
    Finished(&'a MoveReport),
}

/// Summary of a batch move operation.
#[derive(Debug, Default)]
pub struct MoveReport {
    /// Source paths that were moved successfully.
    pub moved_files: Vec<PathBuf>,
    /// Source paths left in place because the destination already exists.
    pub skipped_files: Vec<PathBuf>,
    /// Source paths that could not be moved.
    pub failed_files: Vec<PathBuf>,
}

/// Receives the steps and byte progress of moving files.
///
/// Every method does nothing by default, and `()` reports nothing.
pub trait MoveObserver {
    /// One step of the move happened.
    fn on_event(&mut self, _event: MoveEvent<'_>) {}

    /// This many more bytes were moved, copied, or skipped.
    fn on_progress(&mut self, _bytes: u64) {}
}

impl MoveObserver for () {}

impl MoveReport {
    /// Return the number of files moved successfully.
    #[must_use]
    pub const fn moved_count(&self) -> usize {
        self.moved_files.len()
    }
}

/// Move files to the target directory, creating it if needed.
///
/// Uses `rename` first for fast same-device moves, then falls back to copy-verify-delete for cross-device moves.
///
/// # Errors
///
/// Returns an error if the target path cannot be created or is not a directory.
pub fn move_files_to_target_dir(
    target_directory: &Path,
    files: &[PathBuf],
    overwrite: bool,
    observer: &mut impl MoveObserver,
) -> anyhow::Result<MoveReport> {
    process_file_moves(target_directory, files, overwrite, false, observer)
}

/// Preview file moves without creating a directory or moving files.
/// The report records eligible sources in `moved_files` and reports the same collisions as a real move.
///
/// # Errors
/// Returns an error if the target path exists but is not a directory.
pub fn preview_file_moves(
    target_directory: &Path,
    files: &[PathBuf],
    overwrite: bool,
    observer: &mut impl MoveObserver,
) -> anyhow::Result<MoveReport> {
    process_file_moves(target_directory, files, overwrite, true, observer)
}

/// Check each destination in order and move eligible files unless this is a preview.
fn process_file_moves(
    target_directory: &Path,
    files: &[PathBuf],
    overwrite: bool,
    dry_run: bool,
    observer: &mut impl MoveObserver,
) -> anyhow::Result<MoveReport> {
    if !target_directory.exists() {
        if !dry_run {
            fs::create_dir_all(target_directory)?;
            observer.on_event(MoveEvent::CreatedDirectory(target_directory));
        }
    } else if !target_directory.is_dir() {
        anyhow::bail!(
            "Target path exists but is not a directory: {}",
            target_directory.display()
        );
    }

    let file_sizes = collect_file_sizes(files);
    observer.on_event(MoveEvent::Started {
        total_bytes: file_sizes.values().sum(),
    });

    let mut report = MoveReport::default();
    // Names are sanitized to valid UTF-8, so two sources can end up with the same destination.
    // Overwrite only replaces files that existed before the move, never another file moved in the same batch.
    let mut reservations = TargetReservations::new();
    for (index, file_path) in files.iter().enumerate() {
        let file_name = path_to_filename_string(file_path);
        if file_name.is_empty() {
            observer.on_event(MoveEvent::MissingFileName(file_path));
            report.failed_files.push(file_path.clone());
            continue;
        }

        observer.on_event(MoveEvent::FileStarted {
            index,
            count: files.len(),
            name: &file_name,
        });
        let expected_size = file_sizes.get(file_path).copied().unwrap_or(0);
        let new_path = target_directory.join(&file_name);

        if !reservations.reserve(&new_path) {
            observer.on_event(MoveEvent::DuplicateName(file_path));
            observer.on_progress(expected_size);
            report.skipped_files.push(file_path.clone());
            continue;
        }

        if fs::symlink_metadata(&new_path).is_ok() && !overwrite {
            observer.on_event(MoveEvent::SkippedExisting(&new_path));
            observer.on_progress(expected_size);
            report.skipped_files.push(file_path.clone());
            continue;
        }

        if cli_tools_common::paths_refer_to_same_entry(file_path, &new_path) {
            observer.on_event(MoveEvent::AlreadyInPlace(&new_path));
            observer.on_progress(expected_size);
            report.skipped_files.push(file_path.clone());
            continue;
        }

        // The existing destination is replaced by the final rename, never removed up front,
        // so a failed move leaves it in place.
        if overwrite && fs::symlink_metadata(&new_path).is_ok_and(|metadata| metadata.is_dir()) {
            observer.on_event(MoveEvent::DirectoryInTheWay(&new_path));
            observer.on_progress(expected_size);
            report.failed_files.push(file_path.clone());
            continue;
        }

        if dry_run {
            report.moved_files.push(file_path.clone());
            continue;
        }

        match move_single_file(file_path, &new_path, expected_size, overwrite, observer) {
            Ok(()) => {
                observer.on_event(MoveEvent::Moved(&file_name));
                report.moved_files.push(file_path.clone());
            }
            Err(error) => {
                observer.on_event(MoveEvent::Failed {
                    source: file_path,
                    error: &error,
                });
                if expected_size > 0 {
                    observer.on_progress(expected_size);
                }
                report.failed_files.push(file_path.clone());
            }
        }
    }

    observer.on_event(MoveEvent::Finished(&report));
    Ok(report)
}

/// Move a single file, falling back to copy-verify-delete only when rename cannot cross devices.
fn move_single_file(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    overwrite: bool,
    observer: &mut impl MoveObserver,
) -> anyhow::Result<()> {
    // This returns an error for cross-drive moves, which then fall back to the custom copy path.
    match fs::rename(source, destination) {
        Ok(()) => {
            observer.on_progress(expected_size);
            return Ok(());
        }
        Err(rename_error) => {
            if fs::symlink_metadata(source).is_err() {
                // On some network filesystems, rename succeeds remotely while returning an error locally.
                // Only treat as moved when not overwriting a pre-existing file
                // and the destination is a regular file matching the non-zero source size.
                if !overwrite
                    && expected_size > 0
                    && fs::symlink_metadata(destination)
                        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected_size)
                {
                    observer.on_progress(expected_size);
                    return Ok(());
                }
                anyhow::bail!(
                    "rename of {} -> {} reported an error and source is gone. Original error: {rename_error}",
                    source.display(),
                    destination.display()
                );
            }

            if !is_cross_device_error(&rename_error) {
                observer.on_event(MoveEvent::RenameFailed {
                    source,
                    destination,
                    error: &rename_error,
                });
                return Err(anyhow::Error::new(rename_error).context(format!(
                    "Failed to rename {} -> {}",
                    source.display(),
                    destination.display()
                )));
            }
        }
    }

    if fs::symlink_metadata(source).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        move_symlink_via_temp(source, destination)?;
        observer.on_progress(expected_size);
        return Ok(());
    }

    // Copy to a temporary name first so an existing destination is only replaced by a verified copy.
    let temporary = unique_sibling_path(destination, "dirmove-tmp");
    let source_hash = copy_file_with_progress(source, &temporary, expected_size, observer)?;
    observer.on_event(MoveEvent::Copied);

    if let Err(error) = verify_copied_file(source, &temporary, expected_size, &source_hash) {
        let _ = remove_failed_destination(&temporary);
        return Err(error);
    }
    observer.on_event(MoveEvent::HashVerified(&source_hash));
    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = remove_failed_destination(&temporary);
        return Err(anyhow::Error::new(error).context(format!(
            "Failed to move verified copy into place at {}. Original file preserved.",
            destination.display()
        )));
    }

    fs::remove_file(source).map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "File copied successfully but failed to delete original: {}. You may have a duplicate.",
            source.display()
        ))
    })?;

    Ok(())
}

/// Recreate a symbolic link at `destination` via a temporary sibling and remove `source`.
///
/// Windows tells file links and directory links apart,
/// so the new link keeps the kind of the source link and a directory link is removed as a directory.
pub(crate) fn move_symlink_via_temp(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let link_target = fs::read_link(source)?;
    let is_directory_link = is_directory_symlink(source);
    let temporary = unique_sibling_path(destination, "dirmove-tmp");
    create_symlink(&link_target, &temporary, is_directory_link)?;
    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = remove_symlink(&temporary, is_directory_link);
        return Err(anyhow::Error::new(error).context(format!(
            "Failed to move recreated symlink into place at {}",
            destination.display()
        )));
    }
    remove_symlink(source, is_directory_link)?;
    Ok(())
}

/// Check if `path` is a symbolic link that Windows created as a directory link.
///
/// Unix links have no kind, so this is always false there.
fn is_directory_symlink(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink_dir())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// Create a symbolic link at `link` pointing to `target`, as a directory link on Windows when requested.
fn create_symlink(target: &Path, link: &Path, is_directory_link: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        if is_directory_link {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = is_directory_link;
        std::os::unix::fs::symlink(target, link)
    }
}

/// Remove the symbolic link at `path` without touching what it points to.
fn remove_symlink(path: &Path, is_directory_link: bool) -> io::Result<()> {
    if is_directory_link {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Copy a file in chunks while reporting the copied bytes.
fn copy_file_with_progress(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    observer: &mut impl MoveObserver,
) -> anyhow::Result<blake3::Hash> {
    match copy_file_inner(source, destination, expected_size, observer) {
        Ok(hash) => Ok(hash),
        Err(copy_error) => {
            if let Err(cleanup_error) = remove_failed_destination(destination) {
                anyhow::bail!(
                    "{copy_error:#}. Also failed to remove partial destination {}: {cleanup_error:#}. Original file preserved.",
                    destination.display()
                );
            }
            Err(copy_error)
        }
    }
}

/// Inner copy loop. File handles are dropped before cleanup can run.
fn copy_file_inner(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    observer: &mut impl MoveObserver,
) -> anyhow::Result<blake3::Hash> {
    let mut source_file = File::open(source)?;
    let mut destination_file = File::create(destination)?;
    let mut source_hasher = blake3::Hasher::new();
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    let mut bytes_copied = 0;

    loop {
        let bytes_read = source_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        let Some(chunk) = buffer.get(..bytes_read) else {
            anyhow::bail!(
                "Read returned {bytes_read} bytes for a {}-byte copy buffer",
                buffer.len()
            );
        };
        source_hasher.update(chunk);
        destination_file.write_all(chunk)?;
        bytes_copied += bytes_read as u64;
        observer.on_progress(bytes_read as u64);
    }

    destination_file.flush()?;

    // A moved file keeps its modification time and permissions, as a rename on the same device would.
    let source_metadata = source_file.metadata()?;
    destination_file.set_modified(source_metadata.modified()?)?;
    drop(destination_file);
    fs::set_permissions(destination, source_metadata.permissions())?;

    if bytes_copied != expected_size {
        anyhow::bail!(
            "Incomplete copy for {}: expected {} bytes, copied {} bytes. Original file preserved.",
            source.display(),
            expected_size,
            bytes_copied
        );
    }

    Ok(source_hasher.finalize())
}

/// Verify that a copied file has the expected size and BLAKE3 hash.
fn verify_copied_file(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_hash: &blake3::Hash,
) -> anyhow::Result<()> {
    let copied_size = fs::metadata(destination)?.len();
    if copied_size != expected_size {
        if let Err(cleanup_error) = remove_failed_destination(destination) {
            anyhow::bail!(
                "Size verification failed for {}: expected {} bytes, got {} bytes. Also failed to remove invalid destination {}: {cleanup_error:#}. Original file preserved.",
                source.display(),
                expected_size,
                copied_size,
                destination.display()
            );
        }
        anyhow::bail!(
            "Size verification failed for {}: expected {} bytes, got {} bytes. Original file preserved.",
            source.display(),
            expected_size,
            copied_size
        );
    }

    let copied_hash = hash_file(destination)?;
    if copied_hash != *expected_hash {
        if let Err(cleanup_error) = remove_failed_destination(destination) {
            anyhow::bail!(
                "Hash verification failed for {} -> {}. Expected BLAKE3 {}, got {}. Also failed to remove invalid destination: {cleanup_error:#}. Original file preserved.",
                source.display(),
                destination.display(),
                expected_hash,
                copied_hash
            );
        }
        anyhow::bail!(
            "Hash verification failed for {} -> {}. Expected BLAKE3 {}, got {}. Original file preserved.",
            source.display(),
            destination.display(),
            expected_hash,
            copied_hash
        );
    }

    Ok(())
}

/// Remove a destination file after a failed copy or verification step.
fn remove_failed_destination(destination: &Path) -> anyhow::Result<()> {
    match fs::remove_file(destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => anyhow::bail!("{}: {error}", destination.display()),
    }
}

/// Check if an I/O error indicates a cross-device move attempt.
///
/// The standard library maps `EXDEV` on Unix and `ERROR_NOT_SAME_DEVICE` on Windows to this kind.
fn is_cross_device_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::CrossesDevices
}

/// Collect the size of each file that has readable metadata.
fn collect_file_sizes(files: &[PathBuf]) -> HashMap<PathBuf, u64> {
    files
        .iter()
        .filter_map(|file_path| {
            fs::metadata(file_path)
                .ok()
                .map(|metadata| (file_path.clone(), metadata.len()))
        })
        .collect()
}

#[cfg(test)]
mod test_observer {
    use super::*;

    /// Observer that counts the reported bytes.
    #[derive(Default)]
    pub struct ByteCounter {
        /// Total bytes reported so far.
        pub bytes: u64,
    }

    /// Observer that records the name of each reported event.
    #[derive(Default)]
    pub struct EventRecorder {
        /// Event variant names in the order they were reported.
        pub events: Vec<String>,
        /// Moved file count from the `Finished` event.
        pub finished_moved_count: Option<usize>,
    }

    impl MoveObserver for ByteCounter {
        fn on_progress(&mut self, bytes: u64) {
            self.bytes += bytes;
        }
    }

    impl MoveObserver for EventRecorder {
        fn on_event(&mut self, event: MoveEvent<'_>) {
            if let MoveEvent::Finished(report) = &event {
                self.finished_moved_count = Some(report.moved_count());
            }
            let debug = format!("{event:?}");
            let name = debug
                .split(|character: char| !character.is_alphanumeric())
                .next()
                .unwrap_or_default();
            self.events.push(name.to_string());
        }
    }
}

#[cfg(test)]
mod test_copy_verification {
    use super::test_observer::ByteCounter;
    use super::*;

    #[test]
    fn copy_file_with_progress_preserves_contents() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        let contents = vec![42; COPY_BUFFER_SIZE + 13];
        fs::write(&source, &contents)?;
        let mut progress = ByteCounter::default();

        copy_file_with_progress(&source, &destination, contents.len() as u64, &mut progress)?;

        assert_eq!(fs::read(destination)?, contents);
        assert!(source.exists());
        Ok(())
    }

    #[test]
    fn copy_file_with_progress_keeps_modification_time_and_permissions() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        fs::write(&source, b"contents")?;
        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        File::options().write(true).open(&source)?.set_modified(old_time)?;
        let mut permissions = fs::metadata(&source)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions)?;

        copy_file_with_progress(&source, &destination, 8, &mut ByteCounter::default())?;

        let copied = fs::metadata(&destination)?;
        assert_eq!(copied.modified()?, old_time);
        assert!(copied.permissions().readonly());
        Ok(())
    }

    #[test]
    fn verify_copied_file_rejects_hash_mismatch() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        fs::write(&source, b"source")?;
        fs::write(&destination, b"target")?;

        let result = verify_copied_file(&source, &destination, 6, &blake3::hash(b"source"));

        assert!(result.is_err());
        assert!(source.exists());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn copy_file_with_progress_removes_incomplete_destination() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.bin");
        let destination = temp_directory.path().join("destination.bin");
        fs::write(&source, b"source")?;
        let mut progress = ByteCounter::default();

        let result = copy_file_with_progress(&source, &destination, 10, &mut progress);

        assert!(result.is_err());
        assert!(source.exists());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn verify_copied_file_rejects_size_mismatch() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.bin");
        let destination = temp_directory.path().join("destination.bin");
        fs::write(&source, b"source")?;
        fs::write(&destination, b"short")?;

        let result = verify_copied_file(&source, &destination, 6, &blake3::hash(b"source"));

        assert!(result.is_err());
        assert!(source.exists());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn verify_copied_file_accepts_matching_contents() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.bin");
        let destination = temp_directory.path().join("destination.bin");
        fs::write(&source, b"matching")?;
        fs::write(&destination, b"matching")?;

        verify_copied_file(&source, &destination, 8, &blake3::hash(b"matching"))?;

        assert!(source.exists());
        assert!(destination.exists());
        Ok(())
    }

    #[test]
    fn copy_failure_reports_destination_cleanup_failure() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.bin");
        let destination = temp_directory.path().join("destination");
        fs::write(&source, b"source")?;
        fs::create_dir(&destination)?;
        let mut progress = ByteCounter::default();

        let error = copy_file_with_progress(&source, &destination, 6, &mut progress)
            .expect_err("directory destination should fail creation and cleanup");

        assert!(error.to_string().contains("Also failed to remove partial destination"));
        assert!(source.exists());
        assert!(destination.is_dir());
        Ok(())
    }
}

#[cfg(test)]
mod test_move_helpers {
    use super::test_observer::ByteCounter;
    use super::*;

    #[test]
    fn move_single_file_accepts_matching_destination_when_source_is_gone() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("missing.bin");
        let destination = temp_directory.path().join("destination.bin");
        fs::write(&destination, b"existing")?;
        let mut progress = ByteCounter::default();

        move_single_file(&source, &destination, 8, false, &mut progress)?;

        assert_eq!(progress.bytes, 8);
        assert_eq!(fs::read(&destination)?, b"existing");
        Ok(())
    }

    #[test]
    fn move_single_file_rejects_mismatched_destination_when_source_is_gone() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("missing.bin");
        let destination = temp_directory.path().join("destination.bin");
        fs::write(&destination, b"short")?;
        let mut progress = ByteCounter::default();

        let error = move_single_file(&source, &destination, 8, false, &mut progress)
            .expect_err("mismatched destination should not be treated as completed move");

        assert!(error.to_string().contains("source is gone"));
        assert!(destination.exists());
        Ok(())
    }

    #[test]
    fn remove_failed_destination_accepts_missing_path() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        remove_failed_destination(&temp_directory.path().join("missing.bin"))
    }

    #[test]
    fn recognizes_cross_device_error_kind() {
        assert!(is_cross_device_error(&io::Error::from(io::ErrorKind::CrossesDevices)));
        assert!(!is_cross_device_error(&io::Error::from(io::ErrorKind::AlreadyExists)));
        assert!(!is_cross_device_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[cfg(unix)]
    #[test]
    fn recognizes_unix_cross_device_error_code() {
        // EXDEV is 18 and EEXIST is 17 on Linux and macOS.
        assert!(is_cross_device_error(&io::Error::from_raw_os_error(18)));
        assert!(!is_cross_device_error(&io::Error::from_raw_os_error(17)));
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_windows_cross_device_error_code() {
        // ERROR_NOT_SAME_DEVICE is 17 and ERROR_ALREADY_EXISTS is 183 on Windows.
        assert!(is_cross_device_error(&io::Error::from_raw_os_error(17)));
        assert!(!is_cross_device_error(&io::Error::from_raw_os_error(183)));
    }

    #[test]
    fn collect_file_sizes_ignores_missing_files() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let first = temp_directory.path().join("first.bin");
        let second = temp_directory.path().join("second.bin");
        let missing = temp_directory.path().join("missing.bin");
        fs::write(&first, b"one")?;
        fs::write(&second, b"second")?;

        let sizes = collect_file_sizes(&[first.clone(), missing.clone(), second.clone()]);

        assert_eq!(sizes.get(&first), Some(&3));
        assert_eq!(sizes.get(&second), Some(&6));
        assert!(!sizes.contains_key(&missing));
        Ok(())
    }
}

#[cfg(test)]
mod test_move_files {
    use super::test_observer::EventRecorder;
    use super::*;

    #[test]
    fn reports_events_in_order_and_finishes_with_the_report() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let input = temp_directory.path().join("input");
        let output = temp_directory.path().join("output");
        fs::create_dir(&input)?;
        fs::write(input.join("one.txt"), "one")?;
        fs::write(input.join("two.txt"), "two")?;
        fs::create_dir(&output)?;
        fs::write(output.join("two.txt"), "old")?;

        let files = vec![input.join("one.txt"), input.join("two.txt")];
        let mut recorder = EventRecorder::default();
        let report = move_files_to_target_dir(&output, &files, false, &mut recorder)?;

        assert_eq!(
            recorder.events,
            [
                "Started",
                "FileStarted",
                "Moved",
                "FileStarted",
                "SkippedExisting",
                "Finished"
            ]
        );
        assert_eq!(recorder.finished_moved_count, Some(report.moved_count()));
        assert_eq!(report.moved_count(), 1);
        Ok(())
    }

    #[test]
    fn reports_created_directory_before_starting() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.txt");
        fs::write(&source, "contents")?;

        let mut recorder = EventRecorder::default();
        move_files_to_target_dir(
            &temp_directory.path().join("created"),
            std::slice::from_ref(&source),
            false,
            &mut recorder,
        )?;

        assert_eq!(
            recorder.events,
            ["CreatedDirectory", "Started", "FileStarted", "Moved", "Finished"]
        );
        Ok(())
    }

    #[test]
    fn reports_finished_for_an_empty_batch() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let mut recorder = EventRecorder::default();
        let report = move_files_to_target_dir(temp_directory.path(), &[], false, &mut recorder)?;

        assert_eq!(recorder.events, ["Started", "Finished"]);
        assert_eq!(report.moved_count(), 0);
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_moves_files() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir(&input)?;
        fs::create_dir(&output)?;
        fs::write(input.join("one.txt"), "one")?;
        fs::write(input.join("two.txt"), "two")?;

        let files = vec![input.join("one.txt"), input.join("two.txt")];
        let report = move_files_to_target_dir(&output, &files, false, &mut ())?;

        assert_eq!(report.moved_count(), 2);
        assert_eq!(report.moved_files, files);
        assert_eq!(report.skipped_files, [] as [PathBuf; 0]);
        assert_eq!(report.failed_files, [] as [PathBuf; 0]);
        assert!(!input.join("one.txt").exists());
        assert!(!input.join("two.txt").exists());
        assert_eq!(fs::read_to_string(output.join("one.txt"))?, "one");
        assert_eq!(fs::read_to_string(output.join("two.txt"))?, "two");
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_skips_existing_without_overwrite() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let input = tmp.path().join("input");
        let output = tmp.path().join("output");
        fs::create_dir(&input)?;
        fs::create_dir(&output)?;
        fs::write(input.join("one.txt"), "new")?;
        fs::write(output.join("one.txt"), "old")?;

        let source = input.join("one.txt");
        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), false, &mut ())?;

        assert_eq!(report.moved_count(), 0);
        assert_eq!(report.moved_files, [] as [PathBuf; 0]);
        assert_eq!(report.skipped_files, vec![source]);
        assert_eq!(report.failed_files, [] as [PathBuf; 0]);
        assert_eq!(fs::read_to_string(input.join("one.txt"))?, "new");
        assert_eq!(fs::read_to_string(output.join("one.txt"))?, "old");
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_creates_missing_target() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.txt");
        let output = temp_directory.path().join("nested/output");
        fs::write(&source, "contents")?;

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), false, &mut ())?;

        assert_eq!(report.moved_files, vec![source.clone()]);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(output.join("source.txt"))?, "contents");
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_rejects_file_target() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let source = temp_directory.path().join("source.txt");
        let target_file = temp_directory.path().join("target");
        fs::write(&source, "source")?;
        fs::write(&target_file, "target")?;

        let error = move_files_to_target_dir(&target_file, &[source], false, &mut ())
            .expect_err("regular file should not be accepted as target directory");

        assert!(error.to_string().contains("Target path exists but is not a directory"));
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_overwrites_existing_file() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let input = temp_directory.path().join("input");
        let output = temp_directory.path().join("output");
        fs::create_dir(&input)?;
        fs::create_dir(&output)?;
        let source = input.join("one.txt");
        fs::write(&source, "new")?;
        fs::write(output.join("one.txt"), "old")?;

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), true, &mut ())?;

        assert_eq!(report.moved_files, vec![source.clone()]);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(output.join("one.txt"))?, "new");
        Ok(())
    }

    #[test]
    fn overwrite_never_deletes_a_file_already_in_the_target_directory() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let output = temp_directory.path().join("Show");
        fs::create_dir(&output)?;
        let source = output.join("Show.S01E01.mkv");
        fs::write(&source, "episode")?;

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), true, &mut ())?;

        assert_eq!(fs::read_to_string(&source)?, "episode");
        assert_eq!(report.failed_files, [] as [PathBuf; 0]);
        assert_eq!(report.skipped_files, vec![source]);
        Ok(())
    }

    #[test]
    fn overwrite_never_deletes_a_file_reached_through_a_differently_spelled_directory() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let output = temp_directory.path().join("Show");
        fs::create_dir(&output)?;
        let source = output.join("Show.S01E01.mkv");
        fs::write(&source, "episode")?;
        let alias = temp_directory.path().join(".").join("Show");

        let report = move_files_to_target_dir(&alias, std::slice::from_ref(&source), true, &mut ())?;

        assert_eq!(fs::read_to_string(&source)?, "episode");
        assert_eq!(report.failed_files, [] as [PathBuf; 0]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_file_name_that_is_not_utf8_is_sanitized() -> anyhow::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp_directory = tempfile::TempDir::new()?;
        let input = temp_directory.path().join("input");
        let output = temp_directory.path().join("output");
        fs::create_dir(&input)?;
        let source = input.join(OsStr::from_bytes(b"Show.E01\xff.mkv"));
        // File systems such as APFS reject names that are not valid UTF-8, so there is nothing to test there.
        if fs::write(&source, "episode").is_err() {
            return Ok(());
        }

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), false, &mut ())?;

        assert_eq!(report.moved_files, vec![source]);
        assert_eq!(fs::read_to_string(output.join("Show.E01.mkv"))?, "episode");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn names_that_sanitize_alike_never_overwrite_each_other() -> anyhow::Result<()> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let temp_directory = tempfile::TempDir::new()?;
        let input = temp_directory.path().join("input");
        let output = temp_directory.path().join("output");
        fs::create_dir(&input)?;
        let first = input.join(OsStr::from_bytes(b"Show.E01\xfe.mkv"));
        let second = input.join(OsStr::from_bytes(b"Show.E01\xff.mkv"));
        if fs::write(&first, "first").is_err() {
            return Ok(());
        }
        fs::write(&second, "second")?;

        let report = move_files_to_target_dir(&output, &[first.clone(), second.clone()], true, &mut ())?;

        assert_eq!(report.moved_files, vec![first]);
        assert_eq!(report.skipped_files, vec![second.clone()]);
        assert_eq!(fs::read_to_string(output.join("Show.E01.mkv"))?, "first");
        assert_eq!(fs::read_to_string(&second)?, "second");
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_reports_missing_source() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let output = temp_directory.path().join("output");
        let source = temp_directory.path().join("missing.txt");

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), false, &mut ())?;

        assert_eq!(report.moved_files, [] as [PathBuf; 0]);
        assert_eq!(report.failed_files, vec![source]);
        Ok(())
    }

    #[test]
    fn move_files_to_target_dir_reports_empty_source_path() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let output = temp_directory.path().join("output");
        let source = PathBuf::new();

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), false, &mut ())?;

        assert_eq!(report.failed_files, vec![source]);
        Ok(())
    }

    #[test]
    fn overwrite_reports_destination_removal_failure() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        let input = temp_directory.path().join("input");
        let output = temp_directory.path().join("output");
        fs::create_dir(&input)?;
        fs::create_dir(&output)?;
        let source = input.join("one.txt");
        fs::write(&source, "source")?;
        fs::create_dir(output.join("one.txt"))?;

        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), true, &mut ())?;

        assert_eq!(report.failed_files, vec![source.clone()]);
        assert!(source.exists());
        assert!(output.join("one.txt").is_dir());
        Ok(())
    }
}

#[cfg(test)]
mod test_move_single_file {
    use super::test_observer::ByteCounter;
    use super::*;
    use cli_tools_common::test_support::TempTree;

    #[test]
    fn symlink_fallback_recreates_a_directory_link_without_following_it() {
        let tree = TempTree::new();
        tree.file_with("outside/real/data.txt", "outside data");
        let Some(source) = tree.try_symlink("outside/real", "source/link") else {
            return;
        };
        tree.dir("destination");
        let destination = tree.join("destination/link");

        move_symlink_via_temp(&source, &destination).expect("the directory link should be recreated");

        assert!(fs::symlink_metadata(&destination).unwrap().file_type().is_symlink());
        assert!(fs::symlink_metadata(&source).is_err());
        assert_eq!(tree.read("outside/real/data.txt"), "outside data");
        assert_eq!(tree.read("destination/link/data.txt"), "outside data");
    }

    #[test]
    fn a_plain_rename_reports_the_moved_bytes() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let source = directory.path().join("source.txt");
        let destination = directory.path().join("destination.txt");
        std::fs::write(&source, b"content").expect("file should be written");
        let mut bar = ByteCounter::default();

        move_single_file(&source, &destination, 7, false, &mut bar).expect("the move should succeed");

        assert!(destination.is_file());
        assert!(!source.exists());
        assert_eq!(bar.bytes, 7);
    }

    #[test]
    fn a_missing_source_with_the_file_already_moved_is_treated_as_done() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let source = directory.path().join("gone.txt");
        let destination = directory.path().join("already.txt");
        std::fs::write(&destination, b"content").expect("file should be written");
        let mut bar = ByteCounter::default();

        move_single_file(&source, &destination, 7, false, &mut bar).expect("an already moved file should succeed");

        assert_eq!(bar.bytes, 7, "the observer should still account for the bytes");
    }

    #[test]
    fn a_missing_source_with_no_destination_is_an_error() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let source = directory.path().join("gone.txt");
        let destination = directory.path().join("missing.txt");

        let error = move_single_file(&source, &destination, 7, false, &mut ByteCounter::default())
            .expect_err("a missing source should fail");

        assert!(error.to_string().contains("source is gone"), "{error}");
    }

    #[test]
    fn a_missing_source_with_a_differently_sized_destination_is_an_error() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let source = directory.path().join("gone.txt");
        let destination = directory.path().join("partial.txt");
        std::fs::write(&destination, b"short").expect("file should be written");

        let error = move_single_file(&source, &destination, 7, false, &mut ByteCounter::default())
            .expect_err("a partial destination should fail");

        assert!(error.to_string().contains("source is gone"), "{error}");
    }
}
