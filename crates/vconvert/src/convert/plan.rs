//! Processing plan for video convert.
//!
//! Holds the remuxes, subtitle muxes, and conversions of one run,
//! drops the operations the run skips, applies the file count limit,
//! and lists the database entries for the files still to process.

use std::path::Path;

use crate::database::PendingAction;
use crate::types::{ProcessableFile, VideoInfo};

/// Files to process in one run, in processing order.
#[derive(Debug, Default)]
pub(super) struct ProcessingPlan {
    /// Files to remux to MP4, processed first.
    pub(super) remuxes: Vec<ProcessableFile>,
    /// Files to mux external subtitles into, processed after the remuxes.
    pub(super) subtitle_muxes: Vec<ProcessableFile>,
    /// Files to convert to HEVC, processed last.
    pub(super) conversions: Vec<ProcessableFile>,
}

impl ProcessingPlan {
    /// Plan from the three operation lists.
    pub(super) const fn new(
        remuxes: Vec<ProcessableFile>,
        subtitle_muxes: Vec<ProcessableFile>,
        conversions: Vec<ProcessableFile>,
    ) -> Self {
        Self {
            remuxes,
            subtitle_muxes,
            conversions,
        }
    }

    /// Database entries for every planned file, with the action each one needs.
    pub(super) fn pending_entries(&self) -> Vec<(&Path, &str, &VideoInfo, PendingAction)> {
        let actions = [
            (&self.remuxes, PendingAction::Remux),
            (&self.subtitle_muxes, PendingAction::SubtitleMux),
            (&self.conversions, PendingAction::Convert),
        ];
        actions
            .into_iter()
            .flat_map(|(files, action)| {
                files.iter().map(move |file| {
                    (
                        file.file.path.as_path(),
                        file.file.extension.as_str(),
                        &file.info,
                        action,
                    )
                })
            })
            .collect()
    }

    /// Drop the operations the run skips.
    ///
    /// Skipping remuxes also skips subtitle muxes, since both copy the video stream.
    pub(super) fn skip(&mut self, skip_remux: bool, skip_convert: bool) {
        if skip_remux {
            self.remuxes.clear();
            self.subtitle_muxes.clear();
        }
        if skip_convert {
            self.conversions.clear();
        }
    }

    /// Keep at most `count` files in total, filling the limit in processing order.
    pub(super) fn truncate_to(&mut self, count: usize) {
        let mut remaining = count;
        for files in [&mut self.remuxes, &mut self.subtitle_muxes, &mut self.conversions] {
            let limit = files.len().min(remaining);
            files.truncate(limit);
            remaining -= limit;
        }
    }

    /// Number of planned files.
    pub(super) const fn total(&self) -> usize {
        self.remuxes.len() + self.subtitle_muxes.len() + self.conversions.len()
    }
}

#[cfg(test)]
mod test_processing_plan {
    use std::path::PathBuf;

    use super::super::test_helpers::processable;
    use super::*;

    fn files(prefix: &str, count: usize) -> Vec<ProcessableFile> {
        (0..count)
            .map(|index| processable(&PathBuf::from(format!("{prefix}{index}.mkv")), "h264"))
            .collect()
    }

    fn plan(remuxes: usize, subtitle_muxes: usize, conversions: usize) -> ProcessingPlan {
        ProcessingPlan::new(
            files("remux", remuxes),
            files("subtitle", subtitle_muxes),
            files("convert", conversions),
        )
    }

    #[test]
    fn truncation_fills_the_limit_in_processing_order() {
        let mut plan = plan(2, 2, 3);

        plan.truncate_to(3);

        assert_eq!(plan.remuxes.len(), 2);
        assert_eq!(plan.subtitle_muxes.len(), 1);
        assert_eq!(plan.conversions.len(), 0);
        assert_eq!(plan.total(), 3);
    }

    #[test]
    fn truncation_above_the_total_keeps_everything() {
        let mut plan = plan(1, 1, 1);

        plan.truncate_to(10);

        assert_eq!(plan.total(), 3);
    }

    #[test]
    fn skipped_operations_do_not_use_the_limit() {
        let mut plan = plan(2, 1, 2);

        plan.skip(true, false);
        plan.truncate_to(2);

        assert_eq!(plan.remuxes.len(), 0);
        assert_eq!(plan.subtitle_muxes.len(), 0);
        assert_eq!(plan.conversions.len(), 2);
    }

    #[test]
    fn skipping_conversions_keeps_remuxes() {
        let mut plan = plan(1, 1, 2);

        plan.skip(false, true);

        assert_eq!(plan.total(), 2);
        assert!(plan.conversions.is_empty());
    }

    #[test]
    fn pending_entries_carry_the_action_of_each_list() {
        let plan = plan(1, 1, 1);

        let actions: Vec<PendingAction> = plan.pending_entries().into_iter().map(|entry| entry.3).collect();

        assert_eq!(
            actions,
            vec![PendingAction::Remux, PendingAction::SubtitleMux, PendingAction::Convert]
        );
    }
}
