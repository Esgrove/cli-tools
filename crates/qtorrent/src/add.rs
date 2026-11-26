//! Adding parsed torrents to qBittorrent.
//!
//! Parses the torrent files, finds torrents that already exist in qBittorrent,
//! adds new torrents with their output name, and renames existing torrents.
//! Every step after the add request is reported to an `AddObserver`, so callers decide what to show.
//! The file level updates after adding live in `file_updates`.

mod file_updates;

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::naming::NameSuggester;
use crate::qbittorrent::{AddTorrentParams, QBittorrentClient, TorrentListItem};
use crate::tags::TagRules;
use crate::torrent::{FileFilter, parse_torrent};
use crate::utils::TorrentInfo;

/// Delays before each attempt to rename the content of a just added torrent on disk.
/// qBittorrent needs time to fully register the torrent.
const DISK_RENAME_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1000),
];

/// Delay before renaming files when no folder rename gave the torrent time to load.
const FILE_RENAME_DELAY: Duration = Duration::from_millis(500);

/// One step of adding a torrent or renaming an existing one.
#[derive(Debug)]
pub enum AddEvent<'a> {
    /// The torrent was added to qBittorrent.
    Added {
        /// Output folder or file name.
        name: &'a str,
        /// Whether the name is a folder name.
        is_multi_file: bool,
    },
    /// The content of the added torrent was renamed on disk.
    RenamedOnDisk {
        /// Name in the torrent metadata.
        old_name: &'a str,
        /// New name on disk.
        new_name: &'a str,
    },
    /// Renaming the content of the added torrent on disk failed after all retries.
    RenameOnDiskFailed {
        /// Name in the torrent metadata.
        old_name: &'a str,
        /// Requested name on disk.
        new_name: &'a str,
        /// Error from the last attempt.
        error: Option<&'a anyhow::Error>,
    },
    /// An existing torrent was renamed in qBittorrent.
    ExistingRenamed {
        /// Name before the rename.
        old_name: &'a str,
        /// Name after the rename.
        new_name: &'a str,
    },
    /// The content of a renamed existing torrent was renamed on disk.
    ExistingRenamedOnDisk {
        /// Previous path on disk.
        old_name: &'a str,
        /// New name on disk.
        new_name: &'a str,
    },
    /// Renaming the content of a renamed existing torrent on disk failed.
    ExistingRenameOnDiskFailed(&'a anyhow::Error),
    /// Renaming the files of a multi-file torrent with dots formatting started.
    RenamingFiles,
    /// The file list for renaming files could not be read, so no files were renamed.
    FileListFailed(&'a anyhow::Error),
    /// One file was renamed with dots formatting.
    FileRenamed {
        /// File name before the rename.
        old_name: &'a str,
        /// File name after the rename.
        new_name: &'a str,
    },
    /// Renaming one file failed on the last attempt.
    FileRenameFailed {
        /// File name that could not be renamed.
        name: &'a str,
        /// Error from the rename.
        error: &'a anyhow::Error,
    },
    /// This many files were renamed with dots formatting.
    FilesRenamed(usize),
    /// This many files could not be renamed after all attempts.
    FileRenamesIncomplete(usize),
    /// This many excluded files were set to be skipped.
    ExcludedFilesSkipped(usize),
    /// Setting the excluded files to be skipped failed after all attempts.
    ExcludedFilesSkipFailed {
        /// Number of excluded files.
        count: usize,
        /// Why the last attempt failed.
        error: Option<&'a str>,
    },
}

/// Options for adding torrents to qBittorrent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddOptions {
    /// Save path for the downloaded files.
    pub save_path: Option<String>,
    /// Category for the torrents.
    pub category: Option<String>,
    /// Add torrents in paused state.
    pub paused: bool,
}

/// Adds torrents to qBittorrent and renames their content.
pub struct TorrentAdder<'a> {
    /// Logged in API client.
    client: &'a QBittorrentClient,
    /// Options for the add request.
    options: &'a AddOptions,
    /// Name formatting for renaming files.
    names: &'a NameSuggester,
}

