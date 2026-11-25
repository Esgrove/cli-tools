//! Codec label renames for video convert.
//!
//! Renames files that already use a target codec so their names carry the codec label,
//! never replacing an existing file.

use std::time::Instant;

use cli_tools_common::{OverwritePolicy, RenameOutcome, print_error, print_yellow};
use colored::Colorize;

use super::VideoConvert;
use crate::types::ProcessableFile;

impl VideoConvert {
    /// Process all files that need renaming. Returns the number of files successfully renamed.
    pub(super) fn process_renames(&self, files: &[ProcessableFile]) -> usize {
        let start = Instant::now();
        let total = files.len();
        let num_digits = total.checked_ilog10().map_or(1, |d| d as usize + 1);
        let mut renamed_count = 0;

        for (index, file) in files.iter().enumerate() {
            let file_index = format!("[{:>width$}/{total}]", index + 1, width = num_digits);

            if self.config.dryrun {
                println!("{}", format!("{file_index} [DRYRUN] Rename:").bold().purple());
                cli_tools_common::show_diff(
                    &cli_tools_common::path_to_string_relative(&file.file.path),
                    &cli_tools_common::path_to_string_relative(&file.output_path),
                );
                renamed_count += 1;
            } else {
                println!("{}", format!("{file_index} Rename:").bold().purple());
                cli_tools_common::show_diff(
                    &cli_tools_common::path_to_string_relative(&file.file.path),
                    &cli_tools_common::path_to_string_relative(&file.output_path),
                );
                match cli_tools_common::safe_rename(&file.file.path, &file.output_path, OverwritePolicy::Skip) {
                    Ok(RenameOutcome::SkippedExisting) => {
                        print_yellow!("Skipping rename, file already exists: {}", file.output_path.display());
                    }
                    Ok(_) => renamed_count += 1,
                    Err(e) => print_error!("Failed to rename {}: {e}", file.file.path.display()),
                }
            }
        }

        self.log_renames(renamed_count, total, start.elapsed());
        renamed_count
    }
}

#[cfg(test)]
mod test_process_renames {
    use super::super::test_helpers::{converter, processable, video_info};
    use super::*;
    use crate::config::Config;
    use crate::types::VideoFile;

    #[test]
    fn renames_move_files_to_their_output_names() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "hevc");
        let output = file.output_path.clone();
        let (_log, converter) = converter(Config::default());

        let renamed = converter.process_renames(&[file]);

        assert_eq!(renamed, 1);
        assert!(!path.exists());
        assert!(output.is_file());
    }

    #[test]
    fn dryrun_renames_count_without_moving_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let (_log, converter) = converter(Config {
            dryrun: true,
            ..Config::default()
        });

        let renamed = converter.process_renames(&[processable(&path, "hevc")]);

        assert_eq!(renamed, 1);
        assert!(path.is_file());
    }

    #[test]
    fn a_rename_never_replaces_an_existing_file() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let first = directory.path().join("show.x265.mkv");
        let second = directory.path().join("show.av1.mkv");
        let output = directory.path().join("show.mkv");
        std::fs::write(&first, b"FIRST").expect("Failed to write video file");
        std::fs::write(&second, b"SECOND").expect("Failed to write video file");
        let files = [&first, &second].map(|path| {
            ProcessableFile::new(
                VideoFile::new(path, 1024),
                video_info("h264"),
                output.clone(),
                Vec::new(),
            )
        });
        let (_log, converter) = converter(Config::default());

        let renamed = converter.process_renames(&files);

        assert_eq!(renamed, 1);
        assert_eq!(std::fs::read(&output).expect("output should exist"), b"FIRST");
        assert_eq!(std::fs::read(&second).expect("second file should remain"), b"SECOND");
    }

    #[test]
    fn a_failed_rename_is_not_counted() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let missing = directory.path().join("Missing.mp4");
        let (_log, converter) = converter(Config::default());

        assert_eq!(converter.process_renames(&[processable(&missing, "hevc")]), 0);
    }
}
