//! Safe file handling shared by the tools that rename, move, and copy files.
//!
//! Provides same file detection, case only rename detection,
//! a collision aware rename that never replaces a different file unless asked to,
//! destination reservations for detecting collisions within one batch,
//! atomic file writes, and a recursive directory copy that refuses to copy a directory into itself.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::{append_extension_to_path, into_nfc};

/// What to do when a rename target already exists and is a different file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// Leave both files in place and report the rename as skipped.
    Skip,
    /// Replace the existing target file. Existing directories are never replaced.
    Overwrite,
    /// Return an `AlreadyExists` error.
    Error,
}

/// Result of a [`safe_rename`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    /// Source and target are the same entry with the same name, so nothing was done.
    Unchanged,
    /// The source was renamed to the target.
    Renamed,
    /// The source was renamed to a different spelling of its own name through a temporary name,
    /// for example a capitalization only change on a case-insensitive file system.
    CaseRenamed,
    /// The target is a different existing file and the policy was [`OverwritePolicy::Skip`].
    SkippedExisting,
}

/// Result of a [`safe_move_file`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveResult {
    /// Source and destination were the same entry with the same name.
    Unchanged,
    /// Source was moved via same-filesystem rename.
    Renamed,
    /// Source was copied across devices, verified, atomically published, and removed.
    CopiedAndRemoved,
    /// Destination already existed and the policy was [`OverwritePolicy::Skip`].
    SkippedExisting,
}

/// Planned destination paths for one batch of renames or moves.
///
/// Paths are compared case-insensitively and normalized to Unicode NFC.
/// Names that differ only by case or canonical Unicode spelling count as collisions on every file system.
#[derive(Debug, Default, Clone)]
pub struct TargetReservations {
    reserved: HashSet<String>,
}

impl TargetReservations {
    /// Create an empty set of reservations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a destination path.
    ///
    /// Returns `false` when the path or an equivalent spelling is already reserved.
    pub fn reserve(&mut self, path: &Path) -> bool {
        self.reserved.insert(Self::key(path))
    }

    /// Check whether a path or an equivalent spelling is already reserved.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.reserved.contains(&Self::key(path))
    }

    /// Comparison key for a path, normalized before applying lowercase mappings.
    fn key(path: &Path) -> String {
        into_nfc(path.to_string_lossy().into_owned()).to_lowercase()
    }
}

/// Check whether two paths refer to the same filesystem entry.
///
/// Symbolic links are followed, so a link and its target count as the same file.
/// Paths that do not exist never refer to the same file unless they are identical.
#[must_use]
pub fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    paths_refer_to_same_file_by_metadata(left, right) || paths_refer_to_same_file_by_canonical_path(left, right)
}

/// Check whether the target only changes the capitalization of the source file name.
///
/// Both paths must share the same parent directory,
/// and the file names must differ but be equal after Unicode lowercasing.
/// Names that are not valid UTF-8 never count as a case only change.
#[must_use]
pub fn is_case_only_change(source: &Path, target: &Path) -> bool {
    if source.parent() != target.parent() {
        return false;
    }
    let (Some(source_name), Some(target_name)) = (
        source.file_name().and_then(|name| name.to_str()),
        target.file_name().and_then(|name| name.to_str()),
    ) else {
        return false;
    };

    source_name != target_name && source_name.to_lowercase() == target_name.to_lowercase()
}

/// Check whether a directory lives on a case-sensitive file system.
///
/// Creates and removes a small probe file inside the directory.
///
/// # Errors
/// Returns an error if the probe file cannot be created or removed.
pub fn directory_is_case_sensitive(directory: &Path) -> io::Result<bool> {
    let mut counter: u32 = 0;
    loop {
        let lower_name = if counter == 0 {
            ".case-probe.tmp".to_string()
        } else {
            format!(".case-probe.{counter}.tmp")
        };
        let upper_name = lower_name.to_uppercase();
        let lower_path = directory.join(&lower_name);
        let upper_path = directory.join(&upper_name);
        if fs::symlink_metadata(&upper_path).is_ok() {
            counter = counter.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "Exhausted case sensitivity probe names")
            })?;
            continue;
        }
        match fs::OpenOptions::new().write(true).create_new(true).open(&lower_path) {
            Ok(probe_file) => {
                drop(probe_file);
                let case_sensitive = fs::symlink_metadata(&upper_path).is_err();
                fs::remove_file(&lower_path)?;
                return Ok(case_sensitive);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                counter = counter.checked_add(1).ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Rename `source` to `target` without silently replacing a different file.
///
/// - Identical paths return [`RenameOutcome::Unchanged`] after verifying the source exists.
/// - When the target already exists and is the source itself under a different spelling,
///   such as a capitalization only change on a case-insensitive file system,
///   the rename goes through a temporary sibling name so the new spelling is applied.
/// - When the target is a different existing entry, the `policy` decides.
///   An existing directory is never replaced.
///
/// The existence check and the rename are separate system calls,
/// so a file created at the target in between can still be replaced.
///
/// # Errors
/// Returns an error if `source` does not exist or a rename fails,
/// or if the target is a different existing entry and the policy is [`OverwritePolicy::Error`],
/// or the policy is [`OverwritePolicy::Overwrite`] and the target is a directory.
pub fn safe_rename(source: &Path, target: &Path, policy: OverwritePolicy) -> io::Result<RenameOutcome> {
    let _source_metadata = fs::symlink_metadata(source)?;

    if source == target {
        return Ok(RenameOutcome::Unchanged);
    }

    if let Ok(target_metadata) = fs::symlink_metadata(target) {
        if paths_refer_to_same_entry(source, target) {
            if source.file_name() == target.file_name() {
                return Ok(RenameOutcome::Unchanged);
            }
            rename_through_temporary(source, target)?;
            return Ok(RenameOutcome::CaseRenamed);
        }

        match policy {
            OverwritePolicy::Skip => return Ok(RenameOutcome::SkippedExisting),
            OverwritePolicy::Error => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Target already exists: {}", target.display()),
                ));
            }
            OverwritePolicy::Overwrite => {
                if target_metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::IsADirectory,
                        format!("Refusing to replace existing directory: {}", target.display()),
                    ));
                }
                // On POSIX, `rename` between two distinct hard links to the same inode is a no-op
                // and leaves the source entry behind. Removing the source completes the move
                // while keeping the inode alive through the target entry.
                if same_entry_metadata_without_following(source, target).is_some_and(|(is_directory, _)| !is_directory)
                {
                    fs::remove_file(source)?;
                    return Ok(RenameOutcome::Renamed);
                }
            }
        }
    }

    fs::rename(source, target)?;
    Ok(RenameOutcome::Renamed)
}

