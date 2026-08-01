//! Metadata collection and exact content hash helpers for `dupefind`.
//!
//! This module owns ffprobe cache integration, BLAKE3 hash calculation, file fingerprinting,
//! and cache aware grouping of files with identical contents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indicatif::ProgressBar;
#[cfg(not(test))]
use indicatif::ProgressStyle;

use cli_tools::dupe_find::hash::{
    CalculatedFileHash, IndexedFileHash, calculate_file_hash_with_progress, collect_hash_candidates, group_hash_matches,
};
use cli_tools::dupe_find::{DupeFileInfo, DuplicateGroup};
use cli_tools::scan_cache::{CachedFileHash, ScanCache};
use cli_tools::video_info::VideoInfo;
use cli_tools::{create_semaphore_for_io_bound, print_yellow};

#[cfg(not(test))]
pub const PROGRESS_BAR_CHARS: &str = "=>-";
#[cfg(not(test))]
pub const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
#[cfg(not(test))]
pub const HASH_PROGRESS_BAR_TEMPLATE: &str =
    "[{elapsed_precise}] {bar:60.magenta/blue} {bytes}/{total_bytes} {percent}% {msg}";
#[cfg(not(test))]
pub const SPINNER_TEMPLATE: &str = "[{elapsed_precise}] {spinner:.magenta} {msg} ({pos} files found)";

/// Collect metadata for all files in duplicate groups using ffprobe.
///
/// Checks the shared scan cache first so that files already analysed by
/// `vconvert` or a previous `dupefind` run are not probed again.
/// Newly probed results are written back to the cache.
pub fn collect_metadata_for_groups(groups: &[DuplicateGroup]) -> HashMap<PathBuf, VideoInfo> {
    let all_files: Vec<PathBuf> = groups
        .iter()
        .flat_map(|group| group.files.iter().map(|file| file.path.clone()))
        .collect();

    if all_files.is_empty() {
        return HashMap::new();
    }

    let scan_cache = match ScanCache::open() {
        Ok(cache) => Some(cache),
        Err(error) => {
            print_yellow!("Could not open scan cache: {error}");
            None
        }
    };

    let cached_entries = scan_cache
        .as_ref()
        .and_then(|cache| cache.get_all().ok())
        .unwrap_or_default();

    let mut metadata: HashMap<PathBuf, VideoInfo> = HashMap::new();
    let mut cache_misses: Vec<PathBuf> = Vec::new();

    for path in &all_files {
        let path_key = path.to_string_lossy();
        let file_size = std::fs::metadata(path).map(|file_metadata| file_metadata.len()).ok();

        if let Some(cached) = cached_entries.get(path_key.as_ref())
            && file_size == Some(cached.size_bytes)
        {
            metadata.insert(path.clone(), cached.to_video_info());
        } else {
            cache_misses.push(path.clone());
        }
    }

    let cache_hit_count = metadata.len();
    if cache_hit_count > 0 {
        println!(
            "Scan cache: {}, {}",
            cli_tools::count_label(cache_hit_count, "hit", "hits"),
            cli_tools::count_label(cache_misses.len(), "miss", "misses")
        );
    }

    if !cache_misses.is_empty() {
        let runtime = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let probed = runtime.block_on(collect_metadata_async(cache_misses));

        if let Some(mut cache) = scan_cache {
            let entries: Vec<(&Path, &VideoInfo)> = probed.iter().map(|(path, info)| (path.as_path(), info)).collect();
            if let Err(error) = cache.batch_upsert(&entries) {
                print_yellow!("Failed to write scan cache: {error}");
            }
        }

        metadata.extend(probed);
    }

    metadata
}

/// Find groups of files with identical BLAKE3 hashes when hash comparison is enabled.
pub fn find_hash_matches(files: &[DupeFileInfo], hash_compare: bool, verbose: bool) -> Vec<Vec<usize>> {
    if !hash_compare {
        return Vec::new();
    }

    let mut hash_cache = match ScanCache::open() {
        Ok(cache) => Some(cache),
        Err(error) => {
            print_yellow!("Could not open hash cache: {error}");
            None
        }
    };

    find_hash_matches_with_cache(files, verbose, hash_cache.as_mut())
}

