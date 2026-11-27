//! Metadata collection and exact content hash helpers for `dupefind`.
//!
//! This module owns ffprobe cache integration, BLAKE3 hash calculation, file fingerprinting,
//! and cache aware grouping of files with identical contents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use colored::Colorize;
use indicatif::ProgressBar;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use rayon::prelude::*;

use cli_tools_common::print_yellow;
use cli_tools_dupefind::hash::{
    CalculatedFileHash, IndexedFileHash, calculate_file_hash_with_progress, collect_hash_candidates, group_hash_matches,
};
use cli_tools_dupefind::{DupeFileInfo, DuplicateGroup};
use cli_tools_media::file_hash::{FileFingerprint, fingerprint_file};
use cli_tools_media::scan_cache::{CachedFileHash, ScanCache};
use cli_tools_media::video_info::VideoInfo;

/// Flush newly computed hashes to `cli-tools.db` every this many files so interrupted runs keep progress.
const HASH_CHECKPOINT_INTERVAL: usize = 16;

#[cfg(not(test))]
pub const PROGRESS_BAR_CHARS: &str = "=>-";
#[cfg(not(test))]
pub const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
#[cfg(not(test))]
pub const HASH_PROGRESS_BAR_TEMPLATE: &str =
    "[{elapsed_precise}] {bar:60.magenta/blue} {bytes}/{total_bytes} {percent}% {msg}";
#[cfg(not(test))]
pub const SPINNER_TEMPLATE: &str = "[{elapsed_precise}] {spinner:.magenta} {msg} ({pos} files found)";

/// Cache-miss video file queued for parallel metadata probing.
struct ProbeCandidate {
    /// Path to the video file.
    path: PathBuf,
    /// File fingerprint captured before probing, when available.
    fingerprint: Option<FileFingerprint>,
}

/// Probed video metadata paired with its path and optional cache fingerprint.
struct ProbedMetadata {
    /// Path to the video file.
    path: PathBuf,
    /// Probed video metadata.
    info: VideoInfo,
    /// File fingerprint captured before probing, when available.
    fingerprint: Option<FileFingerprint>,
}

/// Remove stale scan and file-hash cache entries whose files no longer exist on mounted volumes.
pub fn clean_cache(verbose: bool) -> Result<()> {
    let mut cache = ScanCache::open()?;
    let stats = cache.cache_stats()?;
    if verbose {
        println!(
            "{}",
            format!(
                "Database: {}",
                cli_tools_db::sqlite::database_path()
                    .map_or_else(|_| "cli-tools.db".to_string(), |path| path.display().to_string())
            )
            .bold()
        );
    }
    if stats.scanned_files_count == 0 && stats.file_hashes_count == 0 {
        println!("Cache is empty");
        return Ok(());
    }
    println!(
        "Checking {} cached scan entries and {} file hashes...",
        stats.scanned_files_count, stats.file_hashes_count
    );
    let removed_scans = cache.remove_missing_scanned_files()?;
    let removed_hashes = cache.remove_missing_file_hashes()?;
    if removed_scans > 0 || removed_hashes > 0 {
        println!(
            "{}",
            format!("Removed {removed_scans} stale scan entries and {removed_hashes} stale hash entries").green()
        );
    } else {
        println!("{}", "All cached entries are up to date".green());
    }
    Ok(())
}

/// Collect metadata for all files in duplicate groups using the shared scan cache and parallel ffprobe.
pub fn collect_metadata_for_groups(groups: &[DuplicateGroup], use_cache: bool) -> HashMap<PathBuf, VideoInfo> {
    if groups.iter().all(|group| group.files.is_empty()) {
        return HashMap::new();
    }

    let scan_cache = if use_cache {
        match ScanCache::open() {
            Ok(cache) => Some(cache),
            Err(error) => {
                print_yellow!("Could not open scan cache: {error}");
                None
            }
        }
    } else {
        None
    };

    collect_metadata_for_groups_with_cache(groups, scan_cache)
}