/// Callback invoked after a cross-device copy completes to verify `copied` matches `source`.
pub type CopyVerifier<'a> = &'a dyn Fn(&Path, &Path) -> io::Result<()>;

/// Move a file to `destination`, falling back to verified copy-and-remove only on `CrossesDevices`.
///
/// Non-cross-device errors such as `PermissionDenied`, `NotFound`, or `AlreadyExists` fail fast
/// instead of triggering an unnecessary multi-gigabyte copy.
/// When a cross-device copy is required, contents are staged to an `O_EXCL` sibling file,
/// validated with `verify_copy` when provided, published respecting `policy`, and only then is `source` removed.
///
/// # Errors
/// Returns an error if moving, copying, verifying, or removing the source file fails.
pub fn safe_move_file(
    source: &Path,
    destination: &Path,
    policy: OverwritePolicy,
    verify_copy: Option<CopyVerifier<'_>>,
) -> io::Result<MoveResult> {
    match safe_rename(source, destination, policy) {
        Ok(RenameOutcome::Unchanged) => Ok(MoveResult::Unchanged),
        Ok(RenameOutcome::SkippedExisting) => Ok(MoveResult::SkippedExisting),
        Ok(RenameOutcome::Renamed | RenameOutcome::CaseRenamed) => Ok(MoveResult::Renamed),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            move_file_across_devices(source, destination, policy, verify_copy)
        }
        Err(error) => Err(error),
    }
}

/// Copy `source` to a sibling temp file next to `destination`, verify, publish, and remove `source`.
fn move_file_across_devices(
    source: &Path,
    destination: &Path,
    policy: OverwritePolicy,
    verify_copy: Option<CopyVerifier<'_>>,
) -> io::Result<MoveResult> {
    if let Ok(target_meta) = fs::symlink_metadata(destination) {
        match policy {
            OverwritePolicy::Skip => return Ok(MoveResult::SkippedExisting),
            OverwritePolicy::Error => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Target already exists: {}", destination.display()),
                ));
            }
            OverwritePolicy::Overwrite => {
                if target_meta.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::IsADirectory,
                        format!("Refusing to replace existing directory: {}", destination.display()),
                    ));
                }
            }
        }
    }

    let source_meta = fs::symlink_metadata(source)?;
    if source_meta.file_type().is_symlink() {
        let (temp_file, temp_path) = create_unique_sibling_file(destination, "move-tmp", None)?;
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        let mut visited = HashSet::new();
        copy_symlink(source, &temp_path, &mut visited)?;
        if let Err(error) = safe_rename(&temp_path, destination, policy) {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        fs::remove_file(source)?;
        return Ok(MoveResult::CopiedAndRemoved);
    }

    let (temp_file, temp_path) = create_unique_sibling_file(destination, "move-tmp", None)?;
    drop(temp_file);
    let copy_result = (|| -> io::Result<MoveResult> {
        copy_file_with_times(source, &temp_path)?;
        if let Some(verify) = verify_copy {
            verify(source, &temp_path)?;
        }
        match safe_rename(&temp_path, destination, policy)? {
            RenameOutcome::SkippedExisting => {
                let _ = fs::remove_file(&temp_path);
                Ok(MoveResult::SkippedExisting)
            }
            RenameOutcome::Renamed | RenameOutcome::CaseRenamed | RenameOutcome::Unchanged => {
                fs::remove_file(source)?;
                Ok(MoveResult::CopiedAndRemoved)
            }
        }
    })();
    if copy_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    copy_result
}

