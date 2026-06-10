//! Reliable file moving helpers for `dirmove`.
//!
//! Moves use a fast `rename` first,
//! then fall back to chunked copy-verify-delete for cross-device moves.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use indicatif::{ProgressBar, ProgressStyle};

use crate::{path_to_filename_string, print_error, print_yellow};

/// Size of chunks used when copying files across devices.
pub const COPY_BUFFER_SIZE: usize = 1024 * 1024;

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

impl MoveReport {
    /// Return the number of files moved successfully.
    #[must_use]
    pub const fn moved_count(&self) -> usize {
        self.moved_files.len()
    }
}

/// Move files to the target directory, creating it if needed.
///
/// Uses `rename` first for fast same-device moves, then falls back to
/// copy-verify-delete for cross-device moves.
///
/// # Errors
///
/// Returns an error if the target path cannot be created or is not a directory.
pub fn move_files_to_target_dir(
    dir_path: &Path,
    files: &[PathBuf],
    overwrite: bool,
    verbose: bool,
    hide_progress: bool,
) -> anyhow::Result<MoveReport> {
    if !dir_path.exists() {
        fs::create_dir_all(dir_path)?;
        println!("  Created directory: {}", path_to_filename_string(dir_path));
    } else if !dir_path.is_dir() {
        anyhow::bail!("Target path exists but is not a directory: {}", dir_path.display());
    }

    let file_sizes = collect_file_sizes(files);
    let total_size = file_sizes.values().sum();
    let progress_bar = create_move_progress_bar(total_size, hide_progress);

    let mut report = MoveReport::default();
    for (index, file_path) in files.iter().enumerate() {
        let file_name = path_to_filename_string(file_path);
        if file_name.is_empty() {
            progress_bar.suspend(|| {
                print_error(&format!("Could not get file name for path: {}", file_path.display()));
            });
            report.failed_files.push(file_path.clone());
            continue;
        }

        progress_bar.set_message(format!("[{}/{}] {file_name}", index + 1, files.len()));
        let expected_size = file_sizes.get(file_path).copied().unwrap_or(0);
        let new_path = dir_path.join(&file_name);

        if new_path.exists() && !overwrite {
            progress_bar.suspend(|| {
                print_yellow(&format!("Skipping existing file: {}", new_path.display()));
            });
            progress_bar.inc(expected_size);
            report.skipped_files.push(file_path.clone());
            continue;
        }

        if overwrite
            && new_path.exists()
            && let Err(error) = fs::remove_file(&new_path)
        {
            progress_bar.suspend(|| {
                print_error(&format!(
                    "Failed to remove existing file {}: {error}",
                    new_path.display()
                ));
            });
            progress_bar.inc(expected_size);
            report.failed_files.push(file_path.clone());
            continue;
        }

        match move_single_file(file_path, &new_path, expected_size, &progress_bar) {
            Ok(()) => {
                if verbose {
                    progress_bar.suspend(|| {
                        println!("  Moved: {file_name}");
                    });
                }
                report.moved_files.push(file_path.clone());
            }
            Err(error) => {
                progress_bar.suspend(|| {
                    print_error(&format!("Failed to move {}: {error}", file_path.display()));
                });
                if expected_size > 0 {
                    progress_bar.inc(expected_size);
                }
                report.failed_files.push(file_path.clone());
            }
        }
    }
    progress_bar.finish_and_clear();
    println!("  Moved {} files", report.moved_count());

    Ok(report)
}

