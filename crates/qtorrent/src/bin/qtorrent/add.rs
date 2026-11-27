//! Add workflow of the qtorrent binary.
//!
//! Parses the torrent files and adds them to qBittorrent one by one with confirmation,
//! or shows what would be done in dry-run and offline mode.
//! Added torrent files are moved to the downloaded directory and skipped duplicates are trashed.
//! Terminal output lives in `display`, the library events are printed by `printer`,
//! and the interactive questions live in `prompt`.

mod display;
mod printer;
mod prompt;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use colored::Colorize;

use cli_tools_dots::DotRenameConfig;
use cli_tools_qtorrent::utils;
use cli_tools_qtorrent::{NameSuggester, TorrentAdder, TorrentInfo, TorrentStats};

use crate::QtorrentArgs;
use crate::client;
use crate::config::Config;

use printer::EventPrinter;

/// Main handler for adding torrents to qBittorrent.
pub struct QTorrent {
    config: Config,
    names: NameSuggester,
}

impl QTorrent {
    /// Create a new `QTorrent` from command line arguments.
    ///
    /// Loads user configuration and merges it with CLI arguments.
    ///
    /// # Errors
    /// Returns an error if the config file cannot be read or parsed.
    pub fn new(args: QtorrentArgs) -> Result<Self> {
        let config = Config::from_args(args)?;
        let dot_rename = if config.use_dots_formatting {
            Some(DotRenameConfig::from_user_config()?)
        } else {
            None
        };
        Ok(Self::from_config(config, dot_rename))
    }

    /// Create a `QTorrent` from a finished config and an optional dots formatting config.
    fn from_config(config: Config, dot_rename: Option<DotRenameConfig>) -> Self {
        let names = NameSuggester::new(config.naming.clone(), dot_rename);
        Self { config, names }
    }

    /// Run the main workflow to add torrents.
    ///
    /// # Errors
    /// Returns an error if torrents cannot be parsed or added.
    pub async fn run(self) -> Result<()> {
        let torrent_paths = self.config.collector.collect()?;
        if torrent_paths.is_empty() {
            bail!("No torrent files found");
        }

        let torrents = self.parse_torrents(&torrent_paths);
        let parse_failed = torrents.len() < torrent_paths.len();

        if torrents.is_empty() {
            println!("{}", "No valid torrents to add".yellow());
            bail!("All input torrents failed to parse");
        }

        let failed = if self.config.dryrun {
            let _ = self.run_dryrun(torrents).await?;
            parse_failed
        } else {
            let stats = self.add_torrents_individually(torrents).await?;
            parse_failed || stats.error > 0
        };
        anyhow::ensure!(!failed, "One or more torrent operations failed");
        Ok(())
    }

    /// Connect to qBittorrent and add torrents one by one with individual confirmation.
    async fn add_torrents_individually(&self, torrents: Vec<TorrentInfo>) -> Result<TorrentStats> {
        self.add_torrents_with_collision_handlers(
            torrents,
            |existing, prompt| self.confirm_downloaded_collision(existing, prompt),
            trash_duplicate_torrent_file,
        )
        .await
    }

    /// Run the add workflow with the supplied confirmation and trash handlers for archive collisions.
    async fn add_torrents_with_collision_handlers(
        &self,
        torrents: Vec<TorrentInfo>,
        confirm_collision: impl Fn(&Path, &str) -> Result<bool>,
        trash_duplicate: impl FnMut(&Path) -> bool,
    ) -> Result<TorrentStats> {
        let mut index_changes = Vec::new();
        let result = self
            .process_torrents_with_handlers(torrents, confirm_collision, trash_duplicate, &mut index_changes)
            .await;
        update_filefind_index(&index_changes);
        result
    }