/// Recursively remove empty subdirectories beneath `root` without following symlinks or deleting `root`.
///
/// Returns the number of removed empty directories.
///
/// # Errors
/// Returns an error if a directory
/// that appeared empty cannot be removed for a reason other than becoming non-empty concurrently.
pub fn remove_empty_directories(root: &Path) -> io::Result<usize> {
    let mut removed = 0usize;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .contents_first(true)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        if crate::is_directory_empty(entry.path()) {
            match fs::remove_dir(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}

/// Replace the contents of a file without ever leaving it partly written.
///
/// The new contents go to a temporary file next to the target, which takes the permissions of the target
/// and is then renamed over it, so an interrupted write leaves the original file as it was.
/// A symbolic link is resolved first, so the file it points to is updated and the link stays a link.
///
/// # Errors
/// Returns an error if the temporary file cannot be written or renamed over the target.
pub fn write_atomically(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    use std::io::Write;

    let target = if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        dunce::canonicalize(path).or_else(|_| resolve_symlink_target(path))?
    } else {
        path.to_path_buf()
    };
    let target_permissions = fs::metadata(&target).ok().map(|metadata| metadata.permissions());

    let (mut temp_file, temporary) = create_unique_sibling_file(&target, "tmp", target_permissions.as_ref())?;
    let result = (|| {
        temp_file.write_all(contents.as_ref())?;
        temp_file.sync_data()?;
        drop(temp_file);
        if let Some(ref permissions) = target_permissions {
            #[cfg(windows)]
            if permissions.readonly() {
                let mut writable = permissions.clone();
                #[allow(clippy::permissions_set_readonly_false)]
                writable.set_readonly(false);
                let _ = fs::set_permissions(&target, writable);
            }
            fs::set_permissions(&temporary, permissions.clone())?;
        }
        fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        if let Ok(metadata) = fs::metadata(&temporary) {
            let mut permissions = metadata.permissions();
            if permissions.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(&temporary, permissions);
            }
        }
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Resolve the destination path of a symlink when canonicalization fails, such as for a dangling link.
fn resolve_symlink_target(link_path: &Path) -> io::Result<PathBuf> {
    let raw_target = fs::read_link(link_path)?;
    if raw_target.is_absolute() {
        Ok(raw_target)
    } else {
        Ok(link_path.parent().unwrap_or_else(|| Path::new(".")).join(raw_target))
    }
}

/// Create a brand-new sibling temporary file with `O_EXCL` semantics so concurrent writers cannot collide.
fn create_unique_sibling_file(
    target: &Path,
    extension: &str,
    target_permissions: Option<&fs::Permissions>,
) -> io::Result<(fs::File, PathBuf)> {
    let mut counter: u32 = 0;
    loop {
        let candidate = if counter == 0 {
            append_extension_to_path(target, extension)
        } else {
            append_extension_to_path(target, format!("{counter}.{extension}"))
        };
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(permissions) = target_permissions {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            // Ensure the file starts at least as restrictive as the target while remaining writable during staging.
            options.mode(permissions.mode() | 0o200);
        }
        #[cfg(not(unix))]
        let _ = target_permissions;
        match options.open(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                counter = counter.checked_add(1).ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Check that the existing parent directories of `target` beneath `root` are real directories.
///
/// The caller supplies a trusted `root`, which is not checked.
/// Missing directories are allowed, and the target entry itself is not inspected.
/// This check does not protect against concurrent changes to the path.
///
/// # Errors
/// Returns an error if `target` is not a descendant of `root`, contains non-normal components below it,
/// has a symlink or other non-directory parent beneath it, or a parent cannot be inspected.
pub fn check_destination_parents(root: &Path, target: &Path) -> io::Result<()> {
    let invalid_target = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Destination {} is not a normal path below {}",
                target.display(),
                root.display()
            ),
        )
    };
    let mut components = target.strip_prefix(root).map_err(|_| invalid_target())?.components();
    if !matches!(components.next_back(), Some(Component::Normal(_))) {
        return Err(invalid_target());
    }

    let mut directory = root.to_path_buf();
    for component in components {
        let Component::Normal(name) = component else {
            return Err(invalid_target());
        };
        directory.push(name);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("Destination parent is not a real directory: {}", directory.display()),
                ));
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

/// Recursively copy a directory and its contents, keeping file modification times.
///
/// Symbolic links are recreated as links on Unix instead of being followed.
///
/// # Errors
/// Returns an `InvalidInput` error without copying anything when the target is the source
/// or lies inside it, since the copy would never finish.
/// Otherwise returns an error if directory creation, reading, or file copying fails.
pub fn copy_dir_recursive(source: &Path, target: &Path) -> io::Result<()> {
    let canonical_source = dunce::canonicalize(source)?;
    let canonical_target = canonicalize_existing_prefix(target)?;
    if canonical_target.starts_with(&canonical_source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Cannot copy directory {} into itself at {}",
                source.display(),
                target.display()
            ),
        ));
    }

    let mut visited = HashSet::new();
    visited.insert(canonical_source);
    copy_dir_contents(source, target, &mut visited)
}

/// Copy directory contents after the caller has checked that the target is outside the source.
fn copy_dir_contents(source: &Path, target: &Path, visited: &mut HashSet<PathBuf>) -> io::Result<()> {
    fs::create_dir_all(target)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_contents(&source_path, &target_path, visited)?;
        } else if file_type.is_symlink() {
            copy_symlink(&source_path, &target_path, visited)?;
        } else if file_type.is_file() {
            copy_file_with_times(&source_path, &target_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Cannot copy special file: {}", source_path.display()),
            ));
        }
    }

    Ok(())
}

