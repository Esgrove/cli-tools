//! Duplicate deletion for video convert.
//!
//! A source file whose converted output already exists is deleted
//! when the output is a different HEVC or AV1 file with a duration close to the source.

use std::path::Path;

use super::VideoConvert;
use crate::ffmpeg::{probe_video_info, validate_audio_preserved};
use crate::helpers::{duration_difference_ratio, format_duplicate_duration_match};

/// Largest duration difference, relative to the source duration, for the output to count as a duplicate.
const MAX_DUPLICATE_DURATION_DIFFERENCE: f64 = 0.02;

/// What happened to a source file whose converted output already exists.
#[derive(Debug, Clone, PartialEq)]
pub enum DuplicateOutcome {
    /// The source was deleted.
    Deleted { duration_message: String },
    /// The source would be deleted, but this is a dry run.
    WouldDelete { duration_message: String },
    /// The source and the output are the same file, so nothing was deleted.
    RefusedSameFile,
    /// The output is not HEVC or AV1, so the source was kept.
    RefusedNotTargetCodec,
    /// The durations differ too much for the output to be a copy of the source.
    DurationMismatch {
        source_duration: f64,
        target_duration: f64,
        difference_ratio: f64,
    },
    /// The output could not be probed.
    ProbeFailed { error: String },
    /// Deleting the source failed.
    DeleteFailed { error: String },
}

impl DuplicateOutcome {
    /// Whether the source was deleted, or would be deleted in a dry run.
    #[must_use]
    pub const fn removes_source(&self) -> bool {
        matches!(self, Self::Deleted { .. } | Self::WouldDelete { .. })
    }
}

impl VideoConvert<'_> {
    /// Delete a source file whose converted output already exists,
    /// after checking that the output really is a converted copy of it.
    pub(super) fn delete_duplicate(&self, source: &Path, target: &Path, source_duration: f64) -> DuplicateOutcome {
        if cli_tools_common::paths_refer_to_same_file(source, target) {
            return DuplicateOutcome::RefusedSameFile;
        }

        let target_info = match probe_video_info(target) {
            Ok(target_info) => target_info,
            Err(error) => {
                return DuplicateOutcome::ProbeFailed {
                    error: error.to_string(),
                };
            }
        };
        if !target_info.is_target_codec() {
            return DuplicateOutcome::RefusedNotTargetCodec;
        }

        let difference_ratio = duration_difference_ratio(source_duration, target_info.duration);
        if difference_ratio > MAX_DUPLICATE_DURATION_DIFFERENCE {
            return DuplicateOutcome::DurationMismatch {
                source_duration,
                target_duration: target_info.duration,
                difference_ratio,
            };
        }

        if let Err(error) = validate_audio_preserved(source, target) {
            return DuplicateOutcome::ProbeFailed {
                error: error.to_string(),
            };
        }

        let duration_message = format_duplicate_duration_match(source_duration, target_info.duration);
        if self.options.dry_run {
            DuplicateOutcome::WouldDelete { duration_message }
        } else if let Err(error) = self.delete_file(source) {
            DuplicateOutcome::DeleteFailed {
                error: error.to_string(),
            }
        } else {
            DuplicateOutcome::Deleted { duration_message }
        }
    }
}

#[cfg(test)]
mod test_delete_duplicate {
    use super::super::test_helpers::converter;
    use super::*;
    use crate::options::ConvertOptions;

    #[test]
    fn refuses_when_source_and_target_are_the_same_file() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let path = directory.path().join("Movie.mp4");
        std::fs::write(&path, b"video").expect("Failed to write video file");
        let converter = converter(ConvertOptions::default());

        let outcome = converter.delete_duplicate(&path, &path, 60.0);

        assert_eq!(outcome, DuplicateOutcome::RefusedSameFile);
        assert!(path.is_file());
    }

    #[test]
    fn a_target_that_cannot_be_probed_keeps_the_source() {
        let directory = tempfile::tempdir().expect("Failed to create temporary directory");
        let source = directory.path().join("Movie.mkv");
        let target = directory.path().join("Movie.x265.mp4");
        std::fs::write(&source, b"video").expect("Failed to write source file");
        let converter = converter(ConvertOptions::default());

        let outcome = converter.delete_duplicate(&source, &target, 60.0);

        assert!(matches!(outcome, DuplicateOutcome::ProbeFailed { .. }));
        assert!(source.is_file());
    }

    #[test]
    fn only_deleted_outcomes_remove_the_source() {
        let duration_message = "duration match".to_string();

        assert!(
            DuplicateOutcome::Deleted {
                duration_message: duration_message.clone()
            }
            .removes_source()
        );
        assert!(DuplicateOutcome::WouldDelete { duration_message }.removes_source());
        assert!(!DuplicateOutcome::RefusedSameFile.removes_source());
        assert!(!DuplicateOutcome::RefusedNotTargetCodec.removes_source());
        assert!(
            !DuplicateOutcome::DeleteFailed {
                error: "denied".to_string()
            }
            .removes_source()
        );
    }
}