    /// Execute the torrent add loop while recording completed local `.torrent` moves and removals.
    #[allow(clippy::too_many_lines)]
    async fn process_torrents_with_handlers(
        &self,
        torrents: Vec<TorrentInfo>,
        confirm_collision: impl Fn(&Path, &str) -> Result<bool>,
        mut trash_duplicate: impl FnMut(&Path) -> bool,
        index_changes: &mut Vec<cli_tools_common::PathChange>,
    ) -> Result<TorrentStats> {
        let mut client = client::connect(&self.config.connection, self.config.verbose).await?;
        let adder = TorrentAdder::new(&client, &self.config.add_options, &self.names);
        let mut printer = EventPrinter::new(self.config.verbose);

        // Get the list of existing torrents to check for duplicates
        let mut existing_torrents: HashMap<_, _> = client
            .get_torrent_list()
            .await?
            .into_iter()
            .map(|(hash, item)| (hash, Some(item.name)))
            .collect();
        if self.config.verbose {
            println!(
                "{} {}",
                "Existing torrents in qBittorrent:".dimmed(),
                existing_torrents.len()
            );
        }

        let total = torrents.len();
        let mut stats = TorrentStats::new(total);

        for (index, mut info) in torrents.into_iter().enumerate() {
            if self.config.verbose {
                println!("{}", "─".repeat(60));
            }

            self.print_torrent_info(&info, index + 1, total);

            if info.all_files_excluded() {
                println!("  {} All files excluded by filters, skipping torrent", "⊘".yellow());
                stats.inc_skipped();
                continue;
            }

            // Determine destination "downloaded" directory once per torrent
            let torrent_path = info.path.clone();
            let downloaded_directory = utils::find_downloaded_directory(&torrent_path);
            let downloaded_collision = downloaded_directory
                .as_deref()
                .and_then(|directory| utils::existing_torrent_in_downloaded(directory, &torrent_path));

            let info_hash = info.info_hash.to_lowercase();
            if let Some(existing_name) = existing_torrents.get(&info_hash) {
                let Some(existing_name) = existing_name else {
                    cli_tools_common::print_yellow!(
                        "Already added, but setup failed earlier in this run. Keeping the input."
                    );
                    stats.inc_skipped();
                    continue;
                };
                println!(
                    "  {} Already exists in qBittorrent as: {}",
                    "⊘".yellow(),
                    existing_name.cyan()
                );

                if let Some(existing) = downloaded_collision.as_deref()
                    && !confirm_collision(existing, "Continue anyway?")?
                {
                    println!("{}", "Skipped.".yellow());
                    stats.inc_skipped();
                    if utils::is_duplicate_torrent_file(&torrent_path, existing) && trash_duplicate(&torrent_path) {
                        index_changes.push(cli_tools_common::PathChange::removed_file(torrent_path.clone()));
                    }
                    continue;
                }

                match self
                    .prompt_rename_existing(&info, existing_name, &adder, &mut printer)
                    .await
                {
                    Ok(Some(new_name)) => {
                        existing_torrents.insert(info_hash.clone(), Some(new_name));
                        stats.inc_renamed();
                    }
                    Ok(None) => stats.inc_duplicate(),
                    Err(error) => {
                        cli_tools_common::print_error!("Failed to rename: {error}");
                        stats.inc_error();
                    }
                }

                // The torrent already exists in qBittorrent, so the torrent file has been processed
                if let Some(directory) = downloaded_directory.as_deref() {
                    move_torrent_file(&torrent_path, directory, index_changes);
                }
                continue;
            }

            let add_confirmed_by_collision = if let Some(existing) = downloaded_collision.as_deref() {
                if !confirm_collision(existing, "Add anyway?")? {
                    println!("{}", "Skipped.".yellow());
                    stats.inc_skipped();
                    // Only an identical copy of the downloaded torrent is a duplicate.
                    // A different torrent that happens to share the file name was never added, so it stays.
                    if utils::is_duplicate_torrent_file(&torrent_path, existing) && trash_duplicate(&torrent_path) {
                        index_changes.push(cli_tools_common::PathChange::removed_file(torrent_path.clone()));
                    }
                    continue;
                }
                true
            } else {
                false
            };

            if let Some(new_name) = self.prompt_rename(&info)? {
                info.rename_to = Some(new_name);
            }

            self.print_final_details(&info);

            // Ask for confirmation unless --yes flag is set or the downloaded collision was already confirmed
            let should_add = if self.config.yes || add_confirmed_by_collision {
                true
            } else {
                cli_tools_common::get_user_confirmation("Add this torrent?", true)
                    .map_err(|error| anyhow::anyhow!("Failed to get confirmation: {error}"))?
            };

            if !should_add {
                println!("{}", "Skipped.".yellow());
                stats.inc_skipped();
                continue;
            }

            let display_name = info.display_name().into_owned();
            let add_result = adder.add(info, &mut printer).await;
            if printer.take_add_acknowledged() {
                existing_torrents.insert(info_hash, add_result.is_ok().then_some(display_name));
            }
            match add_result {
                Ok(size) => {
                    stats.inc_success(size);
                    if let Some(directory) = downloaded_directory.as_deref() {
                        move_torrent_file(&torrent_path, directory, index_changes);
                    }
                }
                Err(error) => {
                    stats.inc_error();
                    cli_tools_common::print_error!("{error}");
                }
            }
        }

        client::logout(&mut client).await;

        printer::print_summary(&stats);

        Ok(stats)
    }