/// Copy one file, keeping its permissions and modification time.
///
/// The modification time is applied before read-only permissions so copying read-only files succeeds.
/// Any newly created target file is removed if the copy fails, leaving pre-existing targets intact.
///
/// # Errors
/// Returns `InvalidInput` if the source and target refer to the same file.
/// Also returns an error if the copy fails or the metadata cannot be read or applied.
pub fn copy_file_with_times(source: &Path, target: &Path) -> io::Result<u64> {
    let mut source_file = fs::File::open(source)?;
    let metadata = source_file.metadata()?;
    let modified = metadata.modified()?;
    let permissions = metadata.permissions();

    let target_existed = fs::symlink_metadata(target).is_ok();
    if target_existed
        && let Ok(target_meta) = fs::metadata(target)
        && target_meta.permissions().readonly()
        && !paths_refer_to_same_file(source, target)
    {
        let mut writable = target_meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        fs::set_permissions(target, writable)?;
    }

    // Compare the open files before truncating, so a path change cannot invalidate the check.
    let mut target_file = fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(target)?;
    #[cfg(unix)]
    let same_file = same_unix_file_identity(&metadata, &target_file.metadata()?);
    #[cfg(windows)]
    let same_file = {
        use std::os::windows::io::AsRawHandle;

        let (source_id, _) = windows_file_information(source_file.as_raw_handle())?;
        let (target_id, _) = windows_file_information(target_file.as_raw_handle())?;
        source_id != (0, 0, 0) && source_id == target_id
    };
    #[cfg(not(any(unix, windows)))]
    let same_file = paths_refer_to_same_file(source, target);
    if same_file {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Cannot copy {} onto the same file at {}",
                source.display(),
                target.display()
            ),
        ));
    }
    target_file.set_len(0)?;

    let copy_result = (|| -> io::Result<u64> {
        let bytes = io::copy(&mut source_file, &mut target_file)?;
        target_file.set_modified(modified)?;
        Ok(bytes)
    })();

    drop(target_file);
    match copy_result {
        Ok(bytes) => {
            if let Err(error) = fs::set_permissions(target, permissions) {
                if !target_existed {
                    let _ = fs::remove_file(target);
                }
                return Err(error);
            }
            Ok(bytes)
        }
        Err(error) => {
            if !target_existed {
                let _ = fs::remove_file(target);
            }
            Err(error)
        }
    }
}

/// Recreate a symbolic link at the target.
#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path, _visited: &mut HashSet<PathBuf>) -> io::Result<()> {
    let link_target = fs::read_link(source)?;
    if fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink()) {
        fs::remove_file(target)?;
    }
    std::os::unix::fs::symlink(link_target, target)
}

/// Copy the target a symbolic link points to, since creating links needs extra privileges on Windows.
#[cfg(not(unix))]
fn copy_symlink(source: &Path, target: &Path, visited: &mut HashSet<PathBuf>) -> io::Result<()> {
    if fs::metadata(source)?.is_dir() {
        let canonical = dunce::canonicalize(source)?;
        if !visited.insert(canonical.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Detected symlink directory cycle at {}", source.display()),
            ));
        }
        let result = copy_dir_contents(source, target, visited);
        visited.remove(&canonical);
        result
    } else {
        copy_file_with_times(source, target).map(|_| ())
    }
}

/// Canonicalize the longest existing prefix of a path and apply the remaining components lexically.
fn canonicalize_existing_prefix(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let components: Vec<Component<'_>> = absolute.components().collect();
    for prefix_len in (1..=components.len()).rev() {
        let prefix: PathBuf = components.get(..prefix_len).unwrap_or_default().iter().collect();
        if let Ok(mut canonical) = dunce::canonicalize(&prefix) {
            for component in components.get(prefix_len..).unwrap_or_default() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        canonical.pop();
                    }
                    other => canonical.push(other.as_os_str()),
                }
            }
            return Ok(canonical);
        }
    }

    let mut normalized = PathBuf::new();
    for component in components {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Rename through a unique temporary sibling of the target, for spelling only changes.
fn rename_through_temporary(source: &Path, target: &Path) -> io::Result<()> {
    let temporary = unique_sibling_path(target, "rename-tmp");
    fs::rename(source, &temporary)?;
    if let Err(error) = fs::rename(&temporary, target) {
        // Put the source back under its original name so nothing is left behind under the temporary name.
        let _ = fs::rename(&temporary, source);
        return Err(error);
    }
    Ok(())
}

/// Return a path next to `path` with the given extension appended that does not exist yet.
///
/// Useful for temporary files that are renamed over the final path once complete.
#[must_use]
pub fn unique_sibling_path(path: &Path, extension: &str) -> PathBuf {
    let mut candidate = append_extension_to_path(path, extension);
    let mut counter = 1_u32;
    while fs::symlink_metadata(&candidate).is_ok() {
        candidate = append_extension_to_path(path, format!("{counter}.{extension}"));
        let Some(next) = counter.checked_add(1) else {
            break;
        };
        counter = next;
    }
    candidate
}

/// Check whether two existing paths name the same directory entry without following a final symbolic link.
///
/// Two hard links to the same inode in different directories
/// or under different names in the same directory are separate directory entries,
/// so this returns `false` for them.
/// Two spellings of the same directory entry on a case-insensitive
/// or Unicode-normalizing file system return `true`.
#[must_use]
pub fn paths_refer_to_same_entry(left: &Path, right: &Path) -> bool {
    if left == right {
        return fs::symlink_metadata(left).is_ok();
    }

    let (Some(left_parent), Some(right_parent)) = (parent_dir_for_comparison(left), parent_dir_for_comparison(right))
    else {
        return false;
    };
    if !paths_refer_to_same_file(left_parent, right_parent) {
        return false;
    }

    let Some((is_directory, link_count)) = same_entry_metadata_without_following(left, right) else {
        return false;
    };

    let (Some(left_name), Some(right_name)) = (left.file_name(), right.file_name()) else {
        return false;
    };
    if left_name == right_name {
        return true;
    }

    if !is_directory && link_count > 1 && directory_has_both_entries(left_parent, left_name, right_name) {
        return false;
    }

    true
}

/// Parent directory used for comparing whether two paths sit in the same directory.
fn parent_dir_for_comparison(path: &Path) -> Option<&Path> {
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        Some(Path::new("."))
    } else {
        Some(parent)
    }
}

