//! Removing replaced files for video convert.
//!
//! Deletes or trashes originals and subtitle sidecars once their output is in place,
//! and swaps a muxed output over its input using a hidden backup sibling when both share the same path.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::VideoConvert;
use crate::observer::DeleteMethod;
use crate::types::SubtitleFile;

impl VideoConvert<'_> {
    /// Delete a file, or move it to the trash unless permanent deletion is configured.
    pub(super) fn delete_file(&self, path: &Path) -> Result<()> {
        // Use direct delete if configured or if on a Windows network drive (trash doesn't work there)
        if self.options.delete || cli_tools_common::is_network_path(path) {
            self.observer.deleting(path, DeleteMethod::Delete);
            std::fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            self.observer.deleting(path, DeleteMethod::Trash);
            cli_tools_common::move_to_trash(path).context("Failed to move original file to trash")?;
        }
        Ok(())
    }

    /// Delete a file and report a failure to the observer instead of returning it.
    pub(super) fn delete_file_reporting_failure(&self, path: &Path) {
        if let Err(error) = self.delete_file(path) {
            self.observer.delete_failed(path, &error);
        }
    }

    /// Delete the subtitle sidecars that were muxed into an output.
    pub(super) fn delete_subtitle_files(&self, subtitle_files: &[SubtitleFile]) {
        for subtitle_file in subtitle_files {
            for path in subtitle_file.paths_to_delete() {
                self.delete_file_reporting_failure(path);
            }
        }
    }

    /// Move a staged output into `final_output` while keeping `input` backed up under a hidden sibling path.
    pub(super) fn replace_input_with_output(
        &self,
        input: &Path,
        final_output: &Path,
        staged_output: &Path,
    ) -> Result<()> {
        let backup = hidden_backup_path(input)?;
        std::fs::rename(input, &backup).context("Failed to move original file aside before replacement")?;

        if let Err(error) = std::fs::rename(staged_output, final_output) {
            if let Err(restore_error) = std::fs::rename(&backup, input) {
                anyhow::bail!(
                    "Failed to move muxed subtitle output into place: {error}, failed to restore original file: {restore_error}"
                );
            }
            return Err(error).context("Failed to move muxed subtitle output into place");
        }

        self.delete_file_reporting_failure(&backup);
        Ok(())
    }
}

/// Reserve a unique hidden backup path beside `input` so concurrent directory scans ignore it.
fn hidden_backup_path(input: &Path) -> Result<PathBuf> {
    let parent = input
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let extension = cli_tools_common::path_to_file_extension_string(input);
    let suffix = if extension.is_empty() {
        String::new()
    } else {
        format!(".{extension}")
    };
    let placeholder = tempfile::Builder::new()
        .prefix(".vconvert-backup-")
        .suffix(&suffix)
        .tempfile_in(parent)
        .context("Failed to reserve hidden backup path")?;
    let path = placeholder.path().to_path_buf();
    drop(placeholder);
    Ok(path)
}
