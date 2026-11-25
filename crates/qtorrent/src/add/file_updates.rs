//! File level updates for a torrent that was just added to qBittorrent.
//!
//! Renames the files of a multi-file torrent with dot formatting,
//! and marks excluded files to be skipped, retrying until qBittorrent confirms the change.
//! Each outcome is reported to the `AddObserver`.

use std::path::Path;
use std::time::Duration;

use cli_tools_dots::DotFormat;

use super::{AddEvent, AddObserver};
use crate::qbittorrent::{QBittorrentClient, TorrentFileItem};
use crate::utils;

/// Number of attempts to rename the files that failed to rename.
const FILE_RENAME_ATTEMPTS: usize = 3;

/// Delay before each retry of the file renames.
const FILE_RENAME_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Priority that tells qBittorrent not to download a file.
const EXCLUDED_PRIORITY: u8 = 0;

/// Delays before each attempt to set the excluded file priorities, in milliseconds.
const PRIORITY_RETRY_DELAYS_MS: [u64; 5] = [500, 750, 1000, 1500, 2000];

/// Rename individual files within a multi-file torrent using dot formatting.
///
/// Queries the qBittorrent API for the actual file paths
/// (which reflect any folder renames that have already been applied),
/// then renames every file with dot formatting.
/// Retries with a fresh file list if renames fail
/// (paths can change asynchronously after a folder rename propagates).
pub(super) async fn rename_torrent_files(
    client: &QBittorrentClient,
    info_hash: &str,
    dot_rename: &DotFormat<'_>,
    observer: &mut impl AddObserver,
) {
    observer.on_event(AddEvent::RenamingFiles);
    let mut total_renamed: usize = 0;
    // Indices that still need renaming, `None` before the first attempt
    let mut pending_indices: Option<Vec<usize>> = None;

    for attempt in 0..FILE_RENAME_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(FILE_RENAME_RETRY_DELAY).await;
        }

        let api_files = match client.get_torrent_files(info_hash).await {
            Ok(files) => files,
            Err(error) => {
                observer.on_event(AddEvent::FileListFailed(&error));
                return;
            }
        };

        let mut failed_indices: Vec<usize> = Vec::new();

        for file in &api_files {
            // On retries, only process previously failed indices
            if let Some(ref pending) = pending_indices
                && !pending.contains(&file.index)
            {
                continue;
            }

            // The API returns the full path as qBittorrent sees it, including the root folder
            let old_path = &file.name;
            let path = Path::new(old_path.as_str());

            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };

            let formatted = utils::format_single_file_name(dot_rename, filename);
            if formatted == filename {
                continue;
            }

            // Build the new path by replacing only the file name
            let parent = path.parent().and_then(|parent| parent.to_str()).unwrap_or("");
            let new_path = if parent.is_empty() {
                formatted.clone()
            } else {
                format!("{parent}/{formatted}")
            };

            match client.rename_file(info_hash, old_path, &new_path).await {
                Ok(()) => {
                    total_renamed += 1;
                    observer.on_event(AddEvent::FileRenamed {
                        old_name: filename,
                        new_name: &formatted,
                    });
                }
                Err(error) => {
                    failed_indices.push(file.index);
                    if attempt == FILE_RENAME_ATTEMPTS - 1 {
                        observer.on_event(AddEvent::FileRenameFailed {
                            name: filename,
                            error: &error,
                        });
                    }
                }
            }
        }

        pending_indices = Some(failed_indices);
        if pending_indices.as_ref().is_some_and(Vec::is_empty) {
            break;
        }
    }

    if total_renamed > 0 {
        observer.on_event(AddEvent::FilesRenamed(total_renamed));
    }
    if let Some(ref pending) = pending_indices
        && !pending.is_empty()
    {
        observer.on_event(AddEvent::FileRenamesIncomplete(pending.len()));
    }
}

/// Set file priorities to skip excluded files in qBittorrent.
///
/// Retries with increasing delays until the API confirms the priorities are applied.
/// If `wait_for_metadata` is true, waits before the first attempt for the torrent to load.
pub(super) async fn apply_excluded_file_priorities(
    client: &QBittorrentClient,
    info_hash: &str,
    excluded_indices: &[usize],
    wait_for_metadata: bool,
    observer: &mut impl AddObserver,
) {
    if wait_for_metadata {
        tokio::time::sleep(Duration::from_millis(PRIORITY_RETRY_DELAYS_MS[0])).await;
    }

    let mut last_error: Option<String> = None;

    for (attempt, delay_ms) in PRIORITY_RETRY_DELAYS_MS.iter().enumerate() {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }

        let api_files = match client.get_torrent_files(info_hash).await {
            Ok(files) => files,
            Err(error) => {
                last_error = Some(format!("Could not read torrent files yet: {error}"));
                continue;
            }
        };

        if !excluded_indices_exist(&api_files, excluded_indices) {
            last_error = Some("Torrent metadata is not fully available yet".to_string());
            continue;
        }

        if let Err(error) = client
            .set_file_priorities(info_hash, excluded_indices, EXCLUDED_PRIORITY)
            .await
        {
            last_error = Some(format!("Could not set file priorities: {error}"));
            continue;
        }

        let updated_files = match client.get_torrent_files(info_hash).await {
            Ok(files) => files,
            Err(error) => {
                last_error = Some(format!("Could not verify file priorities: {error}"));
                continue;
            }
        };

        if excluded_indices_have_priority(&updated_files, excluded_indices, EXCLUDED_PRIORITY) {
            observer.on_event(AddEvent::ExcludedFilesSkipped(excluded_indices.len()));
            return;
        }

        last_error = Some("qBittorrent has not applied the skip priorities yet".to_string());
    }

    observer.on_event(AddEvent::ExcludedFilesSkipFailed {
        count: excluded_indices.len(),
        error: last_error.as_deref(),
    });
}