/// Return whether two distinct directory entries in `directory` match `left_name` and `right_name`.
///
/// Matching checks both exact bytes and Unicode-normalized case-folded spellings
/// so that hard links in the same directory are never collapsed on case-insensitive file systems.
fn directory_has_both_entries(directory: &Path, left_name: &std::ffi::OsStr, right_name: &std::ffi::OsStr) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    let left_key = into_nfc(left_name.to_string_lossy().into_owned()).to_lowercase();
    let right_key = into_nfc(right_name.to_string_lossy().into_owned()).to_lowercase();
    let mut matching_entries = 0usize;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let key = into_nfc(name.to_string_lossy().into_owned()).to_lowercase();
        let matches = name == left_name || name == right_name || key == left_key || key == right_key;
        if matches {
            matching_entries += 1;
            if matching_entries >= 2 {
                return true;
            }
        }
    }
    false
}

/// Compare entry identity without following a final symbolic link.
///
/// Returns `Some((is_directory, link_count))` when both paths name the same underlying object.
#[cfg(unix)]
fn same_entry_metadata_without_following(left: &Path, right: &Path) -> Option<(bool, u64)> {
    use std::os::unix::fs::MetadataExt;

    let left_meta = fs::symlink_metadata(left).ok()?;
    let right_meta = fs::symlink_metadata(right).ok()?;
    if same_unix_file_identity(&left_meta, &right_meta) {
        Some((left_meta.is_dir(), left_meta.nlink()))
    } else {
        None
    }
}

/// Compare entry identity without following a final symbolic link on Windows.
///
/// Returns `Some((is_directory, link_count))` when both paths name the same underlying object.
#[cfg(windows)]
fn same_entry_metadata_without_following(left: &Path, right: &Path) -> Option<(bool, u64)> {
    let (left_id, left_links) = windows_file_identity(left, false)?;
    let (right_id, _) = windows_file_identity(right, false)?;
    if left_id == right_id {
        let is_directory = fs::symlink_metadata(left).ok()?.is_dir();
        Some((is_directory, u64::from(left_links)))
    } else {
        None
    }
}

/// Fallback when neither Unix nor Windows file identity metadata is available.
#[cfg(not(any(unix, windows)))]
const fn same_entry_metadata_without_following(_left: &Path, _right: &Path) -> Option<(bool, u64)> {
    None
}

/// Compare device and inode numbers for two sets of Unix file metadata.
#[cfg(unix)]
fn same_unix_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Compare device and inode numbers, following symbolic links.
#[cfg(unix)]
fn paths_refer_to_same_file_by_metadata(left: &Path, right: &Path) -> bool {
    match (fs::metadata(left), fs::metadata(right)) {
        (Ok(left), Ok(right)) => same_unix_file_identity(&left, &right),
        _ => false,
    }
}

/// Compare Windows volume serial number and file index, following symbolic links.
#[cfg(windows)]
fn paths_refer_to_same_file_by_metadata(left: &Path, right: &Path) -> bool {
    let Some((left_id, _)) = windows_file_identity(left, true) else {
        return false;
    };
    let Some((right_id, _)) = windows_file_identity(right, true) else {
        return false;
    };

    left_id == right_id
}

/// Read the Windows volume serial, file index, and link count for `path`.
#[cfg(windows)]
fn windows_file_identity(path: &Path, follow_symlinks: bool) -> Option<((u32, u32, u32), u32)> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let flags = if follow_symlinks {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
    };

    // SAFETY: The path buffer is null terminated and lives for the duration of the calls.
    // The handle returned by CreateFileW is closed before returning when it is valid.
    #[allow(unsafe_code)]
    unsafe {
        let handle = CreateFileW(
            path_wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }

        let information = windows_file_information(handle);
        _ = CloseHandle(handle);
        information.ok().filter(|(id, _)| *id != (0, 0, 0))
    }
}

/// Read the Windows file identity and link count from an open handle.
#[cfg(windows)]
fn windows_file_information(handle: std::os::windows::io::RawHandle) -> io::Result<((u32, u32, u32), u32)> {
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};

    // SAFETY: Callers keep the handle open for the call, and the output points to a writable structure.
    #[allow(unsafe_code)]
    let information = unsafe {
        let mut information = std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>();
        if GetFileInformationByHandle(handle, &raw mut information) == 0 {
            return Err(io::Error::last_os_error());
        }
        information
    };
    Ok((
        (
            information.dwVolumeSerialNumber,
            information.nFileIndexHigh,
            information.nFileIndexLow,
        ),
        information.nNumberOfLinks,
    ))
}

/// Fallback when neither Unix nor Windows file identity metadata is available.
#[cfg(not(any(unix, windows)))]
const fn paths_refer_to_same_file_by_metadata(_left: &Path, _right: &Path) -> bool {
    false
}