/// Move a single file, falling back to copy-verify-delete when rename cannot cross devices.
fn move_single_file(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    progress_bar: &ProgressBar,
) -> anyhow::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => {
            progress_bar.inc(expected_size);
            return Ok(());
        }
        Err(rename_error) => {
            if !source.exists() {
                if destination.exists()
                    && let Ok(metadata) = fs::metadata(destination)
                    && metadata.len() == expected_size
                {
                    progress_bar.inc(expected_size);
                    return Ok(());
                }
                anyhow::bail!(
                    "rename of {} -> {} reported an error and source is gone. Original error: {rename_error}",
                    source.display(),
                    destination.display()
                );
            }

            if !is_cross_device_error(&rename_error) {
                progress_bar.suspend(|| {
                    print_yellow(&format!(
                        "Rename failed for {} -> {} ({rename_error}), falling back to copy",
                        source.display(),
                        destination.display()
                    ));
                });
            }
        }
    }

    copy_file_with_progress(source, destination, expected_size, progress_bar)?;

    let copied_size = fs::metadata(destination)?.len();
    if copied_size != expected_size {
        let _ = fs::remove_file(destination);
        anyhow::bail!(
            "Size verification failed for {}: expected {} bytes, got {} bytes. Original file preserved.",
            source.display(),
            expected_size,
            copied_size
        );
    }

    fs::remove_file(source).map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "File copied successfully but failed to delete original: {}. You may have a duplicate.",
            source.display()
        ))
    })?;

    Ok(())
}

/// Copy a file in chunks while updating the progress bar.
fn copy_file_with_progress(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    progress_bar: &ProgressBar,
) -> anyhow::Result<()> {
    let result = copy_file_inner(source, destination, expected_size, progress_bar);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

/// Inner copy loop. File handles are dropped before cleanup can run.
fn copy_file_inner(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    progress_bar: &ProgressBar,
) -> anyhow::Result<()> {
    let mut source_file = File::open(source)?;
    let mut destination_file = File::create(destination)?;
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    let mut bytes_copied = 0;

    loop {
        let bytes_read = source_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        destination_file.write_all(&buffer[..bytes_read])?;
        bytes_copied += bytes_read as u64;
        progress_bar.inc(bytes_read as u64);
    }

    destination_file.flush()?;

    if bytes_copied != expected_size {
        anyhow::bail!(
            "Incomplete copy for {}: expected {} bytes, copied {} bytes. Original file preserved.",
            source.display(),
            expected_size,
            bytes_copied
        );
    }

    Ok(())
}

/// Check if an I/O error indicates a cross-device move attempt.
fn is_cross_device_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(17) || error.raw_os_error() == Some(18)
}

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

/// Create a progress bar for moving file bytes.
fn create_move_progress_bar(length: u64, hide_progress: bool) -> ProgressBar {
    if hide_progress {
        return ProgressBar::hidden();
    }

    let progress_bar = ProgressBar::new(length);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta}) {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("█▓░"),
    );
    progress_bar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_file_with_progress_preserves_contents() -> anyhow::Result<()> {
        let tmp = tempfile::TempDir::new()?;
        let source = tmp.path().join("source.bin");
        let destination = tmp.path().join("destination.bin");
        let contents = vec![42; COPY_BUFFER_SIZE + 13];
        fs::write(&source, &contents)?;
        let progress_bar = ProgressBar::hidden();

        copy_file_with_progress(&source, &destination, contents.len() as u64, &progress_bar)?;

        assert_eq!(fs::read(destination)?, contents);
        assert!(source.exists());
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
        let report = move_files_to_target_dir(&output, &files, false, false, true)?;

        assert_eq!(report.moved_count(), 2);
        assert_eq!(report.moved_files, files);
        assert!(report.skipped_files.is_empty());
        assert!(report.failed_files.is_empty());
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
        let report = move_files_to_target_dir(&output, std::slice::from_ref(&source), false, false, true)?;

        assert_eq!(report.moved_count(), 0);
        assert!(report.moved_files.is_empty());
        assert_eq!(report.skipped_files, vec![source]);
        assert!(report.failed_files.is_empty());
        assert_eq!(fs::read_to_string(input.join("one.txt"))?, "new");
        assert_eq!(fs::read_to_string(output.join("one.txt"))?, "old");
        Ok(())
    }
}