/// Parsed torrents and the files that could not be parsed.
#[derive(Debug, Default)]
pub struct ParsedTorrents {
    /// Successfully parsed torrents in input order.
    pub torrents: Vec<TorrentInfo>,
    /// Torrent files that could not be parsed and why.
    pub failures: Vec<(PathBuf, anyhow::Error)>,
}

/// Receives the steps of adding torrents.
///
/// Every method does nothing by default, and `()` reports nothing.
pub trait AddObserver {
    /// One step of adding happened.
    fn on_event(&mut self, _event: AddEvent<'_>) {}
}

impl AddObserver for () {}

impl<'a> TorrentAdder<'a> {
    /// Create an adder using the client, the add options, and the name formatting.
    #[must_use]
    pub const fn new(client: &'a QBittorrentClient, options: &'a AddOptions, names: &'a NameSuggester) -> Self {
        Self { client, options, names }
    }

    /// Add a torrent to qBittorrent and return the size of its included files.
    ///
    /// After adding, the content is renamed on disk when the torrent has a new output name,
    /// the files of a multi-file torrent are renamed with dots formatting when it is enabled,
    /// Torrents with excluded files start paused until their skip priorities are verified.
    /// Rename failures are reported to the observer and do not fail the add.
    ///
    /// # Errors
    /// Returns an error if qBittorrent rejects the torrent,
    /// if excluded priorities cannot be verified, or if starting a filtered torrent fails.
    pub async fn add(&self, info: TorrentInfo, observer: &mut impl AddObserver) -> Result<u64> {
        let info_hash = info.info_hash.clone();
        let display_name = info.display_name().into_owned();
        let effective_is_multi_file = info.effective_is_multi_file;
        let excluded_indices = info.excluded_indices.clone();
        let rename_to = info.rename_to.clone();
        let original_name = info.original_name.clone();

        // Use "Original" to preserve torrent structure, or "NoSubfolder" for single files
        let content_layout = if effective_is_multi_file {
            Some("Original".to_string())
        } else {
            Some("NoSubfolder".to_string())
        };

        let params = AddTorrentParams {
            torrent_path: info.path.to_string_lossy().to_string(),
            torrent_bytes: info.bytes,
            save_path: self.options.save_path.clone(),
            category: self.options.category.clone(),
            tags: info.tags,
            rename: rename_to.clone(),
            skip_checking: false,
            paused: self.options.paused || !excluded_indices.is_empty(),
            content_layout,
        };

        self.client.add_torrent(params).await?;

        observer.on_event(AddEvent::Added {
            name: &display_name,
            is_multi_file: effective_is_multi_file,
        });

        let folder_rename = rename_to
            .as_deref()
            .zip(original_name.as_deref())
            .filter(|(new_name, old_name)| new_name != old_name);

        if let Some((new_name, old_name)) = folder_rename {
            self.rename_on_disk(&info_hash, old_name, new_name, effective_is_multi_file, observer)
                .await;
        }

        if effective_is_multi_file && let Some(dot_rename) = self.names.dot_formatter() {
            // Wait for the torrent to be ready when no folder rename already waited for it
            if folder_rename.is_none() {
                tokio::time::sleep(FILE_RENAME_DELAY).await;
            }

            file_updates::rename_torrent_files(self.client, &info_hash, &dot_rename, observer).await;
        }

        if !excluded_indices.is_empty() {
            let wait_for_metadata = rename_to.is_none() || original_name.is_none();
            file_updates::apply_excluded_file_priorities(
                self.client,
                &info_hash,
                &excluded_indices,
                wait_for_metadata,
                observer,
            )
            .await?;
            if !self.options.paused {
                self.client.start_torrent(&info_hash).await.with_context(|| {
                    format!("Could not start torrent {info_hash} after verifying excluded file priorities")
                })?;
            }
        }

        Ok(info.included_size)
    }