/// Compare two paths after canonicalizing both of them.
fn paths_refer_to_same_file_by_canonical_path(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod test_file_handling_bugfixes {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn safe_rename_returns_not_found_when_source_is_missing_and_target_exists() {
        let directory = TempDir::new().unwrap();
        let missing = directory.path().join("missing.txt");
        let existing = directory.path().join("existing.txt");
        fs::write(&existing, "keep").unwrap();

        let err =
            safe_rename(&missing, &existing, OverwritePolicy::Skip).expect_err("missing source should return NotFound");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_to_string(&existing).unwrap(), "keep");
    }

    #[test]
    fn safe_rename_skip_does_not_replace_hard_link_with_case_variant_source_path() {
        let directory = TempDir::new().unwrap();
        if directory_is_case_sensitive(directory.path()).unwrap() {
            return;
        }
        let first = directory.path().join("Photo.JPG");
        let second = directory.path().join("other.jpg");
        fs::write(&first, "shared").unwrap();
        fs::hard_link(&first, &second).unwrap();

        let lowercase_first = directory.path().join("photo.jpg");
        let outcome = safe_rename(&lowercase_first, &second, OverwritePolicy::Skip).unwrap();
        assert_eq!(outcome, RenameOutcome::SkippedExisting);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn directory_is_case_sensitive_ignores_preexisting_uppercase_probe() {
        let directory = TempDir::new().unwrap();
        let expected = directory_is_case_sensitive(directory.path()).unwrap();
        fs::write(directory.path().join(".CASE-PROBE.TMP"), b"").unwrap();

        let actual = directory_is_case_sensitive(directory.path()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn safe_move_file_moves_and_respects_skip_policy() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        fs::write(&src, "payload").unwrap();

        let outcome = safe_move_file(&src, &dst, OverwritePolicy::Skip, None).unwrap();
        assert_eq!(outcome, MoveResult::Renamed);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");

        let other = dir.path().join("other.txt");
        fs::write(&other, "other").unwrap();
        let skipped = safe_move_file(&other, &dst, OverwritePolicy::Skip, None).unwrap();
        assert_eq!(skipped, MoveResult::SkippedExisting);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "payload");
    }

    #[test]
    fn remove_empty_directories_prunes_nested_empties_only() {
        let dir = TempDir::new().unwrap();
        let empty_nested = dir.path().join("a").join("b");
        let kept = dir.path().join("kept");
        fs::create_dir_all(&empty_nested).unwrap();
        fs::create_dir_all(&kept).unwrap();
        fs::write(kept.join("file.txt"), b"x").unwrap();

        let removed = remove_empty_directories(dir.path()).unwrap();
        assert_eq!(removed, 2);
        assert!(!dir.path().join("a").exists());
        assert!(kept.join("file.txt").exists());
    }
}

#[cfg(test)]
mod test_case_only_change {
    use super::*;

    #[test]
    fn detects_capitalization_change_in_same_directory() {
        assert!(is_case_only_change(
            Path::new("dir/Photo.JPG"),
            Path::new("dir/Photo.jpg")
        ));
        assert!(is_case_only_change(
            Path::new("dir/ärrä.txt"),
            Path::new("dir/ÄRRÄ.txt")
        ));
    }

    #[test]
    fn rejects_identical_names_other_directories_and_different_names() {
        assert!(!is_case_only_change(Path::new("dir/a.txt"), Path::new("dir/a.txt")));
        assert!(!is_case_only_change(Path::new("one/a.txt"), Path::new("two/A.txt")));
        assert!(!is_case_only_change(Path::new("dir/a.txt"), Path::new("dir/b.txt")));
    }
}

#[cfg(test)]
mod test_safe_rename {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn renames_to_free_target() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a.txt");
        let target = directory.path().join("b.txt");
        fs::write(&source, "a").expect("write");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("rename");

        assert_eq!(outcome, RenameOutcome::Renamed);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target).expect("read"), "a");
    }

    #[test]
    fn identical_paths_are_unchanged() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a.txt");
        fs::write(&source, "a").expect("write");

        let outcome = safe_rename(&source, &source, OverwritePolicy::Overwrite).expect("rename");

        assert_eq!(outcome, RenameOutcome::Unchanged);
        assert_eq!(fs::read_to_string(&source).expect("read"), "a");
    }

    #[test]
    fn case_only_rename_applies_new_spelling() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("Photo.JPG");
        let target = directory.path().join("Photo.jpg");
        fs::write(&source, "photo").expect("write");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("rename");

        assert!(matches!(outcome, RenameOutcome::Renamed | RenameOutcome::CaseRenamed));
        let names: Vec<String> = fs::read_dir(directory.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["Photo.jpg".to_string()]);
        assert_eq!(fs::read_to_string(&target).expect("read"), "photo");
    }

    #[test]
    fn different_file_differing_by_case_is_not_replaced() {
        let directory = TempDir::new().expect("temp directory");
        if !directory_is_case_sensitive(directory.path()).expect("probe") {
            return;
        }
        let source = directory.path().join("Photo.JPG");
        let target = directory.path().join("Photo.jpg");
        fs::write(&source, "upper").expect("write");
        fs::write(&target, "original").expect("write");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("rename");

        assert_eq!(outcome, RenameOutcome::SkippedExisting);
        assert_eq!(fs::read_to_string(&source).expect("read"), "upper");
        assert_eq!(fs::read_to_string(&target).expect("read"), "original");
    }

    #[test]
    fn skip_policy_keeps_both_files() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a.txt");
        let target = directory.path().join("b.txt");
        fs::write(&source, "a").expect("write");
        fs::write(&target, "b").expect("write");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("rename");

        assert_eq!(outcome, RenameOutcome::SkippedExisting);
        assert_eq!(fs::read_to_string(&source).expect("read"), "a");
        assert_eq!(fs::read_to_string(&target).expect("read"), "b");
    }

    #[test]
    fn error_policy_returns_already_exists() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a.txt");
        let target = directory.path().join("b.txt");
        fs::write(&source, "a").expect("write");
        fs::write(&target, "b").expect("write");

        let error = safe_rename(&source, &target, OverwritePolicy::Error).expect_err("should fail");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&target).expect("read"), "b");
    }

    #[test]
    fn overwrite_policy_replaces_different_file() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a.txt");
        let target = directory.path().join("b.txt");
        fs::write(&source, "a").expect("write");
        fs::write(&target, "b").expect("write");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Overwrite).expect("rename");

        assert_eq!(outcome, RenameOutcome::Renamed);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target).expect("read"), "a");
    }

    #[test]
    fn overwrite_policy_never_replaces_directory() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a");
        let target = directory.path().join("b");
        fs::create_dir(&source).expect("create");
        fs::create_dir(&target).expect("create");
        fs::write(target.join("keep.txt"), "keep").expect("write");

        let error = safe_rename(&source, &target, OverwritePolicy::Overwrite).expect_err("should fail");

        assert_eq!(error.kind(), io::ErrorKind::IsADirectory);
        assert_eq!(fs::read_to_string(target.join("keep.txt")).expect("read"), "keep");
    }

    #[test]
    fn hard_link_to_source_respects_skip_and_error_policies() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("a.txt");
        let target = directory.path().join("b.txt");
        fs::write(&source, "a").expect("write");
        fs::hard_link(&source, &target).expect("hard link");

        assert!(paths_refer_to_same_file(&source, &target));

        let skip_outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("skip rename");
        assert_eq!(skip_outcome, RenameOutcome::SkippedExisting);
        assert!(source.exists(), "source hard link must stay under Skip");
        assert_eq!(fs::read_to_string(&target).expect("read"), "a");

        let error = safe_rename(&source, &target, OverwritePolicy::Error).expect_err("error rename");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(source.exists(), "source hard link must stay under Error");
    }

    #[test]
    fn cross_directory_hard_link_respects_overwrite_policy() {
        let directory = TempDir::new().expect("temp directory");
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        fs::create_dir(&first_dir).expect("create first");
        fs::create_dir(&second_dir).expect("create second");
        let source = first_dir.join("a.txt");
        let target = second_dir.join("a.txt");
        fs::write(&source, "a").expect("write");
        fs::hard_link(&source, &target).expect("hard link");

        let skip_outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("skip rename");
        assert_eq!(skip_outcome, RenameOutcome::SkippedExisting);
        assert!(source.exists());

        let error = safe_rename(&source, &target, OverwritePolicy::Error).expect_err("error rename");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        let overwrite_outcome = safe_rename(&source, &target, OverwritePolicy::Overwrite).expect("overwrite rename");
        assert_eq!(overwrite_outcome, RenameOutcome::Renamed);
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target).expect("read"), "a");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_pointing_to_target_respects_skip_policy() {
        let directory = TempDir::new().expect("temp directory");
        let target = directory.path().join("a.txt");
        let source = directory.path().join("link.txt");
        fs::write(&target, "important").expect("write");
        std::os::unix::fs::symlink(&target, &source).expect("symlink");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("skip rename");
        assert_eq!(outcome, RenameOutcome::SkippedExisting);
        assert!(
            fs::symlink_metadata(&source)
                .expect("source meta")
                .file_type()
                .is_symlink()
        );
        assert!(
            !fs::symlink_metadata(&target)
                .expect("target meta")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).expect("read"), "important");
    }

    #[test]
    fn case_only_rename_works_with_external_hard_link() {
        let directory = TempDir::new().expect("temp directory");
        let other_dir = directory.path().join("other");
        fs::create_dir(&other_dir).expect("create other");
        let source = directory.path().join("Photo.JPG");
        let target = directory.path().join("Photo.jpg");
        let external_link = other_dir.join("Photo.JPG");
        fs::write(&source, "photo").expect("write");
        fs::hard_link(&source, &external_link).expect("hard link");

        let outcome = safe_rename(&source, &target, OverwritePolicy::Skip).expect("rename");
        assert!(matches!(outcome, RenameOutcome::Renamed | RenameOutcome::CaseRenamed));
        assert_eq!(fs::read_to_string(&target).expect("read"), "photo");
        assert_eq!(fs::read_to_string(&external_link).expect("read external"), "photo");
    }

    #[test]
    fn case_only_directory_rename_handles_trailing_slash() {
        let directory = TempDir::new().expect("temp directory");
        let source_dir = directory.path().join("Show");
        fs::create_dir(&source_dir).expect("create dir");
        fs::write(source_dir.join("episode.mkv"), "video").expect("write");

        let source_with_slash = PathBuf::from(format!("{}/", source_dir.display()));
        let target_with_slash = PathBuf::from(format!("{}/", directory.path().join("show").display()));

        let outcome = safe_rename(&source_with_slash, &target_with_slash, OverwritePolicy::Skip)
            .expect("case-only rename with trailing slash should succeed");
        assert!(matches!(outcome, RenameOutcome::Renamed | RenameOutcome::CaseRenamed));
        assert!(directory.path().join("show").join("episode.mkv").exists());
    }
}

