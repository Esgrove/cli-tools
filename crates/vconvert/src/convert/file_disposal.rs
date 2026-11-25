//! Removing replaced files for video convert.
//!
//! Deletes or trashes originals and subtitle sidecars once their output is in place,
//! and swaps a muxed output over its input when both share the same path.

use std::path::Path;

use anyhow::{Context, Result};
use cli_tools_common::print_error;

use super::VideoConvert;
use crate::helpers::backup_output_path;
use crate::types::SubtitleFile;

impl VideoConvert {
    pub(super) fn delete_file(&self, path: &Path) -> Result<()> {
        // Use direct delete if configured or if on a Windows network drive (trash doesn't work there)
        if self.config.delete || cli_tools_common::is_network_path(path) {
            println!("Deleting: {}", path.display());
            std::fs::remove_file(path).context("Failed to delete original file")?;
        } else {
            println!("Trashing: {}", path.display());
            trash::delete(path).context("Failed to move original file to trash")?;
        }
        Ok(())
    }

    pub(super) fn delete_subtitle_files(&self, subtitle_files: &[SubtitleFile]) {
        for subtitle_file in subtitle_files {
            for path in subtitle_file.paths_to_delete() {
                if let Err(error) = self.delete_file(path) {
                    print_error!("Failed to delete subtitle file {}: {error}", path.display());
                }
            }
        }
    }

    pub(super) fn replace_input_with_output(&self, input: &Path, output: &Path) -> Result<()> {
        let backup = backup_output_path(input);
        std::fs::rename(input, &backup).context("Failed to move original file aside before replacement")?;

        if let Err(error) = std::fs::rename(output, input) {
            if let Err(restore_error) = std::fs::rename(&backup, input) {
                anyhow::bail!(
                    "Failed to move muxed subtitle output into place: {error}, failed to restore original file: {restore_error}"
                );
            }
            return Err(error).context("Failed to move muxed subtitle output into place");
        }

        if let Err(error) = self.delete_file(&backup) {
            print_error!("Failed to delete replaced original file {}: {error}", backup.display());
        }
        Ok(())
    }
}
