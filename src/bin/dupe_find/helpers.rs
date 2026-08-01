//! Metadata collection and exact-content hash helpers for `dupefind`.
//!
//! This module owns ffprobe cache integration, BLAKE3 hash calculation, file fingerprinting,
//! and cache-aware grouping of files with identical contents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::Context;
#[cfg(not(test))]
use indicatif::ProgressStyle;
use indicatif::{ParallelProgressIterator, ProgressBar};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use cli_tools::dupe_find::{DupeFileInfo, DuplicateGroup};
use cli_tools::scan_cache::{CachedFileHash, ScanCache};
use cli_tools::video_info::VideoInfo;
use cli_tools::{create_semaphore_for_io_bound, print_yellow};

#[cfg(not(test))]
pub const PROGRESS_BAR_CHARS: &str = "=>-";
#[cfg(not(test))]
pub const PROGRESS_BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:80.magenta/blue} {pos}/{len} {percent}%";
#[cfg(not(test))]
pub const SPINNER_TEMPLATE: &str = "[{elapsed_precise}] {spinner:.magenta} {msg} ({pos} files found)";

/// File metadata used to detect whether a cached hash is still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    size_bytes: u64,
    modified_time_ns: i64,
}

/// A file that needs a BLAKE3 hash for content comparison.
#[derive(Debug, Clone)]
struct HashCandidate {
    index: usize,
    path: PathBuf,
    fingerprint: FileFingerprint,
}

/// A newly calculated BLAKE3 hash and its source file index.
#[derive(Debug)]
struct CalculatedFileHash {
    index: usize,
    path: PathBuf,
    cache_entry: CachedFileHash,
}

/// Collect metadata for all files in duplicate groups using ffprobe.
///
/// Checks the shared scan cache first so that files already analysed by
/// `vconvert` or a previous `dupefind` run are not re-probed.
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
    let mut candidates_by_size: HashMap<u64, Vec<HashCandidate>> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        match file_fingerprint(&file.path) {
            Ok(fingerprint) => candidates_by_size
                .entry(fingerprint.size_bytes)
                .or_default()
                .push(HashCandidate {
                    index,
                    path: file.path.clone(),
                    fingerprint,
                }),
            Err(error) => print_yellow!("Could not inspect {} for hashing: {error}", file.path.display()),
        }
    }

    let candidates: Vec<HashCandidate> = candidates_by_size
        .into_values()
        .filter(|same_size| same_size.len() > 1)
        .flatten()
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let cached_entries = hash_cache
        .as_deref()
        .map_or_else(|| Ok(HashMap::new()), ScanCache::get_all_hashes)
        .unwrap_or_else(|error| {
            print_yellow!("Could not read hash cache: {error}");
            HashMap::new()
        });

    let mut hash_values = Vec::with_capacity(candidates.len());
    let mut cache_misses = Vec::new();
    for candidate in candidates {
        let path_key = candidate.path.to_string_lossy();
        if let Some(cached) = cached_entries.get(path_key.as_ref())
            && cached.is_current(candidate.fingerprint.size_bytes, candidate.fingerprint.modified_time_ns)
        {
            hash_values.push((candidate.index, cached.size_bytes, cached.blake3_hash.clone()));
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

    #[cfg(test)]
    let progress_bar = ProgressBar::hidden();
    #[cfg(not(test))]
    let progress_bar = {
        let progress_bar = ProgressBar::new(cache_misses.len() as u64);
        progress_bar.set_style(
            ProgressStyle::default_bar()
                .template(PROGRESS_BAR_TEMPLATE)
                .expect("Failed to set progress bar template")
                .progress_chars(PROGRESS_BAR_CHARS),
        );
        progress_bar.set_message("Hashing files");
        progress_bar
    };

    let calculated_results: Vec<anyhow::Result<CalculatedFileHash>> = cache_misses
        .par_iter()
        .progress_with(progress_bar)
        .map(calculate_file_hash)
        .collect();
    let mut calculated_hashes = Vec::new();
    for result in calculated_results {
        match result {
            Ok(calculated) => calculated_hashes.push(calculated),
            Err(error) => print_yellow!("Could not hash file: {error:#}"),
        }
    }

    if let Some(cache) = hash_cache {
        let entries: Vec<(&Path, &CachedFileHash)> = calculated_hashes
            .iter()
            .map(|calculated| (calculated.path.as_path(), &calculated.cache_entry))
            .collect();
        if let Err(error) = cache.batch_upsert_hashes(&entries) {
            print_yellow!("Failed to write hash cache: {error}");
        }
    }

    hash_values.extend(calculated_hashes.iter().map(|calculated| {
        (
            calculated.index,
            calculated.cache_entry.size_bytes,
            calculated.cache_entry.blake3_hash.clone(),
        )
    }));

    let mut hash_to_indices: HashMap<(u64, String), Vec<usize>> = HashMap::new();
    for (index, size_bytes, hash) in hash_values {
        hash_to_indices.entry((size_bytes, hash)).or_default().push(index);
    }

    hash_to_indices
        .into_values()
        .filter(|indices| indices.len() > 1)
        .collect()
}

/// Read the file metadata used to validate a cached hash.
fn file_fingerprint(path: &Path) -> anyhow::Result<FileFingerprint> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", path.display()))?;
    let modified_time_ns = modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX));

    Ok(FileFingerprint {
        size_bytes: metadata.len(),
        modified_time_ns,
    })
}

/// Calculate a BLAKE3 hash and reject the result if the file changed while it was being read.
fn calculate_file_hash(candidate: &HashCandidate) -> anyhow::Result<CalculatedFileHash> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(&candidate.path)
        .with_context(|| format!("Failed to read {}", candidate.path.display()))?;

    let current_fingerprint = file_fingerprint(&candidate.path)?;
    if current_fingerprint != candidate.fingerprint {
        anyhow::bail!("{} changed while it was being hashed", candidate.path.display());
    }

    Ok(CalculatedFileHash {
        index: candidate.index,
        path: candidate.path.clone(),
        cache_entry: CachedFileHash {
            size_bytes: candidate.fingerprint.size_bytes,
            modified_time_ns: candidate.fingerprint.modified_time_ns,
            blake3_hash: hasher.finalize().to_string(),
        },
    })
}

/// Collect video metadata concurrently using semaphore-limited async tasks.
///
/// Each ffprobe call runs in a blocking task with concurrency controlled
/// by a semaphore sized for I/O-bound work.
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
}