/// Collect metadata for the grouped files, reading and updating the supplied cache when available.
fn collect_metadata_for_groups_with_cache(
    groups: &[DuplicateGroup],
    scan_cache: Option<ScanCache>,
) -> HashMap<PathBuf, VideoInfo> {
    let all_files: Vec<PathBuf> = groups
        .iter()
        .flat_map(|group| group.files.iter().map(|file| file.path.clone()))
        .collect();

    if all_files.is_empty() {
        return HashMap::new();
    }

    let cached_entries = scan_cache
        .as_ref()
        .and_then(|cache| cache.get_all().ok())
        .unwrap_or_default();

    let mut metadata: HashMap<PathBuf, VideoInfo> = HashMap::new();
    let mut cache_misses: Vec<ProbeCandidate> = Vec::new();

    for path in &all_files {
        let fingerprint = fingerprint_file(path).ok();
        if let Some(path_key) = path.to_str()
            && let Some(cached) = cached_entries.get(path_key)
            && fingerprint.is_some_and(|fingerprint| cached.is_current(&fingerprint))
        {
            metadata.insert(path.clone(), cached.to_video_info());
        } else {
            cache_misses.push(ProbeCandidate {
                path: path.clone(),
                fingerprint,
            });
        }
    }

    let cache_hit_count = metadata.len();
    if cache_hit_count > 0 {
        println!(
            "Scan cache: {}, {}",
            cli_tools_common::count_label(cache_hit_count, "hit", "hits"),
            cli_tools_common::count_label(cache_misses.len(), "miss", "misses")
        );
    }

    if !cache_misses.is_empty() {
        let probed = collect_metadata_parallel(cache_misses);

        if let Some(mut cache) = scan_cache {
            let entries: Vec<(&Path, &VideoInfo, FileFingerprint)> = probed
                .iter()
                .filter_map(|item| {
                    item.fingerprint
                        .map(|fingerprint| (item.path.as_path(), &item.info, fingerprint))
                })
                .collect();
            if let Err(error) = cache.batch_upsert_with_fingerprints(&entries) {
                print_yellow!("Failed to write scan cache: {error}");
            }
        }

        for item in probed {
            metadata.insert(item.path, item.info);
        }
    }

    metadata
}

/// Find groups of files with identical BLAKE3 hashes when hash comparison is enabled.
pub fn find_hash_matches(
    files: &[DupeFileInfo],
    hash_compare: bool,
    use_cache: bool,
    verbose: bool,
) -> Vec<Vec<usize>> {
    if !hash_compare {
        return Vec::new();
    }

    let mut hash_cache = if use_cache {
        match ScanCache::open() {
            Ok(cache) => Some(cache),
            Err(error) => {
                print_yellow!("Could not open hash cache: {error}");
                None
            }
        }
    } else {
        None
    };

    find_hash_matches_with_cache(files, verbose, hash_cache.as_mut())
}