    /// Show what would be added and report whether at least one torrent would be submitted.
    async fn run_dryrun(self, torrents: Vec<TorrentInfo>) -> Result<bool> {
        let torrents_with_names = self.preview_torrents(torrents);

        let existing_torrents = if self.config.offline {
            None
        } else if !self.config.connection.has_credentials() {
            cli_tools_common::print_yellow!("No credentials configured. Use --offline to skip qBittorrent connection.");
            None
        } else {
            match client::connect(&self.config.connection, self.config.verbose).await {
                Ok(mut client) => {
                    let existing_torrents = client.get_torrent_list().await;
                    client::logout(&mut client).await;
                    Some(existing_torrents?)
                }
                Err(error) => {
                    cli_tools_common::print_yellow!("Failed to connect: {error}");
                    None
                }
            }
        };

        Ok(self.print_dryrun_summary(&torrents_with_names, existing_torrents.as_ref()))
    }

    /// Prepare names for the dry run without asking interactive questions.
    fn preview_torrents(&self, torrents: Vec<TorrentInfo>) -> Vec<TorrentInfo> {
        if self.config.yes && !self.config.auto_rename {
            return torrents;
        }

        torrents
            .into_iter()
            .map(|mut info| {
                info.rename_to = Some(self.names.clean_suggested_name(&info));
                info
            })
            .collect()
    }

    /// Parse all torrent files and resolve tags for each, printing the files that fail to parse.
    fn parse_torrents(&self, torrent_paths: &[std::path::PathBuf]) -> Vec<TorrentInfo> {
        let parsed =
            cli_tools_qtorrent::parse_torrents(torrent_paths, &self.config.file_filter, &self.config.tag_rules);
        for (path, error) in &parsed.failures {
            cli_tools_common::print_error!("Failed to parse {}: {error:#}", path.display());
        }
        parsed.torrents
    }
}

/// Move the torrent file into the downloaded directory, log the outcome, and record real local moves.
///
/// Errors are reported as warnings. They do not abort the surrounding workflow.
fn move_torrent_file(
    torrent_path: &Path,
    downloaded_directory: &Path,
    index_changes: &mut Vec<cli_tools_common::PathChange>,
) {
    let is_same_file_noop = torrent_path
        .file_name()
        .is_some_and(|name| cli_tools_common::paths_refer_to_same_file(torrent_path, &downloaded_directory.join(name)));

    match utils::move_torrent_to_downloaded(torrent_path, downloaded_directory) {
        Ok(destination) => {
            if !is_same_file_noop {
                index_changes.push(cli_tools_common::PathChange::moved_file(
                    torrent_path.to_path_buf(),
                    destination.clone(),
                ));
            }
            println!(
                "  {} Moved torrent file to: {}",
                "\u{2192}".green(),
                destination.display().to_string().cyan()
            );
        }
        Err(error) => {
            cli_tools_common::print_yellow!("Failed to move torrent file to downloaded directory: {error}");
        }
    }
}

/// Move a skipped duplicate torrent file to the trash and return whether it was removed.
///
/// Errors are reported as warnings. They do not abort the surrounding workflow.
fn trash_duplicate_torrent_file(torrent_path: &Path) -> bool {
    match cli_tools_common::move_to_trash(torrent_path) {
        Ok(()) => {
            println!(
                "  {} Trashed duplicate torrent file: {}",
                "\u{1f5d1}".yellow(),
                torrent_path.display().to_string().cyan()
            );
            true
        }
        Err(error) => {
            cli_tools_common::print_yellow!(
                "Failed to trash duplicate torrent file {}: {error}",
                torrent_path.display()
            );
            false
        }
    }
}