/// Find hash matches while using the supplied cache when available.
fn find_hash_matches_with_cache(
    files: &[DupeFileInfo],
    verbose: bool,
    hash_cache: Option<&mut ScanCache>,
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
        let path_key = candidate.path.to_string_lossy();
        if let Some(cached) = cached_entries.get(path_key.as_ref())
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
            cli_tools::count_label(cache_hit_count, "hit", "hits"),
            cli_tools::count_label(cache_misses.len(), "miss", "misses")
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

    let total_hash_files = cache_misses.len();
    let calculated_results: Vec<anyhow::Result<CalculatedFileHash>> = cache_misses
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            progress_bar.set_message(format!(
                "[{}/{}] {}",
                index + 1,
                total_hash_files,
                candidate.path.file_name().unwrap_or_default().to_string_lossy()
            ));
            calculate_file_hash_with_progress(candidate, |bytes_read| progress_bar.inc(bytes_read))
        })
        .collect();
    progress_bar.finish_and_clear();
    let mut calculated_hashes = Vec::new();
    for result in calculated_results {
        match result {
            Ok(calculated) => calculated_hashes.push(calculated),
            Err(error) => print_yellow!("Could not hash file: {error:#}"),
        }
    }

    if let Some(cache) = hash_cache {
        let cache_values: Vec<CachedFileHash> = calculated_hashes
            .iter()
            .map(|calculated| CachedFileHash {
                size_bytes: calculated.fingerprint.size_bytes,
                modified_time_ns: calculated.fingerprint.modified_time_ns,
                blake3_hash: calculated.hash.to_string(),
            })
            .collect();
        let entries: Vec<(&Path, &CachedFileHash)> = calculated_hashes
            .iter()
            .zip(&cache_values)
            .map(|(calculated, cached)| (calculated.path.as_path(), cached))
            .collect();
        if let Err(error) = cache.batch_upsert_hashes(&entries) {
            print_yellow!("Failed to write hash cache: {error}");
        }
    }

    hash_values.extend(calculated_hashes.iter().map(CalculatedFileHash::indexed_hash));
    group_hash_matches(&hash_values)
}

/// Collect video metadata concurrently using async tasks limited by a semaphore.
///
/// Each ffprobe call runs in a blocking task with concurrency controlled
/// by a semaphore sized for input and output work.
async fn collect_metadata_async(files: Vec<PathBuf>) -> HashMap<PathBuf, VideoInfo> {
    let semaphore = create_semaphore_for_io_bound();

    #[cfg(not(test))]
    let progress_bar = {
        let progress_bar = ProgressBar::new(files.len() as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );
        Arc::new(progress_bar)
    };
    #[cfg(test)]
    let progress_bar = Arc::new(ProgressBar::hidden());

    let tasks: Vec<_> = files
        .into_iter()
        .map(|path| {
            let semaphore = Arc::clone(&semaphore);
            let progress = Arc::clone(&progress_bar);
            tokio::spawn(async move {
                let permit = semaphore.acquire().await.expect("Failed to acquire semaphore");
                let result = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || VideoInfo::from_path(&path)
                })
                .await
                .expect("spawn_blocking task failed");
                drop(permit);
                progress.inc(1);
                (path, result)
            })
        })
        .collect();

    let metadata = futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter_map(|(path, result)| match result {
            Ok(info) => Some((path, info)),
            Err(error) => {
                eprintln!("Error: {error}");
                None
            }
        })
        .collect();

    progress_bar.finish_and_clear();
    metadata
}

#[cfg(test)]
mod test_video_info_resolution_string {
    use cli_tools::video_info::Resolution;

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
            cli_tools::file_hash::fingerprint_file(&second_path).expect("should fingerprint changed file");
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

        assert!(matches.is_empty());
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
        let fingerprint = cli_tools::file_hash::fingerprint_file(&second_path).expect("should fingerprint second file");
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
}