/// Check that all excluded file indices are present in the API response.
fn excluded_indices_exist(api_files: &[TorrentFileItem], excluded_indices: &[usize]) -> bool {
    excluded_indices
        .iter()
        .all(|index| api_files.iter().any(|file| file.index == *index))
}

/// Check that all excluded file indices have the expected priority value.
fn excluded_indices_have_priority(api_files: &[TorrentFileItem], excluded_indices: &[usize], priority: u8) -> bool {
    excluded_indices.iter().all(|index| {
        api_files
            .iter()
            .find(|file| file.index == *index)
            .is_some_and(|file| file.priority == priority)
    })
}

#[cfg(test)]
mod test_excluded_file_priorities {
    use super::*;

    fn make_torrent_file(index: usize, priority: u8) -> TorrentFileItem {
        TorrentFileItem {
            index,
            name: format!("file-{index}.bin"),
            priority,
        }
    }

    #[test]
    fn empty_excluded_indices_always_exist() {
        let api_files = vec![make_torrent_file(0, 1), make_torrent_file(1, 1)];
        assert!(excluded_indices_exist(&api_files, &[]));
    }

    #[test]
    fn empty_excluded_indices_always_have_priority() {
        let api_files = vec![make_torrent_file(0, 1), make_torrent_file(1, 1)];
        assert!(excluded_indices_have_priority(&api_files, &[], 0));
    }

    #[test]
    fn empty_api_files_means_no_indices_exist() {
        assert!(!excluded_indices_exist(&[], &[0, 1]));
    }

    #[test]
    fn single_excluded_index_present() {
        let api_files = vec![make_torrent_file(0, 1), make_torrent_file(1, 1)];
        assert!(excluded_indices_exist(&api_files, &[1]));
    }

    #[test]
    fn single_excluded_index_missing() {
        let api_files = vec![make_torrent_file(0, 1), make_torrent_file(1, 1)];
        assert!(!excluded_indices_exist(&api_files, &[5]));
    }

    #[test]
    fn all_excluded_indices_present_with_non_contiguous_ids() {
        let api_files = vec![
            make_torrent_file(0, 1),
            make_torrent_file(2, 1),
            make_torrent_file(4, 1),
            make_torrent_file(7, 1),
        ];

        assert!(excluded_indices_exist(&api_files, &[0, 4, 7]));
    }

    #[test]
    fn returns_false_when_any_excluded_index_missing() {
        let api_files = vec![make_torrent_file(0, 1), make_torrent_file(2, 1)];

        assert!(!excluded_indices_exist(&api_files, &[0, 4]));
    }

    #[test]
    fn all_excluded_indices_have_matching_priority() {
        let api_files = vec![
            make_torrent_file(0, 0),
            make_torrent_file(1, 1),
            make_torrent_file(2, 0),
            make_torrent_file(3, 1),
            make_torrent_file(4, 0),
        ];

        assert!(excluded_indices_have_priority(&api_files, &[0, 2, 4], 0));
    }

    #[test]
    fn priority_check_fails_when_one_index_has_wrong_priority() {
        let api_files = vec![
            make_torrent_file(0, 0),
            make_torrent_file(2, 1),
            make_torrent_file(4, 0),
        ];

        // Index 2 has priority 1, but we check for priority 0
        assert!(!excluded_indices_have_priority(&api_files, &[0, 2, 4], 0));
    }

    #[test]
    fn priority_check_fails_when_index_does_not_exist() {
        let api_files = vec![make_torrent_file(0, 0), make_torrent_file(1, 0)];

        // Index 5 doesn't exist, so find returns None and is_some_and returns false
        assert!(!excluded_indices_have_priority(&api_files, &[0, 5], 0));
    }

    #[test]
    fn priority_check_with_single_file() {
        let api_files = vec![make_torrent_file(3, 0)];

        assert!(excluded_indices_have_priority(&api_files, &[3], 0));
        assert!(!excluded_indices_have_priority(&api_files, &[3], 1));
    }
}