    /// Rename a torrent that already exists in qBittorrent, and its content on disk.
    ///
    /// The content on disk keeps its name when it already matches the new name.
    /// A failed rename on disk is reported to the observer and does not fail the rename.
    ///
    /// # Errors
    /// Returns an error if qBittorrent does not rename the torrent.
    pub async fn rename_existing(
        &self,
        info: &TorrentInfo,
        existing_name: &str,
        new_name: &str,
        observer: &mut impl AddObserver,
    ) -> Result<()> {
        self.client
            .set_torrent_name(&info.info_hash, new_name)
            .await
            .context("Failed to set torrent name")?;

        observer.on_event(AddEvent::ExistingRenamed {
            old_name: existing_name,
            new_name,
        });

        if let Err(error) = self.rename_existing_on_disk(info, new_name, observer).await {
            observer.on_event(AddEvent::ExistingRenameOnDiskFailed(&error));
        }
        Ok(())
    }

    /// Read the current server paths before renaming existing content.
    async fn rename_existing_on_disk(
        &self,
        info: &TorrentInfo,
        new_name: &str,
        observer: &mut impl AddObserver,
    ) -> Result<()> {
        let files = self.client.get_torrent_files(&info.info_hash).await?;
        let old_path = if info.effective_is_multi_file {
            let root = files
                .first()
                .and_then(|file| file.name.split_once('/'))
                .map(|(root, _)| root)
                .filter(|root| !root.is_empty())
                .context("Torrent has no current root folder")?;
            ensure!(
                files
                    .iter()
                    .all(|file| file.name.split_once('/').is_some_and(|(first, _)| first == root)),
                "Torrent files do not share a single root folder"
            );
            root
        } else {
            let mut included_files = files.iter().filter(|file| !info.excluded_indices.contains(&file.index));
            let file = included_files.next().context("Torrent has no current included file")?;
            ensure!(
                included_files.next().is_none(),
                "Torrent has more than one current included file"
            );
            &file.name
        };
        if old_path == new_name {
            return Ok(());
        }

        if info.effective_is_multi_file {
            self.client.rename_folder(&info.info_hash, old_path, new_name).await?;
        } else {
            self.client.rename_file(&info.info_hash, old_path, new_name).await?;
        }
        observer.on_event(AddEvent::ExistingRenamedOnDisk {
            old_name: old_path,
            new_name,
        });
        Ok(())
    }

    /// Rename the root folder or the single file of a just added torrent on disk,
    /// retrying with increasing delays while qBittorrent registers the torrent.
    async fn rename_on_disk(
        &self,
        info_hash: &str,
        old_name: &str,
        new_name: &str,
        is_multi_file: bool,
        observer: &mut impl AddObserver,
    ) {
        let mut last_error = None;

        for delay in DISK_RENAME_DELAYS {
            tokio::time::sleep(delay).await;

            let rename_result = if is_multi_file {
                self.client.rename_folder(info_hash, old_name, new_name).await
            } else {
                self.client.rename_file(info_hash, old_name, new_name).await
            };

            match rename_result {
                Ok(()) => {
                    observer.on_event(AddEvent::RenamedOnDisk { old_name, new_name });
                    return;
                }
                Err(error) => last_error = Some(error),
            }
        }

        observer.on_event(AddEvent::RenameOnDiskFailed {
            old_name,
            new_name,
            error: last_error.as_ref(),
        });
    }
}

/// Parse the torrent files, filtering their files and resolving their tags.
#[must_use]
pub fn parse_torrents(torrent_paths: &[PathBuf], filter: &FileFilter, tag_rules: &TagRules) -> ParsedTorrents {
    let mut parsed = ParsedTorrents::default();
    for path in torrent_paths {
        match parse_torrent(path, filter, tag_rules.resolve(path)) {
            Ok(info) => parsed.torrents.push(info),
            Err(error) => parsed.failures.push((path.clone(), error)),
        }
    }
    parsed
}

/// Check if a torrent already exists in qBittorrent by comparing info hashes.
///
/// Returns the existing `TorrentListItem` if found, `None` otherwise.
#[must_use]
pub fn find_existing_torrent<'a, S: BuildHasher>(
    info: &TorrentInfo,
    existing_torrents: &'a HashMap<String, TorrentListItem, S>,
) -> Option<&'a TorrentListItem> {
    existing_torrents.get(&info.info_hash.to_lowercase())
}