/// Find hash matches while incrementally checkpointing computed hashes to `hash_cache`.
fn find_hash_matches_with_cache(
    files: &[DupeFileInfo],
    verbose: bool,
    mut hash_cache: Option<&mut ScanCache>,
) -> Vec<Vec<usize>> {
    let collection = collect_hash_candidates(files);
    for (path, error) in collection.errors {
        print_yellow!("Could not inspect {} for hashing: {error}", path.display());
    }
    if collection.candidates.is_empty() {
        return Vec::new();
    }

    let cached_entries = hash_cache
        .as_deref()
        .map_or_else(|| Ok(HashMap::new()), ScanCache::get_all_hashes)
        .unwrap_or_else(|error| {
            print_yellow!("Could not read hash cache: {error}");
            HashMap::new()
        });

    let mut hash_values = Vec::with_capacity(collection.candidates.len());
    let mut cache_misses = Vec::new();
    for candidate in collection.candidates {
        // Like cache writes, lookups require a lossless path key.
        if let Some(path_key) = candidate.path.to_str()
            && let Some(cached) = cached_entries.get(path_key)
            && cached.is_current(candidate.fingerprint.size_bytes, candidate.fingerprint.modified_time_ns)
        {
            hash_values.push(IndexedFileHash {
                index: candidate.index,
                size_bytes: cached.size_bytes,
                blake3_hash: cached.blake3_hash.clone(),
            });
        } else {
            cache_misses.push(candidate);
        }
    }

    let cache_hit_count = hash_values.len();
    if verbose {
        println!(
            "Hash cache: {}, {}",
            cli_tools_common::count_label(cache_hit_count, "hit", "hits"),
            cli_tools_common::count_label(cache_misses.len(), "miss", "misses")
        );
    }

    #[cfg(not(test))]
    let total_hash_bytes: u64 = cache_misses
        .iter()
        .map(|candidate| candidate.fingerprint.size_bytes)
        .sum();
    #[cfg(test)]
    let progress_bar = ProgressBar::hidden();
    #[cfg(not(test))]
    let progress_bar = {
        let progress_bar = ProgressBar::new(total_hash_bytes);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(HASH_PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set hash progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );
        progress_bar
    };

    let abort_flag = Arc::new(AtomicBool::new(false));
    #[cfg(not(test))]
    {
        let handler_flag = Arc::clone(&abort_flag);
        let _ = ctrlc::set_handler(move || {
            handler_flag.store(true, Ordering::SeqCst);
        });
    }

    let total_hash_files = cache_misses.len();
    let mut pending_checkpoint: Vec<CalculatedFileHash> = Vec::with_capacity(HASH_CHECKPOINT_INTERVAL);
    for (index, candidate) in cache_misses.iter().enumerate() {
        if abort_flag.load(Ordering::SeqCst) {
            print_yellow!("Interrupted while hashing; flushing computed hashes to cache...");
            break;
        }
        progress_bar.set_message(format!(
            "[{}/{}] {}",
            index + 1,
            total_hash_files,
            candidate.path.file_name().unwrap_or_default().to_string_lossy()
        ));
        match calculate_file_hash_with_progress(candidate, |bytes_read| progress_bar.inc(bytes_read)) {
            Ok(calculated) => {
                hash_values.push(calculated.indexed_hash());
                pending_checkpoint.push(calculated);
                if pending_checkpoint.len() >= HASH_CHECKPOINT_INTERVAL {
                    flush_hash_checkpoint(hash_cache.as_deref_mut(), &mut pending_checkpoint);
                }
            }
            Err(error) => print_yellow!("Could not hash file: {error:#}"),
        }
    }
    progress_bar.finish_and_clear();
    flush_hash_checkpoint(hash_cache, &mut pending_checkpoint);

    group_hash_matches(&hash_values)
}

/// Flush accumulated calculated hashes into the shared `ScanCache` and clear the buffer.
fn flush_hash_checkpoint(hash_cache: Option<&mut ScanCache>, buffer: &mut Vec<CalculatedFileHash>) {
    if buffer.is_empty() {
        return;
    }
    if let Some(cache) = hash_cache {
        let cache_values: Vec<CachedFileHash> = buffer
            .iter()
            .map(|calculated| CachedFileHash {
                size_bytes: calculated.fingerprint.size_bytes,
                modified_time_ns: calculated.fingerprint.modified_time_ns,
                blake3_hash: calculated.hash.to_string(),
            })
            .collect();
        let entries: Vec<(&Path, &CachedFileHash)> = buffer
            .iter()
            .zip(&cache_values)
            .map(|(calculated, cached)| (calculated.path.as_path(), cached))
            .collect();
        if let Err(error) = cache.batch_upsert_hashes(&entries) {
            print_yellow!("Failed to write hash cache: {error}");
        }
    }
    buffer.clear();
}