#[cfg(test)]
mod test_target_reservations {
    use super::*;

    #[test]
    fn detects_repeated_and_case_variant_targets() {
        let mut reservations = TargetReservations::new();

        assert!(reservations.reserve(Path::new("dir/movie.x265.mp4")));
        assert!(!reservations.reserve(Path::new("dir/movie.x265.mp4")));
        assert!(!reservations.reserve(Path::new("dir/Movie.X265.mp4")));
        assert!(reservations.contains(Path::new("DIR/movie.x265.mp4")));
        assert!(reservations.reserve(Path::new("dir/other.mp4")));
    }

    #[test]
    fn detects_canonically_equivalent_targets() {
        let mut reservations = TargetReservations::new();
        let composed = Path::new("café/résumé.txt");
        let decomposed = Path::new("CAFE\u{301}/RE\u{301}SUME\u{301}.txt");

        assert!(reservations.reserve(composed));
        assert!(reservations.contains(decomposed));
        assert!(!reservations.reserve(decomposed));
        assert!(reservations.reserve(Path::new("café/resume.txt")));
    }
}

#[cfg(test)]
mod test_copy_file_with_times {
    use super::*;

    use crate::test_support::TempTree;

    #[test]
    fn refuses_to_copy_a_file_onto_itself() {
        let tree = TempTree::new();
        let source = tree.file_with("file.txt", "original");

        let result = copy_file_with_times(&source, &source);

        assert_eq!(tree.read("file.txt"), "original");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn refuses_to_copy_onto_a_hard_link_to_the_source() {
        let tree = TempTree::new();
        let source = tree.file_with("file.txt", "original");
        let target = tree.join("hardlink.txt");
        fs::hard_link(&source, &target).unwrap();

        let result = copy_file_with_times(&source, &target);

        assert_eq!(tree.read("file.txt"), "original");
        assert_eq!(tree.read("hardlink.txt"), "original");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_copy_through_a_symlink_to_the_source() {
        let tree = TempTree::new();
        let source = tree.file_with("file.txt", "original");
        let target = tree.symlink("file.txt", "symlink.txt");

        let result = copy_file_with_times(&source, &target);

        assert_eq!(tree.read("file.txt"), "original");
        assert!(fs::symlink_metadata(&target).unwrap().is_symlink());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn truncates_a_distinct_existing_target() {
        let tree = TempTree::new();
        let source = tree.file_with("source.txt", "new");
        let target = tree.file_with("target.txt", "old and longer");

        assert_eq!(copy_file_with_times(&source, &target).unwrap(), 3);
        assert_eq!(tree.read("source.txt"), "new");
        assert_eq!(tree.read("target.txt"), "new");
    }
}

#[cfg(test)]
mod test_copy_dir_recursive {
    use super::*;

    use crate::test_support::TempTree;
    use tempfile::TempDir;

    #[test]
    fn refuses_to_copy_onto_a_nested_hard_link_to_the_source() {
        let tree = TempTree::new();
        let source_file = tree.file_with("source/nested/file.txt", "original");
        let target_file = tree.dir("target/nested").join("file.txt");
        fs::hard_link(&source_file, &target_file).unwrap();

        let result = copy_dir_recursive(&tree.join("source"), &tree.join("target"));

        assert_eq!(tree.read("source/nested/file.txt"), "original");
        assert_eq!(tree.read("target/nested/file.txt"), "original");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn copies_nested_contents_and_modification_times() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("source");
        fs::create_dir_all(source.join("nested")).expect("create");
        fs::write(source.join("nested").join("file.txt"), "content").expect("write");
        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(source.join("nested").join("file.txt"))
            .expect("open")
            .set_modified(old_time)
            .expect("set time");
        let target = directory.path().join("target");

        copy_dir_recursive(&source, &target).expect("copy");

        let copied = target.join("nested").join("file.txt");
        assert_eq!(fs::read_to_string(&copied).expect("read"), "content");
        assert_eq!(
            fs::metadata(&copied).expect("metadata").modified().expect("time"),
            old_time
        );
    }

    #[test]
    fn copies_read_only_files_and_keeps_modification_times() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("source");
        fs::create_dir_all(&source).expect("create");
        let source_file = source.join("readonly.txt");
        fs::write(&source_file, "readonly content").expect("write");
        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(&source_file)
            .expect("open")
            .set_modified(old_time)
            .expect("set time");
        let mut permissions = fs::metadata(&source_file).expect("meta").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&source_file, permissions).expect("set readonly");

        let target = directory.path().join("target");
        copy_dir_recursive(&source, &target).expect("copy read-only file");

        let copied = target.join("readonly.txt");
        assert_eq!(fs::read_to_string(&copied).expect("read"), "readonly content");
        let copied_meta = fs::metadata(&copied).expect("metadata");
        assert!(copied_meta.permissions().readonly());
        assert_eq!(copied_meta.modified().expect("time"), old_time);

        // Restore write permission so TempDir cleanup succeeds on all platforms.
        let mut writable = copied_meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        let _ = fs::set_permissions(&copied, writable.clone());
        let _ = fs::set_permissions(&source_file, writable);
    }

    #[test]
    fn refuses_to_copy_into_own_child() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("Show");
        fs::create_dir_all(source.join("Show")).expect("create");
        fs::write(source.join("episode.mkv"), "video").expect("write");

        let error = copy_dir_recursive(&source, &source.join("Show").join("Show")).expect_err("should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!source.join("Show").join("Show").exists());
    }

    #[test]
    fn refuses_to_copy_onto_itself() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("Show");
        fs::create_dir_all(&source).expect("create");

        let error = copy_dir_recursive(&source, &source).expect_err("should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn refuses_to_copy_into_itself_through_parent_dir_components() {
        let directory = TempDir::new().expect("temp directory");
        let source = directory.path().join("Show");
        fs::create_dir_all(&source).expect("create");
        fs::write(source.join("episode.mkv"), "video").expect("write");

        let via_missing_sibling = directory
            .path()
            .join("missing_dir")
            .join("..")
            .join("Show")
            .join("child");
        let error = copy_dir_recursive(&source, &via_missing_sibling).expect_err("should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!source.join("child").exists());

        let via_missing_child = source.join("missing_child").join("..");
        let error = copy_dir_recursive(&source, &via_missing_child).expect_err("should fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[cfg(test)]
mod test_write_atomically {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn replaces_contents_and_leaves_no_temporary_file() {
        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("notes.md");
        fs::write(&path, "old").expect("write");

        write_atomically(&path, "new").expect("write atomically");

        assert_eq!(fs::read_to_string(&path).expect("read"), "new");
        assert_eq!(fs::read_dir(directory.path()).expect("read dir").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_permissions_and_symbolic_links() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().expect("temp directory");
        let path = directory.path().join("script.sh");
        let link = directory.path().join("link.sh");
        fs::write(&path, "old").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).expect("set mode");
        std::os::unix::fs::symlink(&path, &link).expect("symlink");

        write_atomically(&link, "new").expect("write atomically");

        assert!(fs::symlink_metadata(&link).expect("metadata").file_type().is_symlink());
        assert_eq!(fs::read_to_string(&path).expect("read"), "new");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o750
        );
    }
}
