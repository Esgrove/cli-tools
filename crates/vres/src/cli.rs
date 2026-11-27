//! The vres run loop.
//!
//! Gathers video files, probes their resolution via the shared `ScanCache` and ffprobe in parallel,
//! checks cross-tool `active_locks`, and renames or deletes files while keeping cache paths synchronized.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[cfg(test)]
use anyhow::anyhow;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

use cli_tools_common::file_handling::TargetReservations;
use cli_tools_common::{print_bold, print_green, print_yellow};
use cli_tools_db::locks::{AcquireOutcome, ActiveLockStore};
use cli_tools_media::Resolution;
use cli_tools_media::file_hash::{FileFingerprint, fingerprint_file};
use cli_tools_media::resolution::print_fuzzy_resolution_ranges;
use cli_tools_media::scan_cache::{CachedVideoInfo, ScanCache};
use cli_tools_media::video_info::VideoInfo;
#[cfg(test)]
use cli_tools_media::video_info::parse_display_resolution;

use crate::config::Config;
use crate::resolution::FFProbeResult;

const PROGRESS_BAR_CHARS: &str = "=>-";
const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";

/// Counts of successful and failed operations, excluding intentional skips.
#[derive(Default)]
struct OperationCounts {
    succeeded: usize,
    failed: usize,
}

/// Result of probing one cache-miss file in parallel.
struct ProbeAttempt {
    path: PathBuf,
    fingerprint: Option<FileFingerprint>,
    result: anyhow::Result<VideoInfo>,
}

/// Main entry point for the resolution CLI.
pub fn run(config: &Config) -> anyhow::Result<()> {
    if config.debug {
        print_fuzzy_resolution_ranges();
    }

    let files = gather_video_files(&config.path, config.recurse, config.delete_limit)?;

    if files.is_empty() {
        if config.verbose {
            println!("No video files to process");
        }
        return Ok(());
    }

    if config.verbose || config.debug {
        println!("Processing {} files...", files.len());
    }

    let mut cache = if config.no_cache { None } else { ScanCache::open().ok() };
    let lock_store = if config.dryrun {
        None
    } else {
        ActiveLockStore::open_default("vres").ok()
    };

    let (results, probe_failures) = get_resolutions(files, cache.as_mut())?;
    let (remaining, deletions) = match config.delete_limit {
        Some(limit) => delete_low_resolution_files(results, limit, config, cache.as_mut(), lock_store.as_ref()),
        None => (results, OperationCounts::default()),
    };
    let renames = add_resolution_labels(config, remaining, cache.as_mut(), lock_store.as_ref());

    anyhow::ensure!(
        probe_failures + deletions.failed + renames.failed == 0 || deletions.succeeded + renames.succeeded > 0,
        "No requested resolution work succeeded"
    );
    Ok(())
}

/// Rename video files to include their resolution label in the filename.
///
/// Files that already have the correct label or are actively locked by another tool are skipped.
/// Errors during renaming are printed and do not stop processing of remaining files.
fn add_resolution_labels(
    config: &Config,
    files: Vec<FFProbeResult>,
    mut cache: Option<&mut ScanCache>,
    lock_store: Option<&ActiveLockStore>,
) -> OperationCounts {
    let mut counts = OperationCounts::default();
    let mut files_to_process: Vec<(FFProbeResult, PathBuf)> = files
        .into_iter()
        .filter_map(|result| match result.new_path_if_needed() {
            Ok(Some(new_path)) => Some((result, new_path)),
            Ok(None) => None,
            Err(err) => {
                eprintln!("Error: {err}");
                counts.failed += 1;
                None
            }
        })
        .collect();

    files_to_process.sort_unstable_by(|a, b| {
        a.0.resolution
            .cmp(&b.0.resolution)
            .then_with(|| a.0.file.cmp(&b.0.file))
    });

    let num_files = files_to_process.len();
    if files_to_process.is_empty() {
        if config.verbose {
            println!("No video files require renaming");
        }
        return counts;
    } else if config.verbose {
        print_bold!("Renaming {}", cli_tools_common::count_label(num_files, "file", "files"));
    }

    print_bold!("Resolution               Label   Path");

    // Several files can map to the same target, for example "Movie.mp4" and "Movie.1920x1080.mp4".
    // Only the first one is renamed, so overwrite never replaces a file renamed earlier in the same run.
    let mut reservations = TargetReservations::new();
    for (result, new_path) in files_to_process {
        if !reservations.reserve(&new_path) {
            cli_tools_common::print_error!(
                "Skipping {}, another file is already renamed to {}",
                cli_tools_common::path_to_string_relative(&result.file),
                cli_tools_common::path_to_string_relative(&new_path)
            );
            continue;
        }
        let _lock_guard = if let Some(store) = lock_store {
            match store.try_acquire(&[result.file.clone(), new_path.clone()], "rename") {
                Ok(AcquireOutcome::Acquired(guard)) => Some(guard),
                Ok(AcquireOutcome::AlreadyLocked(holder)) => {
                    print_yellow!(
                        "Skipping locked file (held by {} pid {}): {}",
                        holder.tool,
                        holder.pid,
                        cli_tools_common::path_to_string_relative(&result.file)
                    );
                    continue;
                }
                Err(_) => None,
            }
        } else {
            None
        };

        match result.rename(&new_path, config.overwrite, config.dryrun) {
            Ok(()) => {
                counts.succeeded += 1;
                if !config.dryrun
                    && let Some(scan_cache) = cache.as_deref_mut()
                {
                    let _ = scan_cache.move_cached_path(&result.file, &new_path);
                }
            }
            Err(error) => {
                counts.failed += 1;
                cli_tools_common::print_error!("{error}");
            }
        }
    }

    let action = if config.dryrun { "Would rename" } else { "Renamed" };
    print_green!(
        "{action} {}",
        cli_tools_common::count_label(counts.succeeded, "file", "files")
    );
    counts
}