/// Collect video metadata in parallel using Rayon.
fn collect_metadata_parallel(files: Vec<ProbeCandidate>) -> Vec<ProbedMetadata> {
    #[cfg(not(test))]
    let progress_bar = {
        let progress_bar = ProgressBar::new(files.len() as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );
        progress_bar
    };
    #[cfg(test)]
    let progress_bar = ProgressBar::hidden();

    let results: Vec<ProbedMetadata> = files
        .into_par_iter()
        .filter_map(|candidate| {
            let result = VideoInfo::from_path(&candidate.path);
            progress_bar.inc(1);
            match result {
                Ok(info) => Some(ProbedMetadata {
                    path: candidate.path,
                    info,
                    fingerprint: candidate.fingerprint,
                }),
                Err(error) => {
                    eprintln!("Error: {error}");
                    None
                }
            }
        })
        .collect();

    progress_bar.finish_and_clear();
    results
}

#[cfg(test)]
mod test_video_info_resolution_string {
    use cli_tools_media::video_info::Resolution;

    use super::*;

    #[test]
    fn formats_resolution() {
        let info = VideoInfo {
            size_bytes: Some(1000),
            duration: Some(60.0),
            resolution: Some(Resolution::new(1920, 1080)),
            codec: Some("h264".to_string()),
            ..Default::default()
        };
        assert_eq!(info.resolution_string(), Some("1920x1080".to_string()));
    }

    #[test]
    fn returns_none_when_resolution_missing() {
        let info = VideoInfo {
            size_bytes: Some(1000),
            duration: Some(60.0),
            codec: Some("h264".to_string()),
            ..Default::default()
        };
        assert_eq!(info.resolution_string(), None);
    }
}

#[cfg(test)]
mod test_hash_matches {
    use std::fs;

    use super::*;

    #[test]
    fn finds_identical_files_with_different_filenames_and_reuses_cache() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("completely.different.name.mp4");
        let unique_path = temp_directory.path().join("unique.mp4");
        fs::write(&first_path, b"same bytes").expect("should write first file");
        fs::write(&second_path, b"same bytes").expect("should write second file");
        fs::write(&unique_path, b"different!").expect("should write unique file");

        let files = vec![
            DupeFileInfo::new(first_path, "mp4".to_string()),
            DupeFileInfo::new(second_path, "mp4".to_string()),
            DupeFileInfo::new(unique_path, "mp4".to_string()),
        ];
        let database_path = temp_directory.path().join("hash-cache.db");
        let mut cache = ScanCache::open_at(&database_path).expect("should open hash cache");