/// Apply completed local `.torrent` moves and removals to the user's filefind index, if one exists.
fn update_filefind_index(changes: &[cli_tools_common::PathChange]) {
    if cfg!(test) || changes.is_empty() {
        return;
    }
    for warning in filefind_common::update_default_index(changes) {
        cli_tools_common::print_yellow!("Could not update filefind index: {warning}");
    }
}

#[cfg(test)]
mod test_add_workflow {
    use std::path::PathBuf;

    use clap::Parser;
    use serde_bytes::ByteBuf;
    use tempfile::TempDir;

    use cli_tools_qtorrent::torrent::{Info, Torrent};
    use cli_tools_qtorrent::{FileFilter, parse_torrent};

    use crate::config::QtorrentConfig;
    use crate::test_server::{Route, route, serve};

    use super::*;

    /// Directory holding one single-file torrent and an empty `downloaded` directory beside it.
    struct TorrentDirectory {
        _temp_directory: TempDir,
        root: PathBuf,
        torrent_path: PathBuf,
    }

    impl TorrentDirectory {
        fn new() -> Self {
            let temp_directory = TempDir::new().expect("should create temp directory");
            let root = temp_directory.path().join("torrents");
            std::fs::create_dir_all(root.join(utils::DOWNLOADED_DIRECTORY_NAME))
                .expect("should create downloaded directory");
            let mut torrent = Torrent::default();
            torrent.announce = Some("http://tracker.example.com/announce".to_string());
            torrent.info = Info {
                name: Some("Example.Video.mp4".to_string()),
                length: Some(1_000_000),
                piece_length: 262_144,
                pieces: ByteBuf::from(vec![0_u8; 20]),
                ..Info::default()
            };
            let torrent_path = root.join("Example.Video.torrent");
            std::fs::write(
                &torrent_path,
                serde_bencode::to_bytes(&torrent).expect("should serialize torrent"),
            )
            .expect("should write torrent file");
            Self {
                _temp_directory: temp_directory,
                root,
                torrent_path,
            }
        }

        fn moved_torrent_path(&self) -> PathBuf {
            self.root
                .join(utils::DOWNLOADED_DIRECTORY_NAME)
                .join("Example.Video.torrent")
        }

        fn info_hash(&self) -> String {
            parse_torrent(&self.torrent_path, &FileFilter::default(), None)
                .expect("should parse torrent")
                .info_hash
        }
    }

    fn qtorrent(arguments: &[&str]) -> QTorrent {
        let args = QtorrentArgs::try_parse_from(arguments).expect("arguments should parse");
        QTorrent::from_config(
            Config::from_args_with_user_config(args, QtorrentConfig::default()),
            None,
        )
    }

    /// `QTorrent` for the directory, connecting to the given local port with credentials and `--yes`.
    fn connected_qtorrent(directory: &TorrentDirectory, port: u16, extra: &[&str]) -> QTorrent {
        let root = directory.root.to_string_lossy().into_owned();
        let port = port.to_string();
        let mut arguments = vec![
            "qtorrent",
            root.as_str(),
            "-H",
            "127.0.0.1",
            "-P",
            port.as_str(),
            "-u",
            "user",
            "-w",
            "password",
            "--yes",
            "--verbose",
        ];
        arguments.extend_from_slice(extra);
        qtorrent(&arguments)
    }

    fn session_routes(torrent_list: String) -> Vec<Route> {
        vec![
            route("/api/v2/auth/login", 200, "Ok."),
            route("/api/v2/auth/logout", 200, ""),
            route("/api/v2/app/version", 200, "v5.0.0"),
            route("/api/v2/app/webapiVersion", 200, "2.11.2"),
            route("/api/v2/torrents/info", 200, torrent_list),
            route("/api/v2/torrents/add", 200, "Ok."),
        ]
    }

    fn torrent_list_with(info_hash: &str) -> String {
        format!(
            r#"[{{"hash":"{info_hash}","name":"Existing Name","added_on":1,"completion_on":-1,"progress":0.0,"ratio":0.0,"save_path":"/downloads","size":1000000,"tags":""}}]"#
        )
    }

