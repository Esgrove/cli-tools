//! Removing replaced files for video convert.
//!
//! Deletes or trashes originals and subtitle sidecars once their output is in place,
//! swaps a muxed output over its input when both share the same path,
//! and removes the partial outputs of failed ffmpeg runs.

use std::path::Path;

use anyhow::{Context, Result};

use super::VideoConvert;
use crate::helpers::{backup_output_path, remove_partial_output};
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
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
    }

    /// Delete a file and report a failure to the observer instead of returning it.
    pub(super) fn delete_file_reporting_failure(&self, path: &Path) {
        if let Err(error) = self.delete_file(path) {
            self.observer.delete_failed(path, &error);
        }
    }

    /// Remove a partial ffmpeg output, reporting a failure to the observer.
    pub(super) fn remove_partial_output(&self, output: &Path) {
        if let Err(error) = remove_partial_output(output) {
            self.observer.partial_output_removal_failed(output, &error);
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

    /// Move a staged output into `final_output` while keeping `input` backed up until the new file is in place.
    pub(super) fn replace_input_with_output(
        &self,
        input: &Path,
        final_output: &Path,
        staged_output: &Path,
    ) -> Result<()> {
        let backup = backup_output_path(input);
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