        let matches = find_hash_matches_with_cache(&files, false, Some(&mut cache));

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], vec![0, 1]);
        assert_eq!(cache.get_all_hashes().expect("should read hashes").len(), 3);

        let cached_matches = find_hash_matches_with_cache(&files, false, Some(&mut cache));
        assert_eq!(cached_matches, matches);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_does_not_use_the_literal_replacement_characters_cached_hash() {
        let tree = cli_tools_common::test_support::TempTree::new();
        let Some(raw_path) = tree.non_utf8_file("videos", "clip", "mp4") else {
            return;
        };
        fs::write(&raw_path, b"RAW").unwrap();
        let replacement_path = tree.file_with("videos/clip\u{fffd}.mp4", b"UTF");
        let matching_path = tree.file_with("videos/matching.mp4", b"RAW");
        assert!(raw_path.to_str().is_none());
        assert_eq!(raw_path.to_string_lossy(), replacement_path.to_str().unwrap());

        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for path in [&raw_path, &replacement_path] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }
        let fingerprint = fingerprint_file(&replacement_path).unwrap();
        assert_eq!(fingerprint_file(&raw_path).unwrap(), fingerprint);
        let replacement_hash = CachedFileHash {
            size_bytes: fingerprint.size_bytes,
            modified_time_ns: fingerprint.modified_time_ns,
            blake3_hash: blake3::hash(b"UTF").to_string(),
        };
        let mut cache = ScanCache::open_at(&tree.join("hash-cache.db")).unwrap();
        assert_eq!(
            cache
                .batch_upsert_hashes(&[(&replacement_path, &replacement_hash)])
                .unwrap(),
            1
        );
        let files = [
            DupeFileInfo::new(raw_path, "mp4".to_string()),
            DupeFileInfo::new(replacement_path, "mp4".to_string()),
            DupeFileInfo::new(matching_path, "mp4".to_string()),
        ];

        let matches = find_hash_matches_with_cache(&files, false, Some(&mut cache));

        assert_eq!(matches, vec![vec![0, 2]]);
    }

    #[test]
    fn replaces_stale_cached_hash_after_file_changes() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("second.mp4");
        fs::write(&first_path, b"same bytes").expect("should write first file");
        fs::write(&second_path, b"same bytes").expect("should write second file");
        let files = vec![
            DupeFileInfo::new(first_path, "mp4".to_string()),
            DupeFileInfo::new(second_path.clone(), "mp4".to_string()),
        ];
        let database_path = temp_directory.path().join("hash-cache.db");
        let mut cache = ScanCache::open_at(&database_path).expect("should open hash cache");
        assert_eq!(
            find_hash_matches_with_cache(&files, false, Some(&mut cache)),
            vec![vec![0, 1]]
        );

        fs::write(&second_path, b"different!").expect("should change second file");
        let current_fingerprint =
            cli_tools_media::file_hash::fingerprint_file(&second_path).expect("should fingerprint changed file");
        let stale_hash = CachedFileHash {
            size_bytes: current_fingerprint.size_bytes,
            modified_time_ns: current_fingerprint.modified_time_ns.saturating_sub(1),
            blake3_hash: blake3::hash(b"same bytes").to_string(),
        };
        cache
            .batch_upsert_hashes(&[(&second_path, &stale_hash)])
            .expect("should write stale hash entry");

        let matches = find_hash_matches_with_cache(&files, false, Some(&mut cache));
        let cached_entries = cache.get_all_hashes().expect("should read refreshed hashes");
        let refreshed = cached_entries
            .get(second_path.to_string_lossy().as_ref())
            .expect("expected refreshed second file hash");

        assert_eq!(matches, [] as [Vec<usize>; 0]);
        assert_eq!(refreshed.blake3_hash, blake3::hash(b"different!").to_string());
        assert_eq!(refreshed.modified_time_ns, current_fingerprint.modified_time_ns);
    }

    #[test]
    fn preserves_file_order_with_mixed_cache_hits_and_misses() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("second.mp4");
        fs::write(&first_path, b"same bytes").expect("should write first file");
        fs::write(&second_path, b"same bytes").expect("should write second file");
        let files = vec![
            DupeFileInfo::new(first_path, "mp4".to_string()),
            DupeFileInfo::new(second_path.clone(), "mp4".to_string()),
        ];
        let database_path = temp_directory.path().join("hash-cache.db");
        let mut cache = ScanCache::open_at(&database_path).expect("should open hash cache");
        let fingerprint =
            cli_tools_media::file_hash::fingerprint_file(&second_path).expect("should fingerprint second file");
        let cached_hash = CachedFileHash {
            size_bytes: fingerprint.size_bytes,
            modified_time_ns: fingerprint.modified_time_ns,
            blake3_hash: blake3::hash(b"same bytes").to_string(),
        };
        cache
            .batch_upsert_hashes(&[(&second_path, &cached_hash)])
            .expect("should cache second file hash");

        let matches = find_hash_matches_with_cache(&files, false, Some(&mut cache));

        assert_eq!(matches, vec![vec![0, 1]]);
    }

    #[test]
    fn disabled_hash_comparison_finds_nothing() {
        let files = vec![DupeFileInfo::new(PathBuf::from("missing.mp4"), "mp4".to_string())];

        assert_eq!(find_hash_matches(&files, false, false, true), [] as [Vec<usize>; 0]);
    }

    #[test]
    fn verbose_hashing_without_cache_still_groups_identical_files() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("second.mp4");
        fs::write(&first_path, b"same bytes").expect("should write first file");
        fs::write(&second_path, b"same bytes").expect("should write second file");
        let files = vec![
            DupeFileInfo::new(first_path, "mp4".to_string()),
            DupeFileInfo::new(second_path, "mp4".to_string()),
        ];

        assert_eq!(find_hash_matches_with_cache(&files, true, None), vec![vec![0, 1]]);
    }

    #[test]
    fn missing_files_are_reported_and_skipped() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let files = vec![
            DupeFileInfo::new(temp_directory.path().join("missing.mp4"), "mp4".to_string()),
            DupeFileInfo::new(temp_directory.path().join("also-missing.mp4"), "mp4".to_string()),
        ];

        assert_eq!(find_hash_matches_with_cache(&files, true, None), [] as [Vec<usize>; 0]);
    }
}

