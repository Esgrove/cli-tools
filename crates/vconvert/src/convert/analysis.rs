//! File analysis for video convert.
//!
//! Classifies the gathered files in three phases.
//! Files with a valid scan cache entry are classified from the cache,
//! the rest are probed with ffprobe in parallel and their results written back to the cache,
//! and the combined results are collected into the operation lists and analysis statistics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use cli_tools_common::{print_error, print_yellow};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

use super::VideoConvert;
use super::duplicates::print_duplicate_outcome;
use crate::classification::{ClassificationRequest, probe_and_classify, resolve_output_collisions};
use crate::database::Database;
use crate::stats::AnalysisStats;
use crate::types::{
    AnalysisFilter, AnalysisOutput, AnalysisResult, ProcessableFile, ProcessableFileSliceExt, SkipReason, SubtitleFile,
    VideoFile, VideoInfo, VideoInfoCache,
};

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

impl VideoConvert {
    /// Analyze files in parallel to determine which need processing.
    /// Runs ffprobe on each file concurrently and filters based on video information.
    #[allow(clippy::too_many_lines)]
    pub(super) fn analyze_files(
        &self,
        files: Vec<VideoFile>,
        database: &mut Database,
        mut subtitle_matches: HashMap<PathBuf, Vec<SubtitleFile>>,
    ) -> AnalysisOutput {
        let start = Instant::now();
        let total_files = files.len();

        // Extract config values needed for analysis to avoid borrowing self in parallel context
        let filter = AnalysisFilter::from(&self.config);
        let movie_mode = self.config.movie_mode;

        // Phase 1 (sequential): bulk-load the scan cache into a HashMap for O(1) lookups,
        // then split files into cache hits (classified immediately) and cache misses.
        let scan_cache: HashMap<String, VideoInfo> = database.get_all_scanned_files().unwrap_or_default();
        let mut cache_results: Vec<AnalysisResult> = Vec::new();
        let mut cache_misses: Vec<(VideoFile, Vec<SubtitleFile>)> = Vec::new();

        for file in files {
            let path_key = file.path.to_string_lossy();
            let subtitles = subtitle_matches.remove(&file.path).unwrap_or_default();
            if let Some(cached_info) = scan_cache.get(path_key.as_ref())
                && cached_info.size_bytes == file.size_bytes
                && cached_info.bit_depth >= 8
            {
                cache_results
                    .push(ClassificationRequest::new(&filter, cached_info, movie_mode, subtitles).classify(file));
            } else {
                cache_misses.push((file, subtitles));
            }
        }

        let cache_hit_count = cache_results.len();
        let cache_miss_count = cache_misses.len();
        if self.config.verbose && cache_hit_count > 0 {
            println!(
                "Scan cache: {}, {} — running ffprobe on {}",
                cli_tools_common::count_label(cache_hit_count, "hit", "hits"),
                cli_tools_common::count_label(cache_miss_count, "miss", "misses"),
                cli_tools_common::count_label(cache_miss_count, "file", "files")
            );
        }

        // Phase 2 (parallel): run ffprobe only on cache misses.
        let probe_results: Vec<VideoInfoCache> = if cache_misses.is_empty() {
            Vec::new()
        } else {
            let progress_bar = ProgressBar::new(cache_miss_count as u64);
            progress_bar.set_style(
                ProgressStyle::default_bar()
                    .template(PROGRESS_BAR_TEMPLATE)
                    .expect("Failed to set progress bar template")
                    .progress_chars(PROGRESS_BAR_CHARS),
            );

            let results: Vec<VideoInfoCache> = cache_misses
                .into_par_iter()
                .map(|(file, subtitles)| {
                    let result = probe_and_classify(file, subtitles, &filter, movie_mode);
                    progress_bar.inc(1);
                    result
                })
                .collect();
            progress_bar.finish_and_clear();
            results
        };

        // Phase 3: write new ffprobe results back to the scan cache in a single transaction.
        let cache_entries: Vec<(&Path, &VideoInfo)> = probe_results
            .iter()
            .filter_map(|entry| entry.info.as_ref().map(|info| (entry.path.as_path(), info)))
            .collect();
        if let Err(error) = database.batch_upsert_scanned_files(&cache_entries)
            && self.config.verbose
        {
            print_yellow!("Failed to write scan cache: {error}");
        }

        // Combine cache hits (Phase 1) with ffprobe results (Phase 2)
        let mut results = cache_results;
        results.extend(probe_results.into_iter().map(|entry| entry.result));
        let results = resolve_output_collisions(results);

        // Collect files into separate vectors
        let mut conversions = Vec::new();
        let mut remuxes = Vec::new();
        let mut renames: Vec<ProcessableFile> = Vec::new();
        let mut subtitle_muxes = Vec::new();
        let mut analysis_stats = AnalysisStats::default();
        // Collect duplicate pairs for verbose output when not deleting
        let mut duplicate_pairs: Vec<(PathBuf, PathBuf)> = Vec::new();

        for result in results {
            match result {
                AnalysisResult::NeedsConversion(processable) => {
                    analysis_stats.to_convert += 1;
                    conversions.push(processable);
                }
                AnalysisResult::NeedsRemux(processable) => {
                    analysis_stats.to_remux += 1;
                    remuxes.push(processable);
                }
                AnalysisResult::NeedsRename(processable) => {
                    analysis_stats.to_rename += 1;
                    renames.push(processable);
                }
                AnalysisResult::NeedsSubtitleMux(processable) => {
                    analysis_stats.to_subtitle_mux += 1;
                    subtitle_muxes.push(processable);
                }
                AnalysisResult::Skip { file, reason } => {
                    match &reason {
                        SkipReason::AlreadyConverted => {
                            analysis_stats.skipped_converted += 1;
                        }
                        SkipReason::BitrateBelowThreshold { .. } => {
                            analysis_stats.skipped_bitrate_low += 1;
                        }
                        SkipReason::BitrateAboveThreshold { .. } => {
                            analysis_stats.skipped_bitrate_high += 1;
                        }
                        SkipReason::DurationBelowThreshold { .. } => {
                            analysis_stats.skipped_duration_short += 1;
                        }
                        SkipReason::DurationAboveThreshold { .. } => {
                            analysis_stats.skipped_duration_long += 1;
                        }
                        SkipReason::ResolutionBelowLimit { .. } => {
                            analysis_stats.skipped_resolution_low += 1;
                        }
                        SkipReason::OutputExists {
                            path: target_path,
                            source_duration,
                        } => {
                            if self.config.delete_duplicates {
                                let outcome = self.delete_duplicate(&file.path, target_path, *source_duration);
                                print_duplicate_outcome(&file.path, target_path, &outcome);
                                if outcome.removes_source() {
                                    analysis_stats.duplicates_deleted += 1;
                                } else {
                                    analysis_stats.duplicate_delete_failed += 1;
                                }
                            } else {
                                analysis_stats.skipped_duplicate += 1;
                                if self.config.verbose {
                                    duplicate_pairs.push((file.path.clone(), target_path.clone()));
                                }
                            }
                        }
                        SkipReason::OutputConflict { .. } => {
                            analysis_stats.skipped_output_conflict += 1;
                            print_yellow!("{}: {reason}", cli_tools_common::path_to_string_relative(&file.path));
                        }
                        SkipReason::FileMissing => {
                            analysis_stats.file_missing += 1;
                            print_yellow!(
                                "{}: File no longer exists (may have been moved or renamed)",
                                cli_tools_common::path_to_string_relative(&file.path)
                            );
                        }
                        SkipReason::AnalysisFailed { error } => {
                            analysis_stats.analysis_failed += 1;
                            print_error!("{}: {error}", cli_tools_common::path_to_string_relative(&file.path));
                        }
                    }
                    // Print skipped files (except OutputExists which is handled above, and AnalysisFailed)
                    if self.config.verbose
                        && !matches!(reason, SkipReason::AnalysisFailed { .. })
                        && !matches!(reason, SkipReason::OutputExists { .. })
                        && !matches!(reason, SkipReason::FileMissing)
                        && !matches!(reason, SkipReason::OutputConflict { .. })
                    {
                        print_yellow!("{}: {reason}", cli_tools_common::path_to_string_relative(&file.path));
                    }
                }
            }
        }

        // Print duplicate pairs if verbose and not deleting duplicates
        if self.config.verbose && !self.config.delete_duplicates && !duplicate_pairs.is_empty() {
            println!();
            println!("{}", "Duplicate pairs:".bold());
            for (source, target) in &duplicate_pairs {
                println!("  {}", cli_tools_common::path_to_string_relative(source));
                println!("  {}", cli_tools_common::path_to_string_relative(target));
                println!();
            }
        }

        // Sort conversions based on configured sort order
        conversions.sort_by_order(self.config.sort);
        remuxes.sort_by_order(self.config.sort);
        subtitle_muxes.sort_by_order(self.config.sort);

        self.log_analysis_stats(&analysis_stats, total_files, start.elapsed());
        analysis_stats.print_summary();

        AnalysisOutput {
            conversions,
            remuxes,
            renames,
            subtitle_muxes,
        }
    }
}