    #[tokio::test]
    async fn adds_a_new_torrent_and_moves_the_file_to_downloaded() -> Result<()> {
        let directory = TorrentDirectory::new();
        let port = serve(session_routes("[]".to_string())).await;

        connected_qtorrent(&directory, port, &[]).run().await?;

        assert!(!directory.torrent_path.exists());
        assert!(directory.moved_torrent_path().is_file());
        Ok(())
    }

    #[tokio::test]
    async fn identical_hashes_are_only_submitted_once_per_run() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let directory = TorrentDirectory::new();
        let second_path = directory.root.join("Other.Name.torrent");
        std::fs::copy(&directory.torrent_path, &second_path)?;
        let submissions = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&submissions);
        let port = crate::test_server::serve_with(session_routes("[]".to_owned()), move |request| {
            if request.starts_with("POST /api/v2/torrents/add ") {
                recorded.fetch_add(1, Ordering::Relaxed);
            }
            None
        })
        .await;

        connected_qtorrent(&directory, port, &[]).run().await?;

        assert_eq!(submissions.load(Ordering::Relaxed), 1);
        assert!(directory.moved_torrent_path().is_file());
        assert!(!second_path.exists());
        assert!(
            directory
                .root
                .join(utils::DOWNLOADED_DIRECTORY_NAME)
                .join("Other.Name.torrent")
                .is_file()
        );
        Ok(())
    }

    #[tokio::test]
    async fn existing_torrent_is_not_added_again_but_the_file_is_moved() -> Result<()> {
        let directory = TorrentDirectory::new();
        let mut routes = session_routes(torrent_list_with(&directory.info_hash()));
        routes.retain(|route| !route.path().starts_with("/api/v2/torrents/add"));
        let port = serve(routes).await;

        connected_qtorrent(&directory, port, &[]).run().await?;

        assert!(directory.moved_torrent_path().is_file());
        Ok(())
    }

    #[tokio::test]
    async fn failed_add_keeps_the_torrent_file_in_place() -> Result<()> {
        let directory = TorrentDirectory::new();
        let mut routes = session_routes("[]".to_string());
        routes.retain(|route| !route.path().starts_with("/api/v2/torrents/add"));
        routes.push(route("/api/v2/torrents/add", 415, ""));
        let port = serve(routes).await;

        assert!(connected_qtorrent(&directory, port, &[]).run().await.is_err());

        assert!(directory.torrent_path.is_file());
        assert!(!directory.moved_torrent_path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn declined_archive_collision_trashes_only_identical_bytes() -> Result<()> {
        for identical in [true, false] {
            let directory = TorrentDirectory::new();
            let source_bytes = std::fs::read(&directory.torrent_path)?;
            let archive = directory.moved_torrent_path();
            if identical {
                std::fs::write(&archive, &source_bytes)?;
            } else {
                let mut archived_torrent = Torrent::from_buffer(&source_bytes)?;
                archived_torrent.announce = Some("http://other.example.com/announce".to_owned());
                std::fs::write(&archive, serde_bencode::to_bytes(&archived_torrent)?)?;
            }
            for already_added in [false, true] {
                let listing = if already_added {
                    torrent_list_with(&directory.info_hash())
                } else {
                    "[]".to_owned()
                };
                let port = serve(session_routes(listing)).await;
                let application = connected_qtorrent(&directory, port, &[]);
                let torrents = application.parse_torrents(std::slice::from_ref(&directory.torrent_path));
                let mut trash_requested = Vec::new();
                application
                    .add_torrents_with_collision_handlers(
                        torrents,
                        |path, _| {
                            assert_eq!(path, archive);
                            Ok(false)
                        },
                        |path| {
                            trash_requested.push(path.to_path_buf());
                            true
                        },
                    )
                    .await?;

                assert_eq!(
                    trash_requested.len(),
                    usize::from(identical),
                    "already added: {already_added}"
                );
                assert_eq!(std::fs::read(&directory.torrent_path)?, source_bytes);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn unacknowledged_add_keeps_the_torrent_file_in_place() -> Result<()> {
        let directory = TorrentDirectory::new();
        let mut routes = session_routes("[]".to_string());
        routes.retain(|route| route.path() != "/api/v2/torrents/add");
        routes.push(route("/api/v2/torrents/add", 200, "<html>Sign in</html>"));
        let port = serve(routes).await;

        assert!(connected_qtorrent(&directory, port, &[]).run().await.is_err());

        assert!(directory.torrent_path.is_file());
        assert!(!directory.moved_torrent_path().exists());
        Ok(())
    }

    #[tokio::test]
    async fn dryrun_with_connection_reports_existing_torrents_without_changes() -> Result<()> {
        let directory = TorrentDirectory::new();
        let port = serve(session_routes(torrent_list_with(&directory.info_hash()))).await;

        connected_qtorrent(&directory, port, &["--dryrun"]).run().await?;

        assert!(directory.torrent_path.is_file());
        Ok(())
    }

    #[test]
    fn yes_preview_keeps_the_same_name_as_execution() -> Result<()> {
        let directory = TorrentDirectory::new();
        let friendly_path = directory.root.join("Friendly.torrent");
        std::fs::rename(&directory.torrent_path, &friendly_path)?;
        let application = connected_qtorrent(&directory, 1, &["--dryrun"]);
        let torrents = application.parse_torrents(&[friendly_path]);
        let execution_name = application.prompt_rename(&torrents[0])?;
        let preview = application.preview_torrents(torrents);
        assert_eq!(preview[0].rename_to, execution_name);
        assert_eq!(preview[0].display_name(), "Example.Video.mp4");
        Ok(())
    }

    #[tokio::test]
    async fn connected_dryrun_reports_a_failed_duplicate_lookup() {
        let directory = TorrentDirectory::new();
        let mut routes = session_routes("[]".to_owned());
        routes.retain(|route| route.path() != "/api/v2/torrents/info");
        routes.push(route("/api/v2/torrents/info", 500, "[]"));
        let port = serve(routes).await;
        let error = connected_qtorrent(&directory, port, &["--dryrun"])
            .run()
            .await
            .expect_err("a connected preview must report duplicate lookup failure");
        assert!(error.to_string().contains("500"), "{error}");
        assert!(directory.torrent_path.is_file());
    }

    #[tokio::test]
    async fn dryrun_does_not_double_count_existing_fully_excluded_torrents() -> Result<()> {
        let directory = TorrentDirectory::new();
        let mut torrent = Torrent::from_buffer(&std::fs::read(&directory.torrent_path)?)?;
        torrent.info.length = None;
        torrent.info.files = Some(vec![cli_tools_qtorrent::torrent::File {
            length: 1_000_000,
            path: vec!["cover.jpg".to_owned()],
            ..Default::default()
        }]);
        std::fs::write(&directory.torrent_path, serde_bencode::to_bytes(&torrent)?)?;
        let port = serve(session_routes(torrent_list_with(&directory.info_hash()))).await;
        let application = connected_qtorrent(&directory, port, &["--dryrun"]);
        let torrents = application.parse_torrents(std::slice::from_ref(&directory.torrent_path));
        assert!(torrents[0].all_files_excluded());

        application.run().await?;

        assert!(directory.torrent_path.is_file());
        Ok(())
    }

    #[tokio::test]
    async fn offline_and_unreachable_dryruns_still_print_the_summary() -> Result<()> {
        let directory = TorrentDirectory::new();
        let root = directory.root.to_string_lossy().into_owned();
        let unused_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("should bind a local port");
            listener.local_addr().expect("should have a local address").port()
        };

        qtorrent(&["qtorrent", &root, "--offline", "--verbose"]).run().await?;
        qtorrent(&["qtorrent", &root, "--dryrun"]).run().await?;
        connected_qtorrent(&directory, unused_port, &["--dryrun"]).run().await?;

        assert!(directory.torrent_path.is_file());
        Ok(())
    }

    #[tokio::test]
    async fn adding_without_credentials_or_torrents_fails() {
        let directory = TorrentDirectory::new();
        let root = directory.root.to_string_lossy().into_owned();
        let empty = TempDir::new().expect("should create temp directory");
        let empty_root = empty.path().to_string_lossy().into_owned();

        let credentials_error = qtorrent(&["qtorrent", &root, "--yes"])
            .run()
            .await
            .expect_err("should fail");
        let empty_error = qtorrent(&["qtorrent", &empty_root, "--yes"])
            .run()
            .await
            .expect_err("should fail");

        assert!(credentials_error.to_string().contains("credentials not configured"));
        assert!(empty_error.to_string().contains("No torrent files found"));
    }
}