#[cfg(test)]
mod test_find_existing_torrent {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::qbittorrent::TorrentListItem;
    use crate::torrent::Torrent;

    use super::*;

    /// Creates a minimal `TorrentListItem` for testing with the given name.
    fn create_torrent_list_item(name: &str) -> TorrentListItem {
        TorrentListItem {
            hash: String::new(),
            name: name.to_string(),
            added_on: 0,
            completion_on: None,
            progress: 0.0,
            ratio: 0.0,
            save_path: String::new(),
            size: 0,
            tags: String::new(),
        }
    }

    /// Creates a minimal `TorrentInfo` for testing with the given info hash.
    fn create_torrent_info_with_hash(info_hash: &str) -> TorrentInfo {
        TorrentInfo {
            path: PathBuf::from("test.torrent"),
            torrent: Torrent::default(),
            bytes: vec![],
            info_hash: info_hash.to_string(),
            original_is_multi_file: false,
            effective_is_multi_file: false,
            rename_to: None,
            included_size: 1000,
            excluded_indices: vec![],
            single_included_file: None,
            original_name: None,
            tags: None,
        }
    }

    #[test]
    fn returns_none_when_map_is_empty() {
        let info = create_torrent_info_with_hash("abc123def456");
        let existing: HashMap<String, TorrentListItem> = HashMap::new();

        let result = find_existing_torrent(&info, &existing);
        assert!(result.is_none());
    }

    #[test]
    fn returns_none_when_hash_not_found() {
        let info = create_torrent_info_with_hash("abc123def456");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert("different_hash".to_string(), create_torrent_list_item("Some Torrent"));

        let result = find_existing_torrent(&info, &existing);
        assert!(result.is_none());
    }

    #[test]
    fn returns_name_when_hash_matches() {
        let info = create_torrent_info_with_hash("abc123def456");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert(
            "abc123def456".to_string(),
            create_torrent_list_item("Existing Torrent Name"),
        );

        let result = find_existing_torrent(&info, &existing);
        assert_eq!(
            result.map(|item| &item.name),
            Some(&"Existing Torrent Name".to_string())
        );
    }

    #[test]
    fn matches_case_insensitively() {
        let info = create_torrent_info_with_hash("ABC123DEF456");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert("abc123def456".to_string(), create_torrent_list_item("Existing Torrent"));

        let result = find_existing_torrent(&info, &existing);
        assert_eq!(result.map(|item| &item.name), Some(&"Existing Torrent".to_string()));
    }

    #[test]
    fn finds_among_multiple_torrents() {
        let info = create_torrent_info_with_hash("target_hash_123");
        let mut existing: HashMap<String, TorrentListItem> = HashMap::new();
        existing.insert("hash_one".to_string(), create_torrent_list_item("Torrent One"));
        existing.insert(
            "target_hash_123".to_string(),
            create_torrent_list_item("Target Torrent"),
        );
        existing.insert("hash_three".to_string(), create_torrent_list_item("Torrent Three"));

        let result = find_existing_torrent(&info, &existing);
        assert_eq!(result.map(|item| &item.name), Some(&"Target Torrent".to_string()));
    }
}

#[cfg(test)]
mod test_parse_torrents {
    use std::path::Path;

    use super::*;

    #[test]
    fn keeps_parsed_torrents_and_reports_failures() {
        let paths = vec![
            Path::new("tests/fixtures/dummy.torrent").to_path_buf(),
            Path::new("tests/fixtures/missing.torrent").to_path_buf(),
        ];
        let tag_rules = TagRules::new(Some("tag".to_string()), Vec::new(), Vec::new());

        let parsed = parse_torrents(&paths, &FileFilter::default(), &tag_rules);

        assert_eq!(parsed.torrents.len(), 1);
        assert_eq!(parsed.torrents[0].tags.as_deref(), Some("tag"));
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].0, paths[1]);
    }
}

