//! Codec label renames for video convert.
//!
//! Renames files that already use a target codec so their names carry the codec label,
//! never replacing an existing file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use cli_tools_common::{OverwritePolicy, RenameOutcome};
use cli_tools_db::locks::{AcquireOutcome, ActiveLockStore};

use super::VideoConvert;
use crate::database::Database;
use crate::observer::FilePosition;
use crate::types::ProcessableFile;

impl VideoConvert<'_> {
    /// Process the renames, updating cached paths in `database` and honoring `overwrite`.
    pub(super) fn process_renames(
        &self,
        files: &[ProcessableFile],
        abort_flag: &AtomicBool,
        database: &mut Database,
        lock_store: Option<&ActiveLockStore>,
    ) -> (usize, usize) {
        let start = Instant::now();
        let total = files.len();
        let mut renamed_count = 0;
        let mut failed_count = 0;
        let policy = if self.options.overwrite {
            OverwritePolicy::Overwrite
        } else {
            OverwritePolicy::Skip
        };

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

            let _guard = if let Some(store) = lock_store {
                match store.try_acquire(&[file.file.path.clone(), file.output_path.clone()], "rename") {
                    Ok(AcquireOutcome::Acquired(guard)) => Some(guard),
                    Ok(AcquireOutcome::AlreadyLocked(holder)) => {
                        self.observer.file_locked(&file.file.path, &holder.tool, holder.pid);
                        continue;
                    }
                    Err(error) => {
                        failed_count += 1;
                        self.observer.rename_failed(&file.file.path, &error);
                        continue;
                    }
                }
            } else {
                None
            };

            match cli_tools_common::safe_rename(&file.file.path, &file.output_path, policy) {
                Ok(RenameOutcome::SkippedExisting) => self.observer.rename_skipped_existing(&file.output_path),
                Ok(_) => {
                    renamed_count += 1;
                    self.observer.file_renamed(&file.file.path, &file.output_path);
                    let _ = database.move_cached_path(&file.file.path, &file.output_path);
                }
                Err(error) => {
                    failed_count += 1;
                    self.observer.rename_failed(&file.file.path, &error.into());
                }
            }
        }

        self.observer.renames_finished(renamed_count, total, start.elapsed());
        (renamed_count, failed_count)
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
        let mut database = Database::open_in_memory().expect("open db");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let file = processable(&path, "hevc");
        let output = file.output_path.clone();
        let converter = converter(ConvertOptions::default());

        let renamed = converter.process_renames(&[file], &AtomicBool::new(false), &mut database, None);

        assert_eq!(renamed, (1, 0));
        assert!(!path.exists());
        assert!(output.is_file());
    }

    #[test]
    fn dryrun_renames_count_without_moving_files() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("open db");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let converter = converter(ConvertOptions {
            dry_run: true,
            ..ConvertOptions::default()
        });

        let renamed = converter.process_renames(
            &[processable(&path, "hevc")],
            &AtomicBool::new(false),
            &mut database,
            None,
        );

        assert_eq!(renamed, (1, 0));
        assert!(path.is_file());
    }

    #[test]
    fn a_rename_never_replaces_an_existing_file_without_overwrite() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("open db");
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

        let renamed = converter.process_renames(&files, &AtomicBool::new(false), &mut database, None);

        assert_eq!(renamed, (1, 0));
        assert_eq!(std::fs::read(&output).expect("output should exist"), b"FIRST");
        assert_eq!(std::fs::read(&second).expect("second file should remain"), b"SECOND");
    }

    #[test]
    fn a_failed_rename_is_counted_separately() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let mut database = Database::open_in_memory().expect("open db");
        let missing = directory.path().join("Missing.mp4");
        let converter = converter(ConvertOptions::default());

        assert_eq!(
            converter.process_renames(
                &[processable(&missing, "hevc")],
                &AtomicBool::new(false),
                &mut database,
                None
            ),
            (0, 1)
        );
    }
}

#[cfg(test)]
mod test_rename_events {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::super::test_helpers::processable;
    use super::*;
    use crate::observer::ConvertObserver;
    use crate::options::ConvertOptions;

    /// Observer that records completed renames.
    #[derive(Default)]
    struct RenameRecorder {
        renamed: Mutex<Vec<(PathBuf, PathBuf)>>,
    }

    impl ConvertObserver for RenameRecorder {
        fn file_renamed(&self, from: &Path, to: &Path) {
            self.renamed
                .lock()
                .unwrap()
                .push((from.to_path_buf(), to.to_path_buf()));
        }
    }

    #[test]
    fn completed_renames_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let mut database = Database::open_in_memory().unwrap();
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").unwrap();
        let file = processable(&path, "hevc");
        let output = file.output_path.clone();
        let recorder = RenameRecorder::default();
        let converter = VideoConvert::new(ConvertOptions::default(), &recorder);

        converter.process_renames(&[file], &AtomicBool::new(false), &mut database, None);

        assert_eq!(*recorder.renamed.lock().unwrap(), vec![(path, output)]);
    }

    #[test]
    fn dry_run_and_skipped_renames_are_not_reported() {
        let directory = tempfile::tempdir().unwrap();
        let mut database = Database::open_in_memory().unwrap();
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").unwrap();
        let file = processable(&path, "hevc");
        std::fs::write(&file.output_path, b"existing").unwrap();
        let recorder = RenameRecorder::default();

        VideoConvert::new(
            ConvertOptions {
                dry_run: true,
                ..ConvertOptions::default()
            },
            &recorder,
        )
        .process_renames(
            std::slice::from_ref(&file),
            &AtomicBool::new(false),
            &mut database,
            None,
        );
        VideoConvert::new(ConvertOptions::default(), &recorder).process_renames(
            &[file],
            &AtomicBool::new(false),
            &mut database,
            None,
        );

        assert_eq!(*recorder.renamed.lock().unwrap(), Vec::new());
    }
}