/// Deletes files with valid non-zero resolution smaller than the given limit.
/// Returns the files not selected for deletion and the deletion counts.
fn delete_low_resolution_files(
    results: Vec<FFProbeResult>,
    limit: u32,
    config: &Config,
    mut cache: Option<&mut ScanCache>,
    lock_store: Option<&ActiveLockStore>,
) -> (Vec<FFProbeResult>, OperationCounts) {
    let mut counts = OperationCounts::default();
    let (to_delete, to_keep): (Vec<_>, Vec<_>) = results.into_iter().partition(|result| {
        result.resolution.width > 0 && result.resolution.height > 0 && result.resolution.is_smaller_than(limit)
    });

    if !to_delete.is_empty() {
        let num_delete = to_delete.len();
        if config.dryrun {
            print_bold!(
                "DRYRUN: Would delete {} smaller than {limit}:",
                cli_tools_common::count_label(num_delete, "file", "files")
            );
        } else if config.verbose {
            print_bold!(
                "Deleting {} smaller than {limit}:",
                cli_tools_common::count_label(num_delete, "file", "files")
            );
        }

        print_bold!("Resolution            Path");
        for result in to_delete {
            let _lock_guard = if let Some(store) = lock_store {
                match store.try_acquire(std::slice::from_ref(&result.file), "delete") {
                    Ok(AcquireOutcome::Acquired(guard)) => Some(guard),
                    Ok(AcquireOutcome::AlreadyLocked(holder)) => {
                        print_yellow!(
                            "Skipping locked file (held by {} pid {}): {}",
                            holder.tool,
                            holder.pid,
                            cli_tools_common::path_to_string_relative(&result.file)
                        );
                        continue;
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

            match result.delete(config.dryrun) {
                Ok(()) => {
                    counts.succeeded += 1;
                    if !config.dryrun
                        && let Some(scan_cache) = cache.as_deref_mut()
                    {
                        let _ = scan_cache.remove_cached_path(&result.file);
                    }
                }
                Err(error) => {
                    counts.failed += 1;
                    cli_tools_common::print_error!("{error}");
                }
            }
        }

        if config.verbose {
            if config.dryrun {
                print_green!(
                    "Would delete {}",
                    cli_tools_common::count_label(counts.succeeded, "file", "files")
                );
            } else {
                print_green!(
                    "Deleted {}",
                    cli_tools_common::count_label(counts.succeeded, "file", "files")
                );
            }
        }
    } else if config.verbose {
        println!("No files smaller than {limit}");
    }

    (to_keep, counts)
}

/// Return whether `extension` matches a canonical video extension, ignoring ASCII case.
fn is_supported_extension(extension: &str) -> bool {
    cli_tools_media::is_video_extension(extension)
}

/// Return whether a filename's existing resolution label allows skipping probing during discovery.
fn should_skip_labeled_filename(filename: &str, delete_limit: Option<u32>) -> bool {
    delete_limit.is_none()
        && !Resolution::full_resolution_regex().is_match(filename)
        && Resolution::p_label_regex().is_match(filename)
}

/// Gathers video files for processing without following symlinks.
/// In delete mode, filename labels are not bounds on the actual dimensions, so they cannot skip probing.
/// In rename-only mode, skips files that already have a word-bounded resolution label and no full dimensions.
fn gather_video_files(path: &Path, recurse: bool, delete_limit: Option<u32>) -> anyhow::Result<Vec<PathBuf>> {
    if !path.exists() {
        anyhow::bail!("Path does not exist: {}", path.display());
    }

    // A single file given as the input is processed on its own, like the help text says.
    if path.is_file() {
        let is_video = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(is_supported_extension);
        if !is_video {
            anyhow::bail!("Not a supported video file: {}", path.display());
        }
        return Ok(vec![path.to_path_buf()]);
    }

    let mut files = Vec::new();
    let walker = if recurse {
        WalkDir::new(path).follow_links(false)
    } else {
        WalkDir::new(path).follow_links(false).max_depth(1)
    };

    for entry in walker
        .into_iter()
        .filter_entry(|entry| !cli_tools_common::should_skip_entry(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let entry_path = entry.path();
        if let Some(ext) = entry_path.extension().and_then(|s| s.to_str())
            && is_supported_extension(ext)
            && entry_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|filename| !should_skip_labeled_filename(filename, delete_limit))
        {
            files.push(entry_path.to_path_buf());
        }
    }

    Ok(files)
}

/// Resolve video dimensions via the shared `ScanCache` and parallel `ffprobe` via Rayon.
fn get_resolutions(files: Vec<PathBuf>, cache: Option<&mut ScanCache>) -> anyhow::Result<(Vec<FFProbeResult>, usize)> {
    let cached_entries: HashMap<String, CachedVideoInfo> = cache
        .as_ref()
        .and_then(|scan_cache| scan_cache.get_all().ok())
        .unwrap_or_default();

    let mut results = Vec::with_capacity(files.len());
    let mut cache_misses: Vec<(PathBuf, Option<FileFingerprint>)> = Vec::new();

    for path in files {
        let fingerprint = fingerprint_file(&path).ok();
        if let (Some(path_str), Some(fingerprint)) = (path.to_str(), fingerprint)
            && let Some(cached) = cached_entries.get(path_str)
            && cached.is_current(&fingerprint)
            && cached.width > 0
            && cached.height > 0
        {
            results.push(FFProbeResult {
                file: path,
                resolution: Resolution::new(cached.width, cached.height),
            });
        } else {
            cache_misses.push((path, fingerprint));
        }
    }

    let mut failed = 0usize;
    if !cache_misses.is_empty() {
        let progress_bar = if cfg!(test) {
            ProgressBar::hidden()
        } else {
            let bar = ProgressBar::new(cache_misses.len() as u64);
            bar.set_style(
                ProgressStyle::default_bar()
                    .template(PROGRESS_BAR_TEMPLATE)?
                    .progress_chars(PROGRESS_BAR_CHARS),
            );
            bar
        };

        let attempts: Vec<ProbeAttempt> = cache_misses
            .into_par_iter()
            .map(|(path, fingerprint)| {
                let result = VideoInfo::from_path(&path);
                progress_bar.inc(1);
                ProbeAttempt {
                    path,
                    fingerprint,
                    result,
                }
            })
            .collect();
        progress_bar.finish_and_clear();

        let mut newly_probed: Vec<(PathBuf, VideoInfo, FileFingerprint)> = Vec::new();
        for attempt in attempts {
            match attempt.result {
                Ok(info) => {
                    if let Some(fingerprint) = attempt.fingerprint {
                        newly_probed.push((attempt.path.clone(), info.clone(), fingerprint));
                    }
                    if let Some(resolution) = info.resolution {
                        results.push(FFProbeResult {
                            file: attempt.path,
                            resolution,
                        });
                    } else {
                        eprintln!("Error: No resolution found for {}", attempt.path.display());
                        failed += 1;
                    }
                }
                Err(error) => {
                    eprintln!("Error: {error}");
                    failed += 1;
                }
            }
        }

        if let Some(scan_cache) = cache
            && !newly_probed.is_empty()
        {
            let batch: Vec<(&Path, &VideoInfo, FileFingerprint)> = newly_probed
                .iter()
                .map(|(path, info, fingerprint)| (path.as_path(), info, *fingerprint))
                .collect();
            let _ = scan_cache.batch_upsert_with_fingerprints(&batch);
        }
    }

    results.sort_unstable_by(|a, b| a.resolution.cmp(&b.resolution).then_with(|| a.file.cmp(&b.file)));

    Ok((results, failed))
}

/// Run ffprobe on a single file and parse its display resolution.
#[cfg(test)]
fn run_ffprobe(file: PathBuf) -> anyhow::Result<FFProbeResult> {
    let info = VideoInfo::from_path(&file)?;
    let resolution = info
        .resolution
        .ok_or_else(|| anyhow!("No resolution found for {}", file.display()))?;
    Ok(FFProbeResult { file, resolution })
}

/// Parse the displayed resolution from ffprobe `key=value` output such as "width=1920\nheight=1080\n".
///
/// A rotated video reports its stored dimensions with a rotation, which is applied here.
///
/// # Errors
/// Returns an error if the output has no width or no height.
#[cfg(test)]
pub fn parse_ffprobe_output(output: &[u8]) -> anyhow::Result<Resolution> {
    let text = String::from_utf8_lossy(output);
    parse_display_resolution(&text).ok_or_else(|| anyhow!("Missing width or height in ffprobe output: {text:?}"))
}

#[cfg(test)]
mod regex_tests {
    use super::*;

    #[test]
    fn regex_full_resolution_matches_standard_patterns() {
        assert!(Resolution::full_resolution_regex().is_match("video.1920x1080.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.1080x1920.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.960x540.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.3840x2160.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.720x480.mp4"));
    }

    #[test]
    fn regex_full_resolution_no_match_label_only() {
        assert!(!Resolution::full_resolution_regex().is_match("video.1080p.mp4"));
        assert!(!Resolution::full_resolution_regex().is_match("video.720p.mp4"));
        assert!(!Resolution::full_resolution_regex().is_match("video.mp4"));
    }

    #[test]
    fn regex_full_resolution_no_match_small_numbers() {
        assert!(!Resolution::full_resolution_regex().is_match("video.3x4.mp4"));
        assert!(!Resolution::full_resolution_regex().is_match("video.12x34.mp4"));
    }

    #[test]
    fn regex_full_resolution_word_boundaries() {
        // \b treats digits, letters, and underscore as word characters.
        // The regex \b\d{3,4}x\d{3,4}\b requires a word boundary before the first digit
        // and after the last digit, preventing matches inside larger word-char sequences.
        // Dot-separated patterns match because dots are non-word characters.
        assert!(Resolution::full_resolution_regex().is_match("video.1920x1080.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.960x540.540p.mp4"));
        // 5-digit sequences are rejected since resolutions never exceed 4 digits
        assert!(!Resolution::full_resolution_regex().is_match("video.21920x1080.mp4"));
        assert!(!Resolution::full_resolution_regex().is_match("video.1920x10800.mp4"));
    }

    #[test]
    fn regex_full_resolution_matches_with_label_present() {
        // Files with both full resolution and label should match
        assert!(Resolution::full_resolution_regex().is_match("video.960x540.540p.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.1920x1080.1080p.mp4"));
        assert!(Resolution::full_resolution_regex().is_match("video.1080x1920.Vertical.1080p.mp4"));
    }

    #[test]
    fn regex_resolutions_matches_standard() {
        assert!(Resolution::p_label_regex().is_match("video.480p.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.540p.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.576p.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.720p.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.1080p.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.1440p.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.2160p.mp4"));
    }

    #[test]
    fn regex_resolutions_case_insensitive() {
        assert!(Resolution::p_label_regex().is_match("video.1080P.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.720P.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match() {
        assert!(!Resolution::p_label_regex().is_match("video.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.360p.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.4k.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.1080.mp4"));
    }

    #[test]
    fn regex_resolutions_matches_600p() {
        assert!(Resolution::p_label_regex().is_match("video.600p.mp4"));
    }

    #[test]
    fn regex_resolutions_at_start_of_filename() {
        assert!(Resolution::p_label_regex().is_match("1080p.video.mp4"));
    }

    #[test]
    fn regex_resolutions_at_end_of_filename() {
        assert!(Resolution::p_label_regex().is_match("video.1080p"));
    }

    #[test]
    fn regex_resolutions_in_middle_of_filename() {
        assert!(Resolution::p_label_regex().is_match("my.video.1080p.2024.mp4"));
    }

    #[test]
    fn regex_resolutions_mixed_case() {
        assert!(Resolution::p_label_regex().is_match("video.1080P.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.720P.mp4"));
        assert!(Resolution::p_label_regex().is_match("video.1440P.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match_8k() {
        assert!(!Resolution::p_label_regex().is_match("video.4320p.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.8k.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match_partial() {
        // Should not match partial resolution strings
        assert!(!Resolution::p_label_regex().is_match("video.108p.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.72p.mp4"));
    }

    #[test]
    fn regex_resolutions_no_match_without_p_suffix() {
        assert!(!Resolution::p_label_regex().is_match("video.1080.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.720.mp4"));
        assert!(!Resolution::p_label_regex().is_match("video.1920x1080.mp4"));
    }

    #[test]
    fn regex_resolutions_matches_544p() {
        assert!(Resolution::p_label_regex().is_match("video.544p.mp4"));
    }

    #[test]
    fn regex_resolutions_embedded_in_word_does_not_match() {
        assert!(!Resolution::p_label_regex().is_match("video1080ptest.mp4"));
        assert!(!Resolution::p_label_regex().is_match("episode_1080part2.mp4"));
        assert!(!Resolution::p_label_regex().is_match("12720p.mp4"));
    }

    #[test]
    fn regex_resolutions_only_resolution() {
        assert!(Resolution::p_label_regex().is_match("1080p"));
        assert!(Resolution::p_label_regex().is_match("720p"));
        assert!(Resolution::p_label_regex().is_match("2160p"));
    }
}

#[cfg(test)]
mod ffpprobe_tests {
    use super::*;
    use cli_tools_media::video_info::VideoInfo;

    #[test]
    fn cover_art_without_video_does_not_supply_video_dimensions() {
        if !cli_tools_common::command_exists("ffmpeg") || !cli_tools_common::command_exists("ffprobe") {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("album.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=s=320x320:r=1:d=1",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=d=1",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "mjpeg",
                "-disposition:v:0",
                "attached_pic",
                "-c:a",
                "aac",
            ])
            .arg(&file)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg should generate an album with cover art");

        let shared = VideoInfo::from_path(&file);
        let dedicated = run_ffprobe(file);
        assert_eq!((shared.is_err(), dedicated.is_err()), (true, true));
    }

    #[test]
    fn parse_ffprobe_output_windows_crlf() {
        let output = b"width=1920\r\nheight=1080\r\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_large_resolution() {
        let output = b"width=7680\nheight=4320\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 7680);
        assert_eq!(result.height, 4320);
    }

    #[test]
    fn parse_ffprobe_output_swaps_dimensions_for_a_rotated_video() {
        let output = b"width=1920\nheight=1080\nrotation=-90\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!((result.width, result.height), (1080, 1920));
    }

    #[test]
    fn parse_ffprobe_output_missing_width() {
        let output = b"height=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_missing_height() {
        let output = b"width=1920\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_empty() {
        let output = b"";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_malformed_width() {
        let output = b"width=abc\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_malformed_height() {
        let output = b"width=1920\nheight=xyz\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_1080p() {
        let output = b"width=1920\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_720p() {
        let output = b"width=1280\nheight=720\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1280);
        assert_eq!(result.height, 720);
    }

    #[test]
    fn parse_ffprobe_output_4k() {
        let output = b"width=3840\nheight=2160\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 3840);
        assert_eq!(result.height, 2160);
    }

    #[test]
    fn parse_ffprobe_output_vertical() {
        let output = b"width=1080\nheight=1920\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1080);
        assert_eq!(result.height, 1920);
    }

    #[test]
    fn parse_ffprobe_output_no_trailing_newline() {
        let output = b"width=1920\nheight=1080";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_zero_dimensions() {
        let output = b"width=0\nheight=0\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 0);
        assert_eq!(result.height, 0);
    }

    #[test]
    fn parse_ffprobe_output_square() {
        let output = b"width=1080\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1080);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_single_digit() {
        let output = b"width=1\nheight=1\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1);
        assert_eq!(result.height, 1);
    }

    #[test]
    fn parse_ffprobe_output_480p() {
        let output = b"width=640\nheight=480\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 640);
        assert_eq!(result.height, 480);
    }

    #[test]
    fn parse_ffprobe_output_1440p() {
        let output = b"width=2560\nheight=1440\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 2560);
        assert_eq!(result.height, 1440);
    }

    #[test]
    fn parse_ffprobe_output_ultrawide() {
        let output = b"width=3440\nheight=1440\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 3440);
        assert_eq!(result.height, 1440);
    }

    #[test]
    fn parse_ffprobe_output_very_small() {
        let output = b"width=64\nheight=64\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 64);
        assert_eq!(result.height, 64);
    }

    #[test]
    fn parse_ffprobe_output_mixed_crlf_lf() {
        // First line with CRLF, second with LF
        let output = b"width=1920\r\nheight=1080\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_negative_width() {
        let output = b"width=-1920\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_negative_height() {
        let output = b"width=1920\nheight=-1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_float_width() {
        let output = b"width=1920.5\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_empty_width_value() {
        let output = b"width=\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_empty_height_value() {
        let output = b"width=1920\nheight=\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_whitespace_in_width() {
        let output = b"width= 1920\nheight=1080\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_only_newlines() {
        let output = b"\n\n\n";
        let result = parse_ffprobe_output(output);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ffprobe_output_swapped_order() {
        let output = b"height=1080\nwidth=1920\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!((result.width, result.height), (1920, 1080));
    }

    #[test]
    fn parse_ffprobe_output_extra_fields_after() {
        let output = b"width=1920\nheight=1080\ncodec=h264\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 1920);
        assert_eq!(result.height, 1080);
    }

    #[test]
    fn parse_ffprobe_output_large_values() {
        let output = b"width=15360\nheight=8640\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 15360);
        assert_eq!(result.height, 8640);
    }

    #[test]
    fn parse_ffprobe_output_sd_resolution() {
        let output = b"width=720\nheight=576\n";
        let result = parse_ffprobe_output(output).unwrap();
        assert_eq!(result.width, 720);
        assert_eq!(result.height, 576);
    }
}

#[cfg(test)]
mod test_gather_video_files {
    use super::*;

    /// Temporary directory with a visible name, so the walker does not skip it as hidden.
    fn temporary_directory() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("vres_test")
            .tempdir()
            .expect("temporary directory should be created")
    }

    /// Create a file inside the directory, creating parent directories as needed.
    fn write(directory: &Path, relative: &str) -> PathBuf {
        let path = directory.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directories should be created");
        }
        std::fs::write(&path, b"video").expect("file should be written");
        path
    }

    /// File names of the gathered files, sorted.
    fn names(files: &[PathBuf]) -> Vec<String> {
        let mut names: Vec<String> = files
            .iter()
            .map(|path| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_single_video_file_path_is_gathered() {
        let directory = temporary_directory();
        let file = write(directory.path(), "clip.mp4");

        let files = gather_video_files(&file, false, None).expect("a single file should be accepted");

        assert_eq!(files, vec![file]);
    }

    #[test]
    fn keeps_video_files_and_skips_other_types() {
        let directory = temporary_directory();
        write(directory.path(), "movie.mp4");
        write(directory.path(), "clip.mkv");
        write(directory.path(), "notes.txt");
        write(directory.path(), "cover.jpg");

        let files = gather_video_files(directory.path(), false, None).expect("gathering should succeed");

        assert_eq!(names(&files), vec!["clip.mkv", "movie.mp4"]);
    }

    #[test]
    fn a_file_that_already_has_a_resolution_label_is_skipped() {
        let directory = temporary_directory();
        write(directory.path(), "movie.mp4");
        write(directory.path(), "labelled.1080p.mp4");

        let files = gather_video_files(directory.path(), false, None).expect("gathering should succeed");

        assert_eq!(names(&files), vec!["movie.mp4"]);
    }

    #[test]
    fn a_full_resolution_in_the_name_is_still_gathered_so_it_can_be_replaced() {
        let directory = temporary_directory();
        write(directory.path(), "movie.1920x1080.mp4");

        let files = gather_video_files(directory.path(), false, None).expect("gathering should succeed");

        assert_eq!(names(&files), vec!["movie.1920x1080.mp4"]);
    }

    #[test]
    fn delete_mode_ignores_filename_labels_even_at_the_default_threshold() {
        let directory = temporary_directory();
        write(directory.path(), "small.480p.mp4");
        write(directory.path(), "large.1080p.mp4");

        let files = gather_video_files(directory.path(), false, Some(500)).expect("gathering should succeed");

        assert_eq!(names(&files), vec!["large.1080p.mp4", "small.480p.mp4"]);
    }

    #[test]
    fn delete_mode_matches_uppercase_extensions() {
        let directory = temporary_directory();
        write(directory.path(), "clip.720p.MP4");
        write(directory.path(), "movie.1080p.MOV");

        let files = gather_video_files(directory.path(), false, Some(1080)).expect("gathering should succeed");
        assert_eq!(names(&files), vec!["clip.720p.MP4", "movie.1080p.MOV"]);

        let single = gather_video_files(&directory.path().join("movie.1080p.MOV"), false, None)
            .expect("uppercase single file should be accepted");
        assert_eq!(names(&single), vec!["movie.1080p.MOV"]);
    }

    #[test]
    fn delete_low_resolution_files_does_not_delete_zero_dimensions() {
        let directory = temporary_directory();
        let zero_path = write(directory.path(), "broken.mp4");
        let small_path = write(directory.path(), "small.mp4");
        let config = Config {
            path: directory.path().to_path_buf(),
            debug: false,
            delete_limit: Some(500),
            no_cache: true,
            dryrun: false,
            overwrite: false,
            recurse: false,
            verbose: false,
        };
        let results = vec![
            FFProbeResult {
                file: zero_path.clone(),
                resolution: Resolution::new(0, 0),
            },
            FFProbeResult {
                file: small_path.clone(),
                resolution: Resolution::new(320, 240),
            },
        ];

        let (kept, deletions) = delete_low_resolution_files(results, 500, &config, None, None);
        assert_eq!(deletions.succeeded, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].file, zero_path);
        assert!(zero_path.exists());
        assert!(!small_path.exists());
    }

    #[test]
    fn subdirectories_are_only_walked_when_recursing() {
        let directory = temporary_directory();
        write(directory.path(), "top.mp4");
        write(directory.path(), "nested/deep.mp4");

        let shallow = gather_video_files(directory.path(), false, None).expect("gathering should succeed");
        let deep = gather_video_files(directory.path(), true, None).expect("gathering should succeed");

        assert_eq!(names(&shallow), vec!["top.mp4"]);
        assert_eq!(names(&deep), vec!["deep.mp4", "top.mp4"]);
    }

    #[test]
    fn hidden_files_are_skipped_in_both_modes() {
        let directory = temporary_directory();
        write(directory.path(), "movie.mp4");
        write(directory.path(), ".hidden.mp4");

        let shallow = gather_video_files(directory.path(), false, None).expect("gathering should succeed");
        let deep = gather_video_files(directory.path(), true, None).expect("gathering should succeed");

        assert_eq!(names(&shallow), vec!["movie.mp4"]);
        assert_eq!(names(&deep), vec!["movie.mp4"]);
    }

    #[test]
    fn a_missing_directory_is_an_error_without_recursing() {
        let directory = temporary_directory();

        let result = gather_video_files(&directory.path().join("missing"), false, None);

        assert!(result.is_err());
    }

    #[test]
    fn an_empty_directory_gathers_nothing() {
        let directory = temporary_directory();

        let files = gather_video_files(directory.path(), true, None).expect("gathering should succeed");

        assert_eq!(files, [] as [PathBuf; 0]);
    }
}

#[cfg(test)]
mod test_add_resolution_labels {
    use super::*;

    #[test]
    fn only_successful_renames_are_counted() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let free = directory.path().join("free.mp4");
        let blocked = directory.path().join("blocked.mp4");
        std::fs::write(&free, "free").expect("write");
        std::fs::write(&blocked, "blocked").expect("write");
        std::fs::write(directory.path().join("blocked.1080p.mp4"), "existing").expect("write");
        let config = Config {
            debug: false,
            delete_limit: None,
            no_cache: true,
            dryrun: false,
            overwrite: false,
            path: directory.path().to_path_buf(),
            recurse: false,
            verbose: false,
        };
        let results = [free, blocked]
            .into_iter()
            .map(|file| FFProbeResult {
                file,
                resolution: Resolution::new(1920, 1080),
            })
            .collect();

        let renamed = add_resolution_labels(&config, results, None, None);

        assert_eq!(renamed.succeeded, 1);
    }
}

#[cfg(test)]
mod test_delete_low_resolution_files {
    use super::*;

    /// Config in dryrun mode, so no test ever deletes a file.
    fn config(verbose: bool) -> Config {
        Config {
            debug: false,
            delete_limit: Some(720),
            no_cache: true,
            dryrun: true,
            overwrite: false,
            path: PathBuf::from("."),
            recurse: false,
            verbose,
        }
    }

    /// Probe result for a file of the given resolution.
    fn result(name: &str, width: u32, height: u32) -> FFProbeResult {
        FFProbeResult {
            file: PathBuf::from(name),
            resolution: Resolution::new(width, height),
        }
    }

    /// File names of the kept results, sorted.
    fn kept(results: &[FFProbeResult]) -> Vec<String> {
        let mut names: Vec<String> = results
            .iter()
            .map(|entry| entry.file.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn files_below_the_limit_are_dropped_from_the_remaining_list() {
        let results = vec![
            result("small.mp4", 640, 480),
            result("large.mp4", 1920, 1080),
            result("exact.mp4", 1280, 720),
        ];

        let (remaining, _) = delete_low_resolution_files(results, 720, &config(true), None, None);

        assert_eq!(kept(&remaining), vec!["exact.mp4", "large.mp4"]);
    }

    #[test]
    fn nothing_below_the_limit_keeps_every_file() {
        let results = vec![result("large.mp4", 1920, 1080), result("exact.mp4", 1280, 720)];

        let (remaining, _) = delete_low_resolution_files(results, 720, &config(true), None, None);

        assert_eq!(kept(&remaining), vec!["exact.mp4", "large.mp4"]);
    }

    #[test]
    fn an_empty_list_stays_empty() {
        let (remaining, _) = delete_low_resolution_files(Vec::new(), 720, &config(false), None, None);

        assert_eq!(remaining, [] as [FFProbeResult; 0]);
    }

    #[test]
    fn a_quiet_run_reports_less_but_keeps_the_same_files() {
        let results = vec![result("small.mp4", 640, 480), result("large.mp4", 1920, 1080)];

        let (remaining, _) = delete_low_resolution_files(results, 720, &config(false), None, None);

        assert_eq!(kept(&remaining), vec!["large.mp4"]);
    }
}

#[cfg(test)]
mod test_gather_nested_trees {
    use cli_tools_common::test_support::TempTree;

    use super::*;

    /// Paths of the gathered files relative to the tree root, sorted, with "/" separators.
    fn relative(tree: &TempTree, files: &[PathBuf]) -> Vec<String> {
        let mut paths: Vec<String> = files
            .iter()
            .map(|path| {
                path.strip_prefix(tree.path())
                    .expect("gathered file should be inside the tree")
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect();
        paths.sort();
        paths
    }

    /// Gather from the tree root in both modes and return the relative paths as (shallow, recursive).
    fn gather_both(tree: &TempTree) -> (Vec<String>, Vec<String>) {
        let shallow = gather_video_files(tree.path(), false, None).expect("shallow gathering should succeed");
        let deep = gather_video_files(tree.path(), true, None).expect("recursive gathering should succeed");
        (relative(tree, &shallow), relative(tree, &deep))
    }

    #[test]
    fn videos_at_every_depth_are_gathered_only_when_recursing() {
        let tree = TempTree::with_entries(&[
            "top.mp4",
            "one/first.mkv",
            "one/two/second.mp4",
            "one/two/three/third.mp4",
            "one/two/three/notes.txt",
            "other/four/five/six/deepest.mp4",
            "empty/nested/",
        ]);

        let (shallow, deep) = gather_both(&tree);

        assert_eq!(shallow, vec!["top.mp4"]);
        assert_eq!(
            deep,
            vec![
                "one/first.mkv",
                "one/two/second.mp4",
                "one/two/three/third.mp4",
                "other/four/five/six/deepest.mp4",
                "top.mp4",
            ]
        );
    }

    #[test]
    fn hidden_directories_and_files_are_skipped_at_any_depth() {
        let tree = TempTree::with_entries(&[
            "visible.mp4",
            ".hidden.mp4",
            ".cache/inside.mp4",
            "season/episode.mp4",
            "season/.partial.mp4",
            "season/.thumbnails/preview.mp4",
            "season/extras/.git/object.mp4",
            "season/extras/bonus.mp4",
        ]);

        let (shallow, deep) = gather_both(&tree);

        assert_eq!(shallow, vec!["visible.mp4"]);
        assert_eq!(
            deep,
            vec!["season/episode.mp4", "season/extras/bonus.mp4", "visible.mp4"]
        );
    }

    #[test]
    fn system_directories_are_skipped_when_recursing() {
        let tree = TempTree::with_entries(&[
            "movie.mp4",
            "$RECYCLE.BIN/deleted.mp4",
            "nested/#Recycle/removed.mp4",
            "nested/kept.mp4",
        ]);

        let (_, deep) = gather_both(&tree);

        assert_eq!(deep, vec!["movie.mp4", "nested/kept.mp4"]);
    }

    #[test]
    fn delete_mode_probes_fuzzy_720p_files_from_directories() {
        if !cli_tools_common::command_exists("ffmpeg") || !cli_tools_common::command_exists("ffprobe") {
            return;
        }
        let tree = TempTree::with_entries(&["nested/"]);
        let video = tree.join("nested/clip.720p.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=s=1270x710:r=1:d=1",
                "-c:v",
                "mpeg4",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg should generate a fuzzy 720p video");

        let measured = run_ffprobe(video.clone()).unwrap().resolution;
        assert_eq!(measured.label(), "720p");
        assert!(measured.is_smaller_than(720));

        for (root, recurse) in [(tree.join("nested"), false), (tree.path().to_path_buf(), true)] {
            assert_eq!(
                gather_video_files(&root, recurse, Some(720)).unwrap(),
                std::slice::from_ref(&video)
            );
        }
    }

    #[test]
    fn labelled_files_are_skipped_in_nested_directories_too() {
        let tree = TempTree::with_entries(&["a/b/c/plain.mp4", "a/b/c/done.1080p.mp4", "a/b/vertical.1080x1920.mp4"]);

        let (_, deep) = gather_both(&tree);

        assert_eq!(deep, vec!["a/b/c/plain.mp4", "a/b/vertical.1080x1920.mp4"]);
    }

    #[test]
    fn symlinked_directories_are_not_followed() {
        let tree = TempTree::with_entries(&["library/show/episode.mp4"]);
        if tree.try_symlink("library", "shortcut").is_none()
            || tree.try_symlink("library", "library/show/loop").is_none()
        {
            return;
        }

        let (shallow, deep) = gather_both(&tree);

        assert_eq!(shallow, Vec::<String>::new());
        assert_eq!(deep, vec!["library/show/episode.mp4"]);
    }

    #[test]
    fn symlinked_files_are_skipped_in_both_modes() {
        let tree = TempTree::with_entries(&["real.mp4", "nested/real.mkv"]);
        if tree.try_symlink("real.mp4", "link.mp4").is_none()
            || tree.try_symlink("nested/real.mkv", "nested/link.mkv").is_none()
        {
            return;
        }

        let (shallow, deep) = gather_both(&tree);

        assert_eq!(shallow, vec!["real.mp4"]);
        assert_eq!(deep, vec!["nested/real.mkv", "real.mp4"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_is_skipped_without_failing_the_walk() {
        let tree = TempTree::with_entries(&["valid.mp4", "nested/valid.mp4"]);
        let created = tree.non_utf8_file("nested", "broken", "mp4").is_some();

        let (_, deep) = gather_both(&tree);

        assert_eq!(
            deep,
            vec!["nested/valid.mp4", "valid.mp4"],
            "non-UTF-8 created: {created}"
        );
    }

    #[test]
    fn a_root_that_is_a_non_video_file_is_an_error() {
        let tree = TempTree::with_entries(&["notes.txt"]);

        let shallow = gather_video_files(&tree.join("notes.txt"), false, None);
        let deep = gather_video_files(&tree.join("notes.txt"), true, None);

        assert!(shallow.is_err());
        assert!(deep.is_err());
    }

    #[test]
    fn a_nested_video_file_given_as_the_root_is_gathered_alone() {
        let tree = TempTree::with_entries(&["a/b/c/clip.mp4", "a/b/c/other.mp4"]);
        let file = tree.join("a/b/c/clip.mp4");

        let files = gather_video_files(&file, true, None).expect("a single video file should be accepted");

        assert_eq!(files, vec![file]);
    }

    #[test]
    fn gathering_does_not_change_the_tree() {
        let tree = TempTree::with_entries(&["a/one.mp4", "a/b/two.mkv", "a/b/c/three.txt", ".hidden/four.mp4"]);
        let before = tree.snapshot();

        gather_both(&tree);

        assert_eq!(tree.snapshot(), before);
    }
}

#[cfg(test)]
mod test_add_resolution_labels_in_trees {
    use cli_tools_common::test_support::TempTree;

    use super::*;

    /// Config for renaming inside the tree.
    fn config(tree: &TempTree, dryrun: bool, overwrite: bool) -> Config {
        Config {
            debug: false,
            delete_limit: None,
            no_cache: true,
            dryrun,
            overwrite,
            path: tree.path().to_path_buf(),
            recurse: true,
            verbose: true,
        }
    }

    /// Probe results that give every relative path the same resolution.
    fn results(tree: &TempTree, relative_paths: &[&str], width: u32, height: u32) -> Vec<FFProbeResult> {
        relative_paths
            .iter()
            .map(|relative| FFProbeResult {
                file: tree.join(relative),
                resolution: Resolution::new(width, height),
            })
            .collect()
    }

    #[test]
    fn files_in_nested_directories_are_renamed_in_place() {
        let tree = TempTree::new();
        tree.file_with("movie.mp4", "top");
        tree.file_with("a/movie.mp4", "one level");
        tree.file_with("a/b/c/movie.x265.mkv", "three levels");
        let files = results(&tree, &["movie.mp4", "a/movie.mp4", "a/b/c/movie.x265.mkv"], 1920, 1080);

        let renamed = add_resolution_labels(&config(&tree, false, false), files, None, None);

        assert_eq!(renamed.succeeded, 3);
        assert_eq!(
            tree.snapshot(),
            vec![
                "a/",
                "a/b/",
                "a/b/c/",
                "a/b/c/movie.1080p.x265.mkv: three levels",
                "a/movie.1080p.mp4: one level",
                "movie.1080p.mp4: top",
            ]
        );
    }

    #[test]
    fn a_dryrun_leaves_the_whole_tree_unchanged() {
        let tree = TempTree::new();
        tree.file_with("a/movie.mp4", "one");
        tree.file_with("a/b/clip.1920x1080.mp4", "two");
        let before = tree.snapshot();
        let files = results(&tree, &["a/movie.mp4", "a/b/clip.1920x1080.mp4"], 1920, 1080);

        let renamed = add_resolution_labels(&config(&tree, true, false), files, None, None);

        assert_eq!(renamed.succeeded, 2);
        assert_eq!(tree.snapshot(), before);
    }

    #[test]
    fn two_files_with_the_same_target_keep_both_without_overwrite() {
        let tree = TempTree::new();
        tree.file_with("dir/Movie.mp4", "plain");
        tree.file_with("dir/Movie.1920x1080.mp4", "full resolution");
        let files = results(&tree, &["dir/Movie.mp4", "dir/Movie.1920x1080.mp4"], 1920, 1080);

        let renamed = add_resolution_labels(&config(&tree, false, false), files, None, None);

        assert_eq!(renamed.succeeded, 1);
        assert_eq!(tree.files().len(), 2, "{:?}", tree.snapshot());
    }

    #[test]
    fn two_files_with_the_same_target_keep_both_with_overwrite() {
        let tree = TempTree::new();
        tree.file_with("dir/Movie.mp4", "plain");
        tree.file_with("dir/Movie.1920x1080.mp4", "full resolution");
        let files = results(&tree, &["dir/Movie.mp4", "dir/Movie.1920x1080.mp4"], 1920, 1080);

        let renamed = add_resolution_labels(&config(&tree, false, true), files, None, None);

        assert_eq!(renamed.succeeded, 1);
        let snapshot = tree.snapshot();
        assert_eq!(tree.files().len(), 2, "{snapshot:?}");
        assert!(snapshot.iter().any(|entry| entry.ends_with(": plain")), "{snapshot:?}");
        assert!(
            snapshot.iter().any(|entry| entry.ends_with(": full resolution")),
            "{snapshot:?}"
        );
    }

    #[test]
    fn targets_differing_only_by_case_are_not_both_renamed_onto_each_other() {
        let tree = TempTree::new();
        tree.file_with("dir/movie.mp4", "lower");
        tree.file_with("dir/MOVIE.1920x1080.mp4", "upper");
        let files = results(&tree, &["dir/movie.mp4", "dir/MOVIE.1920x1080.mp4"], 1920, 1080);

        add_resolution_labels(&config(&tree, false, true), files, None, None);

        let snapshot = tree.snapshot();
        assert!(snapshot.iter().any(|entry| entry.ends_with(": lower")), "{snapshot:?}");
        assert!(snapshot.iter().any(|entry| entry.ends_with(": upper")), "{snapshot:?}");
    }

    #[test]
    fn overwrite_replaces_a_file_that_existed_before_the_run() {
        let tree = TempTree::new();
        tree.file_with("dir/movie.mp4", "new");
        tree.file_with("dir/movie.1080p.mp4", "old");
        let files = results(&tree, &["dir/movie.mp4"], 1920, 1080);

        let renamed = add_resolution_labels(&config(&tree, false, true), files, None, None);

        assert_eq!(renamed.succeeded, 1);
        assert_eq!(tree.snapshot(), vec!["dir/", "dir/movie.1080p.mp4: new"]);
    }

    #[test]
    fn skips_locked_file_and_updates_scan_cache_on_rename() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cli-tools.db");
        let busy_file = dir.path().join("busy.mp4");
        let free_file = dir.path().join("free.mp4");
        std::fs::write(&busy_file, b"busy").unwrap();
        std::fs::write(&free_file, b"free").unwrap();

        let mut cache = ScanCache::open_at(&db_path).unwrap();
        let info = VideoInfo {
            size_bytes: Some(4),
            resolution: Some(Resolution::new(1920, 1080)),
            codec: Some("h264".to_string()),
            ..Default::default()
        };
        cache
            .batch_upsert(&[(busy_file.as_path(), &info), (free_file.as_path(), &info)])
            .unwrap();

        let vconvert_lock = ActiveLockStore::open_at(db_path.clone(), "vconvert").unwrap();
        let _guard = vconvert_lock
            .try_acquire(std::slice::from_ref(&busy_file), "convert")
            .unwrap();

        let vres_locks = ActiveLockStore::open_at(db_path, "vres").unwrap();
        let cfg = Config {
            debug: false,
            delete_limit: None,
            no_cache: false,
            dryrun: false,
            overwrite: false,
            path: dir.path().to_path_buf(),
            recurse: false,
            verbose: false,
        };
        let files = vec![
            FFProbeResult {
                file: busy_file.clone(),
                resolution: Resolution::new(1920, 1080),
            },
            FFProbeResult {
                file: free_file.clone(),
                resolution: Resolution::new(1920, 1080),
            },
        ];

        let counts = add_resolution_labels(&cfg, files, Some(&mut cache), Some(&vres_locks));
        assert_eq!(counts.succeeded, 1);
        assert!(busy_file.exists(), "locked file must not be renamed");
        let renamed_free = dir.path().join("free.1080p.mp4");
        assert!(renamed_free.exists());
        let cached = cache.get_all().unwrap();
        assert!(cached.contains_key(renamed_free.to_str().unwrap()));
        assert!(!cached.contains_key(free_file.to_str().unwrap()));
    }
}