#[cfg(test)]
mod test_collect_metadata {
    use std::fs;

    use cli_tools_media::video_info::Resolution;

    use super::*;

    fn cached_info(size_bytes: u64) -> VideoInfo {
        VideoInfo {
            size_bytes: Some(size_bytes),
            duration: Some(90.0),
            resolution: Some(Resolution::new(1920, 1080)),
            codec: Some("hevc".to_string()),
            bitrate_kbps: Some(4_000),
            ..Default::default()
        }
    }

    #[test]
    fn no_groups_collect_nothing() {
        assert!(collect_metadata_for_groups(&[], false).is_empty());
        assert!(collect_metadata_for_groups_with_cache(&[], None).is_empty());
    }

    #[test]
    fn cached_entries_matching_the_file_size_are_used_without_probing() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let first_path = temp_directory.path().join("first.mp4");
        let second_path = temp_directory.path().join("second.mp4");
        fs::write(&first_path, b"first video").expect("should write first file");
        fs::write(&second_path, b"second video").expect("should write second file");
        let first_info = cached_info(11);
        let second_info = cached_info(12);
        let mut cache =
            ScanCache::open_at(&temp_directory.path().join("scan-cache.db")).expect("should open scan cache");
        cache
            .batch_upsert(&[(&first_path, &first_info), (&second_path, &second_info)])
            .expect("should write cache entries");
        let group = DuplicateGroup::new(
            "video".to_string(),
            vec![
                DupeFileInfo::new(first_path.clone(), "mp4".to_string()),
                DupeFileInfo::new(second_path.clone(), "mp4".to_string()),
            ],
        );

        let metadata = collect_metadata_for_groups_with_cache(&[group], Some(cache));

        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[&first_path].codec.as_deref(), Some("hevc"));
        assert_eq!(metadata[&second_path].size_bytes, Some(12));
    }

    #[test]
    fn cached_entry_with_a_different_size_is_not_used() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let path = temp_directory.path().join("not-a-video.mp4");
        fs::write(&path, b"not a video").expect("should write file");
        let stale_info = cached_info(1);
        let mut cache =
            ScanCache::open_at(&temp_directory.path().join("scan-cache.db")).expect("should open scan cache");
        cache
            .batch_upsert(&[(&path, &stale_info)])
            .expect("should write stale cache entry");
        let group = DuplicateGroup::new(
            "video".to_string(),
            vec![DupeFileInfo::new(path.clone(), "mp4".to_string())],
        );

        let metadata = collect_metadata_for_groups_with_cache(&[group], Some(cache));

        // Without ffprobe the file is left out, with it the file is probed again
        assert!(
            metadata
                .get(&path)
                .is_none_or(|info| info.codec.as_deref() != Some("hevc") && info.size_bytes == Some(11)),
            "a size mismatch should not use the cached entry"
        );
    }
}