#[cfg(test)]
mod test_torrent_adder {
    use crate::test_server::{Route, route, serve};
    use crate::utils::test_torrent_info_helpers::create_torrent_info;

    use super::*;

    /// Observer that records a short description of every event.
    #[derive(Default)]
    struct RecordingObserver {
        events: Vec<String>,
    }

    impl AddObserver for RecordingObserver {
        fn on_event(&mut self, event: AddEvent<'_>) {
            let description = match event {
                AddEvent::Added { name, .. } => format!("added {name}"),
                AddEvent::RenamedOnDisk { old_name, new_name } => format!("renamed {old_name} {new_name}"),
                AddEvent::RenameOnDiskFailed { .. } => "rename failed".to_string(),
                AddEvent::ExistingRenamed { old_name, new_name } => format!("existing {old_name} {new_name}"),
                AddEvent::ExistingRenamedOnDisk { .. } => "existing on disk".to_string(),
                AddEvent::ExistingRenameOnDiskFailed(_) => "existing on disk failed".to_string(),
                other => format!("{other:?}"),
            };
            self.events.push(description);
        }
    }

    /// Client that is logged in to a local server with the given routes.
    async fn logged_in_client(mut routes: Vec<Route>) -> Result<QBittorrentClient> {
        routes.push(route("/api/v2/auth/login", 200, "Ok."));
        let mut client = QBittorrentClient::new("127.0.0.1", serve(routes).await);
        client.login("user", "password").await?;
        Ok(client)
    }

    #[tokio::test]
    async fn adding_reports_the_added_name_and_the_disk_rename() -> Result<()> {
        let client = logged_in_client(vec![
            route("/api/v2/torrents/add", 200, "Ok."),
            route("/api/v2/torrents/renameFile", 200, ""),
        ])
        .await?;
        let mut info = create_torrent_info("Show.torrent", Some("show.mkv"), Some("Show.mkv"), false, false, None);
        info.original_name = Some("show.mkv".to_string());
        info.included_size = 1234;
        let options = AddOptions::default();
        let names = NameSuggester::default();
        let mut observer = RecordingObserver::default();

        let size = TorrentAdder::new(&client, &options, &names)
            .add(info, &mut observer)
            .await?;

        assert_eq!(size, 1234);
        assert_eq!(observer.events, vec!["added Show.mkv", "renamed show.mkv Show.mkv"]);
        Ok(())
    }

