//! Integration tests for persistent scan and file hash caching.

#![allow(clippy::panic_in_result_fn)]

use cli_tools::Resolution;
use cli_tools::file_hash::{fingerprint_file, hash_file};
use cli_tools::scan_cache::{CachedFileHash, ScanCache};
use cli_tools::video_info::VideoInfo;

#[test]
fn persists_video_metadata_and_hashes_after_reopening() -> anyhow::Result<()> {
    let temp_directory = tempfile::TempDir::new()?;
    let database_path = temp_directory.path().join("nested/cache/vconvert.db");
    let video_path = temp_directory.path().join("sample.mp4");
    std::fs::write(&video_path, b"video contents")?;
    let fingerprint = fingerprint_file(&video_path)?;
    let cached_hash = CachedFileHash {
        size_bytes: fingerprint.size_bytes,
        modified_time_ns: fingerprint.modified_time_ns,
        blake3_hash: hash_file(&video_path)?.to_string(),
    };
    let video_info = VideoInfo {
        size_bytes: Some(fingerprint.size_bytes),
        resolution: Some(Resolution::new(1920, 1080)),
        duration: Some(60.5),
        codec: Some("h264".to_string()),
        bitrate_kbps: Some(4_000),
    };

    {
        let mut cache = ScanCache::open_at(&database_path)?;
        assert_eq!(cache.batch_upsert(&[(&video_path, &video_info)])?, 1);
        assert_eq!(cache.batch_upsert_hashes(&[(&video_path, &cached_hash)])?, 1);
    }

    assert!(database_path.exists());
    let cache = ScanCache::open_at(&database_path)?;
    let path_key = video_path.to_string_lossy();
    let videos = cache.get_all()?;
    let hashes = cache.get_all_hashes()?;

    assert_eq!(
        videos
            .get(path_key.as_ref())
            .expect("video metadata should persist")
            .codec,
        "h264"
    );
    assert_eq!(hashes.get(path_key.as_ref()), Some(&cached_hash));
    assert!(cached_hash.is_current(fingerprint.size_bytes, fingerprint.modified_time_ns));
    Ok(())
}

#[test]
fn empty_hash_batch_writes_nothing_and_upsert_replaces_existing_value() -> anyhow::Result<()> {
    let temp_directory = tempfile::TempDir::new()?;
    let database_path = temp_directory.path().join("cache.db");
    let video_path = temp_directory.path().join("sample.mp4");
    let mut cache = ScanCache::open_at(&database_path)?;
    let original = CachedFileHash {
        size_bytes: 10,
        modified_time_ns: 100,
        blake3_hash: "original".to_string(),
    };
    let replacement = CachedFileHash {
        size_bytes: 20,
        modified_time_ns: 200,
        blake3_hash: "replacement".to_string(),
    };

    assert_eq!(cache.batch_upsert_hashes(&[])?, 0);
    assert_eq!(cache.batch_upsert_hashes(&[(&video_path, &original)])?, 1);
    assert_eq!(cache.batch_upsert_hashes(&[(&video_path, &replacement)])?, 1);

    let path_key = video_path.to_string_lossy();
    let hashes = cache.get_all_hashes()?;
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes.get(path_key.as_ref()), Some(&replacement));
    Ok(())
}

#[test]
fn opening_cache_reports_invalid_parent_directory() -> anyhow::Result<()> {
    let temp_directory = tempfile::TempDir::new()?;
    let parent_file = temp_directory.path().join("not-a-directory");
    std::fs::write(&parent_file, b"file")?;
    let database_path = parent_file.join("cache.db");

    let error = ScanCache::open_at(&database_path)
        .err()
        .expect("file parent should prevent database creation");

    assert!(error.to_string().contains("Failed to create database directory"));
    Ok(())
}
