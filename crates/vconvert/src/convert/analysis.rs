//! File analysis for video convert.
//!
//! Classifies the gathered files in three phases.
//! Files with a valid scan cache entry are classified from the cache,
//! the rest are probed with ffprobe in parallel and their results written back to the cache,
//! and the combined results are collected into the operation lists and analysis statistics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use super::{DuplicateOutcome, VideoConvert};
use crate::classification::{ClassificationRequest, probe_and_classify, resolve_output_collisions};
use crate::database::{Database, file_modified_time_ns};
use crate::stats::{AnalysisStats, RunStats};
use crate::types::{
    AnalysisFilter, AnalysisOutput, AnalysisResult, ProcessableFileSliceExt, SkipReason, SubtitleFile, VideoFile,
    VideoInfo, VideoInfoCache,
};

impl VideoConvert<'_> {
    /// Analyze files in parallel to determine which need processing.
    /// Runs ffprobe concurrently.
    /// Returns the selected files and the counts from analysis and duplicate handling.
    pub(super) fn analyze_files(
        &self,
        files: Vec<VideoFile>,
        database: &mut Database,
        mut subtitle_matches: HashMap<PathBuf, Vec<SubtitleFile>>,
        abort_flag: &AtomicBool,
    ) -> (AnalysisOutput, RunStats) {
        let start = Instant::now();
        let total_files = files.len();

        // Extract the values needed for analysis to avoid borrowing self in the parallel context
        let filter = AnalysisFilter::from(&self.options);
        let movie_mode = self.options.movie_mode;

        // Phase 1 (sequential): bulk-load the scan cache into a HashMap for O(1) lookups,
        // then split files into cache hits (classified immediately) and cache misses.
        let scan_cache: HashMap<String, (VideoInfo, i64)> =
            database.get_all_scanned_files_with_mtime().unwrap_or_default();
        let mut cache_results: Vec<AnalysisResult> = Vec::new();
        let mut cache_misses: Vec<(VideoFile, Vec<SubtitleFile>)> = Vec::new();

        for file in files {
            let subtitles = subtitle_matches.remove(&file.path).unwrap_or_default();
            if let Some(path_key) = file.path.to_str()
                && let Some((cached_info, cached_mtime)) = scan_cache.get(path_key)
                && is_usable_scan_cache_entry(cached_info, *cached_mtime, &file)
            {
                cache_results
                    .push(ClassificationRequest::new(&filter, cached_info, movie_mode, subtitles).classify(file));
            } else {
                cache_misses.push((file, subtitles));
            }
        }

        self.observer
            .scan_cache_checked(cache_results.len(), cache_misses.len());

        // Phase 2 (parallel): run ffprobe only on cache misses, stopping early on abort.
        let probe_results: Vec<VideoInfoCache> = if cache_misses.is_empty() {
            Vec::new()
        } else {
            self.observer.probing_started(cache_misses.len());
            let results: Vec<VideoInfoCache> = cache_misses
                .into_par_iter()
                .filter_map(|(file, subtitles)| {
                    if abort_flag.load(Ordering::SeqCst) {
                        return None;
                    }
                    let result = probe_and_classify(file, subtitles, &filter, movie_mode);
                    self.observer.file_probed();
                    Some(result)
                })
                .collect();
            self.observer.probing_finished();
            results
        };

        // Phase 3: persist completed ffprobe results even if Ctrl+C interrupted remaining probes.
        if !self.options.dry_run {
            let cache_entries: Vec<(&Path, &VideoInfo)> = probe_results
                .iter()
                .filter_map(|entry| entry.info.as_ref().map(|info| (entry.path.as_path(), info)))
                .collect();
            if let Err(error) = database.batch_upsert_scanned_files(&cache_entries) {
                self.observer.scan_cache_write_failed(&error);
            }
        }

        // Combine cache hits (Phase 1) with ffprobe results (Phase 2)
        let mut results = cache_results;
        results.extend(probe_results.into_iter().map(|entry| entry.result));
        self.collect_analysis_results(
            resolve_output_collisions(results),
            database,
            abort_flag,
            total_files,
            start,
        )
    }

    /// Group the classified files, handle duplicates, and report the analysis counts.
    fn collect_analysis_results(
        &self,
        results: Vec<AnalysisResult>,
        database: &Database,
        abort_flag: &AtomicBool,
        total_files: usize,
        start: Instant,
    ) -> (AnalysisOutput, RunStats) {
        let mut output = AnalysisOutput::default();
        let mut analysis_stats = AnalysisStats::default();
        let mut duplicate_failures = 0;

        for result in results {
            if abort_flag.load(Ordering::SeqCst) {
                break;
            }
            match result {
                AnalysisResult::NeedsConversion(processable) => {
                    analysis_stats.to_convert += 1;
                    output.conversions.push(processable);
                }
                AnalysisResult::NeedsRemux(processable) => {
                    analysis_stats.to_remux += 1;
                    output.remuxes.push(processable);
                }
                AnalysisResult::NeedsRename(processable) => {
                    analysis_stats.to_rename += 1;
                    output.renames.push(processable);
                }
                AnalysisResult::NeedsSubtitleMux(processable) => {
                    analysis_stats.to_subtitle_mux += 1;
                    output.subtitle_muxes.push(processable);
                }
                AnalysisResult::Skip { file, reason } => {
                    if self.options.delete_duplicates
                        && let SkipReason::OutputExists {
                            path: target_path,
                            source_duration,
                        } = &reason
                    {
                        let outcome = self.delete_duplicate(&file.path, target_path, *source_duration);
                        if outcome.removes_source() {
                            analysis_stats.duplicates_deleted += 1;
                            if !self.options.dry_run {
                                let _ = database.remove_cached_path(&file.path);
                            }
                        } else {
                            analysis_stats.duplicate_delete_failed += 1;
                        }
                        if matches!(
                            outcome,
                            DuplicateOutcome::ProbeFailed { .. } | DuplicateOutcome::DeleteFailed { .. }
                        ) {
                            duplicate_failures += 1;
                        }
                        self.observer.duplicate_handled(&file.path, target_path, &outcome);
                    } else {
                        analysis_stats.count_skip(&reason);
                        self.observer.file_skipped(&file.path, &reason);
                    }
                }
            }
        }

        output.conversions.sort_by_order(self.options.sort);
        output.remuxes.sort_by_order(self.options.sort);
        output.subtitle_muxes.sort_by_order(self.options.sort);

        self.observer
            .analysis_finished(&analysis_stats, total_files, start.elapsed());

        let run_stats = RunStats {
            files_failed: analysis_stats.analysis_failed + duplicate_failures,
            duplicates_deleted: analysis_stats.duplicates_deleted,
            ..RunStats::default()
        };
        (output, run_stats)
    }
}

/// Return whether a cached scan entry is complete and still matches `file`'s size and modification time.
fn is_usable_scan_cache_entry(cached_info: &VideoInfo, cached_mtime: i64, file: &VideoFile) -> bool {
    cached_info.size_bytes == file.size_bytes
        && cached_info.bit_depth >= 8
        && (1.0..=240.0).contains(&cached_info.frames_per_second)
        && cached_mtime != 0
        && cached_mtime == file_modified_time_ns(&file.path)
}
