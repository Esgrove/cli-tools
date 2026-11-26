//! Codec label renames for video convert.
//!
//! Renames files that already use a target codec so their names carry the codec label,
//! never replacing an existing file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use cli_tools_common::{OverwritePolicy, RenameOutcome};

use super::VideoConvert;
use crate::observer::FilePosition;
use crate::types::ProcessableFile;

impl VideoConvert<'_> {
    /// Process all files that need renaming. Returns the number of files successfully renamed.
    pub(super) fn process_renames(&self, files: &[ProcessableFile], abort_flag: &AtomicBool) -> usize {
        let start = Instant::now();
        let total = files.len();
        let mut renamed_count = 0;

        for (index, file) in files.iter().enumerate() {
            if abort_flag.load(Ordering::SeqCst) {
                break;
            }
            let position = FilePosition {
                index: index + 1,
                total,
            };
            self.observer
                .rename_started(position, &file.file.path, &file.output_path, self.options.dry_run);
            if abort_flag.load(Ordering::SeqCst) {
                break;
            }

            if self.options.dry_run {
                renamed_count += 1;
                continue;
            }
            match cli_tools_common::safe_rename(&file.file.path, &file.output_path, OverwritePolicy::Skip) {
                Ok(RenameOutcome::SkippedExisting) => self.observer.rename_skipped_existing(&file.output_path),
                Ok(_) => renamed_count += 1,
                Err(error) => self.observer.rename_failed(&file.file.path, &error.into()),
            }
        }

        self.observer.renames_finished(renamed_count, total, start.elapsed());
        renamed_count
    }
}

#[cfg(test)]
mod test_process_renames {
    use super::super::test_helpers::{converter, processable, video_info};
    use super::*;
    use crate::options::ConvertOptions;
    use crate::types::VideoFile;

    #[test]
    fn renames_move_files_to_their_output_names() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "hevc");
        let output = file.output_path.clone();
        let converter = converter(ConvertOptions::default());

        let renamed = converter.process_renames(&[file], &AtomicBool::new(false));

        assert_eq!(renamed, 1);
        assert!(!path.exists());
        assert!(output.is_file());
    }

    #[test]
    fn dryrun_renames_count_without_moving_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let converter = converter(ConvertOptions {
            dry_run: true,
            ..ConvertOptions::default()
        });

        let renamed = converter.process_renames(&[processable(&path, "hevc")], &AtomicBool::new(false));

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
        let converter = converter(ConvertOptions::default());

        let renamed = converter.process_renames(&files, &AtomicBool::new(false));

        assert_eq!(renamed, 1);
        assert_eq!(std::fs::read(&output).expect("output should exist"), b"FIRST");
        assert_eq!(std::fs::read(&second).expect("second file should remain"), b"SECOND");
    }

    #[test]
    fn a_failed_rename_is_not_counted() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let missing = directory.path().join("Missing.mp4");
        let converter = converter(ConvertOptions::default());

        assert_eq!(
            converter.process_renames(&[processable(&missing, "hevc")], &AtomicBool::new(false)),
            0
        );
    }
}