    #[tokio::test]
    async fn filtered_torrent_starts_only_after_its_exclusions_are_verified() -> Result<()> {
        use std::sync::{Arc, Mutex};

        for (paused, can_verify) in [(false, true), (true, true), (false, false)] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let request_log = Arc::clone(&requests);
            let mut priority_changed = false;
            let port = crate::test_server::serve_with(vec![route("/api/v2/auth/login", 200, "Ok.")], move |request| {
                let path = request.split_whitespace().nth(1).unwrap();
                let (event, status, body) = if path.ends_with("/add") {
                    let paused = request.contains("name=\"paused\"\r\n\r\ntrue")
                        && request.contains("name=\"stopped\"\r\n\r\ntrue");
                    (if paused { "add paused" } else { "add active" }, 200, "Ok.")
                } else if path.contains("/files?") {
                    if priority_changed && can_verify {
                        (
                            "verified priority",
                            200,
                            r#"[{"index":1,"name":"notes.txt","priority":0}]"#,
                        )
                    } else {
                        ("read priority", 200, r#"[{"index":1,"name":"notes.txt","priority":1}]"#)
                    }
                } else if path.ends_with("/filePrio") {
                    assert!(request.ends_with("id=1&priority=0"));
                    priority_changed = true;
                    ("set priority", 200, "")
                } else if path.ends_with("/start") {
                    assert!(request.ends_with("hash=test_hash"));
                    ("start", 200, "")
                } else {
                    return None;
                };
                request_log.lock().unwrap().push(event);
                Some(route("", status, body))
            })
            .await;
            let mut client = QBittorrentClient::new("127.0.0.1", port);
            client.login("user", "password").await?;
            let mut info = create_torrent_info("Show.torrent", Some("Show.mkv"), None, true, false, None);
            info.info_hash = "test_hash".to_owned();
            info.excluded_indices = vec![1];
            let options = AddOptions {
                paused,
                ..AddOptions::default()
            };
            let names = NameSuggester::default();
            let result = TorrentAdder::new(&client, &options, &names).add(info, &mut ()).await;

            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests.first(), Some(&"add paused"));
            if can_verify {
                let mut expected = vec!["add paused", "read priority", "set priority", "verified priority"];
                if !paused {
                    expected.push("start");
                }
                assert_eq!(requests, expected);
                assert!(result.is_ok(), "{result:?}");
            } else {
                assert!(requests.contains(&"set priority"));
                assert!(!requests.contains(&"start"));
                assert!(result.is_err());
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejected_add_returns_an_error_without_events() -> Result<()> {
        let client = logged_in_client(vec![route("/api/v2/torrents/add", 415, "")]).await?;
        let info = create_torrent_info("Show.torrent", Some("show.mkv"), None, false, false, None);
        let options = AddOptions::default();
        let names = NameSuggester::default();
        let mut observer = RecordingObserver::default();

        let result = TorrentAdder::new(&client, &options, &names)
            .add(info, &mut observer)
            .await;

        assert!(result.is_err());
        assert_eq!(observer.events, Vec::<String>::new());
        Ok(())
    }

    #[tokio::test]
    async fn existing_torrent_renames_use_current_server_paths() -> Result<()> {
        use std::sync::{Arc, Mutex};

        for (multi_file, server_files, old_path, endpoint) in [
            (
                false,
                r#"[{"index":0,"name":"Current.mkv","priority":1}]"#,
                "Current.mkv",
                "renameFile",
            ),
            (
                true,
                r#"[{"index":0,"name":"Current/one.mkv","priority":1},{"index":1,"name":"Current/sub/two.mkv","priority":1}]"#,
                "Current",
                "renameFolder",
            ),
        ] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            let port = crate::test_server::serve_with(
                vec![
                    route("/api/v2/auth/login", 200, "Ok."),
                    route("/api/v2/torrents/files", 200, server_files),
                    route("/api/v2/torrents/rename", 200, ""),
                ],
                move |request| {
                    if request.contains("oldPath=") {
                        recorded.lock().unwrap().push(request.to_owned());
                    }
                    None
                },
            )
            .await;
            let mut client = QBittorrentClient::new("127.0.0.1", port);
            client.login("user", "password").await?;
            let mut info =
                create_torrent_info("Show.torrent", Some("Original.mkv"), None, multi_file, multi_file, None);
            info.original_name = Some("Original.mkv".to_owned());
            let options = AddOptions::default();
            let names = NameSuggester::default();
            let adder = TorrentAdder::new(&client, &options, &names);
            adder
                .rename_existing(&info, "Display Name", "Replacement", &mut ())
                .await?;

            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("POST /api/v2/torrents/{endpoint} ")));
            assert!(
                requests[0].ends_with(&format!("oldPath={old_path}&newPath=Replacement")),
                "{}",
                requests[0]
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn renaming_an_existing_torrent_reports_both_renames() -> Result<()> {
        let client = logged_in_client(vec![
            route(
                "/api/v2/torrents/files",
                200,
                r#"[{"index":0,"name":"show.mkv","priority":1}]"#,
            ),
            route("/api/v2/torrents/rename", 200, ""),
            route("/api/v2/torrents/renameFile", 200, ""),
        ])
        .await?;
        let mut info = create_torrent_info("Show.torrent", Some("show.mkv"), None, false, false, None);
        info.original_name = Some("show.mkv".to_string());
        let options = AddOptions::default();
        let names = NameSuggester::default();
        let mut observer = RecordingObserver::default();

        TorrentAdder::new(&client, &options, &names)
            .rename_existing(&info, "Old Name", "Show.mkv", &mut observer)
            .await?;

        assert_eq!(observer.events, vec!["existing Old Name Show.mkv", "existing on disk"]);
        Ok(())
    }
}
