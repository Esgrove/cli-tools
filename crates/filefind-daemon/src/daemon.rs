//! Core daemon lifecycle and monitoring logic.
//!
//! This module provides the main daemon struct that manages:
//! - Daemon lifecycle (start, stop, status)
//! - Background monitoring of file changes via USN Journal (NTFS) and file watcher
//! - Coordinating updates to the database based on file changes

use std::collections::HashMap;
#[cfg(not(windows))]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cli_tools_common::{format_number, print_cyan, print_error, print_green, print_yellow};
use colored::Colorize;
use filefind_common::scope::find_most_specific_volume;
use filefind_common::{
    Config, DaemonStateInfo, Database, FileChangeEvent, FileEntry, IndexScope, IpcClient, VolumeMount,
    extract_drive_letter_from_str,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use crate::ipc_server::{IpcServerState, IpcToDaemon, spawn_ipc_server};
use crate::mft::detect_ntfs_volumes;
use crate::pruner::prune_missing_entries;
use crate::scanner::{deleted_references, run_scan, validate_configured_roots};
use crate::usn::{UsnChange, UsnMonitor};
use crate::watcher::FileWatcher;

/// Default USN Journal poll interval in milliseconds.
const DEFAULT_USN_POLL_INTERVAL_MS: u64 = 1000;

/// Default file watcher debounce interval in milliseconds.
const DEFAULT_WATCHER_DEBOUNCE_MS: u64 = 2000;

/// Represents the running state of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// Daemon is not running.
    Stopped,
    /// Daemon is starting up (scanning, initializing monitors).
    Starting,
    /// Daemon is running and monitoring for changes.
    Running,
    /// Daemon is shutting down.
    Stopping,
}

/// Cache key identifying an indexed NTFS directory by drive letter and MFT reference number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DirectoryCacheKey {
    /// Uppercase Windows drive letter.
    drive_letter: char,
    /// MFT file reference number on that drive.
    mft_reference: u64,
}

/// Batch of raw USN journal changes read from one NTFS drive.
struct DriveUsnBatch {
    /// Uppercase Windows drive letter.
    drive_letter: char,
    /// Raw USN changes collected during the poll.
    changes: Vec<UsnChange>,
    /// Next USN cursor to persist after processing `changes`.
    next_usn: i64,
}

/// Updated USN journal position to persist for one NTFS drive.
struct DriveUsnUpdate {
    /// Uppercase Windows drive letter.
    drive_letter: char,
    /// Latest processed USN journal position.
    usn: i64,
}

/// Database row loaded while rebuilding the directory MFT path cache.
struct CachedDirectoryRow {
    /// Uppercase Windows drive letter derived from the volume mount point, if any.
    drive_letter: Option<char>,
    /// MFT file reference number of the directory.
    mft_reference: u64,
    /// Full indexed directory path.
    path: String,
}

/// The main daemon struct that manages file indexing.
pub struct Daemon {
    /// Current daemon state.
    state: DaemonState,

    /// Configuration.
    config: Config,

    /// Daemon options.
    options: DaemonOptions,

    /// Database connection.
    database: Option<Database>,

    /// Shutdown signal.
    shutdown: Arc<AtomicBool>,

    /// Active volume monitors (keyed by drive letter).
    volume_monitors: HashMap<char, VolumeMonitor>,

    /// File watcher for non-NTFS paths.
    file_watcher: Option<FileWatcher>,

    /// Receiver for file watcher events.
    watcher_receiver: Option<mpsc::Receiver<FileChangeEvent>>,

    /// Cache of MFT file reference to full path (for path resolution).
    path_cache: HashMap<DirectoryCacheKey, String>,

    /// Cached volume mounts for component-aware volume lookup.
    volume_mounts: Vec<VolumeMount>,

    /// Shared state for IPC server.
    ipc_state: Arc<IpcServerState>,

    /// Receiver for IPC commands.
    ipc_receiver: Option<mpsc::Receiver<IpcToDaemon>>,

    /// Whether indexing is currently paused.
    is_paused: bool,

    /// A failed reconciliation scan must be retried before consuming more changes.
    recovery_pending: bool,

    /// Last periodic reconciliation of watched paths.
    last_periodic_scan: Instant,
}

/// Configuration for the daemon.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Run in foreground instead of daemonizing.
    pub foreground: bool,

    /// Force a full rescan on startup.
    pub rescan: bool,

    /// USN Journal poll interval in milliseconds.
    /// Controls how often the daemon checks for NTFS file system changes.
    pub usn_poll_interval_ms: u64,

    /// File watcher debounce interval in milliseconds.
    pub watcher_debounce_ms: u64,
}

/// Volume monitor that tracks a single volume.
struct VolumeMonitor {
    /// USN monitor for NTFS volumes (None for non-NTFS).
    usn_monitor: Option<UsnMonitor>,

    /// Last processed USN for this volume.
    last_usn: i64,
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped"),
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Stopping => write!(f, "stopping"),
        }
    }
}

impl Daemon {
    /// Create a new daemon instance.
    #[must_use]
    pub fn new(config: Config, options: DaemonOptions) -> Self {
        Self {
            state: DaemonState::Stopped,
            config,
            options,
            database: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            volume_monitors: HashMap::new(),
            file_watcher: None,
            watcher_receiver: None,
            path_cache: HashMap::new(),
            volume_mounts: Vec::new(),
            ipc_state: Arc::new(IpcServerState::new()),
            ipc_receiver: None,
            is_paused: false,
            recovery_pending: false,
            last_periodic_scan: Instant::now(),
        }
    }

    /// Get the current daemon state.
    #[must_use]
    #[allow(dead_code)]
    pub const fn state(&self) -> DaemonState {
        self.state
    }

    /// Check if verbose output is enabled.
    #[must_use]
    pub const fn verbose(&self) -> bool {
        self.config.daemon.verbose
    }

    /// Check if the daemon is running.
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_running(&self) -> bool {
        matches!(self.state, DaemonState::Running)
    }

    /// Get the shutdown signal handle.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// Start the daemon.
    ///
    /// This initializes the database, performs an initial scan if needed,
    /// and starts monitoring for file changes.
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting filefind daemon");
        if self.state != DaemonState::Stopped {
            warn!("Daemon is already {}", self.state);
            return Ok(());
        }

        self.state = DaemonState::Starting;
        self.ipc_state.state.store(DaemonStateInfo::Starting);
        self.shutdown.store(false, Ordering::Relaxed);

        // Start IPC server and fail immediately if another instance owns the control endpoint.
        let ipc_receiver = spawn_ipc_server(Arc::clone(&self.ipc_state), Arc::clone(&self.shutdown))?;
        self.ipc_receiver = Some(ipc_receiver);

        // Initialize database
        let database_path = self.config.database_path();
        if let Some(parent) = database_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create database directory")?;
        }

        debug!("Opening database: {}", database_path.display());
        let database = Database::open(&database_path).context("Failed to open database")?;

        self.database = Some(database);

        // Update IPC state with database stats
        if let Some(ref database) = self.database {
            self.ipc_state.update_from_database(database);
        }

        // Arm the file watcher before walking so changes during startup scans are buffered and replayed.
        let non_ntfs_paths = self.get_non_ntfs_paths();
        if !non_ntfs_paths.is_empty()
            && let Err(error) = self.start_file_watcher(non_ntfs_paths)
        {
            warn!("Failed to start file watcher before initial scan: {error:#}");
        }

        // Perform initial scan if requested, if the database is empty, or to reconcile walked roots on restart.
        let has_walked_roots = !self.get_non_ntfs_paths().is_empty();
        if self.options.rescan || self.should_perform_initial_scan()? || has_walked_roots {
            info!("Performing scan");
            self.ipc_state.state.store(DaemonStateInfo::Scanning);

            if let Err(error) = run_scan(None, &self.config).await {
                error!("Initial scan incomplete, retrying while running: {error:#}");
                self.recovery_pending = true;
            }

            // Update stats after scan
            if let Some(ref database) = self.database {
                self.ipc_state.update_from_database(database);
            }
        }

        // Build path cache for USN and volume path resolution
        self.build_path_cache()?;

        // Start USN monitors (and retry file watcher if it failed above)
        self.start_monitors();
        self.last_periodic_scan = Instant::now();

        // Update monitored volumes count (USN volume monitors plus active watched roots).
        self.update_monitored_volumes_count();

        self.state = DaemonState::Running;
        self.ipc_state.state.store(DaemonStateInfo::Running);
        if self.verbose() {
            print_green!("Daemon started successfully");
        }

        Ok(())
    }

    /// Stop the daemon.
    pub fn stop(&mut self) {
        info!("Stopping daemon");
        if self.state == DaemonState::Stopped {
            warn!("Daemon is already stopped");
            return;
        }

        self.state = DaemonState::Stopping;
        self.ipc_state.state.store(DaemonStateInfo::Stopping);
        self.shutdown.store(true, Ordering::Relaxed);

        // Stop all USN monitors
        for (drive_letter, monitor) in &self.volume_monitors {
            if let Some(ref usn) = monitor.usn_monitor {
                info!("Stopping USN monitor for {}:\\", drive_letter);
                usn.stop();
            }
        }
        self.volume_monitors.clear();

        // Stop file watcher
        if let Some(ref watcher) = self.file_watcher {
            info!("Stopping file watcher for {} paths", watcher.watched_paths().len());
            for path in watcher.watched_paths() {
                info!("Stopping file watcher for {}", path.display());
            }
            watcher.stop();
        }

        self.state = DaemonState::Stopped;
        self.ipc_state.state.store(DaemonStateInfo::Stopped);
        if self.config.daemon.verbose {
            print_green!("Daemon stopped");
        }
    }

    /// Run the daemon's main loop.
    ///
    /// This runs until shutdown is signaled.
    pub async fn run(&mut self) -> Result<()> {
        if self.state != DaemonState::Running {
            self.start().await?;
        }

        if self.verbose() {
            print_cyan!("Daemon running. Press Ctrl+C to stop.");
        }

        // Main loop - wait for shutdown signal
        while !self.shutdown.load(Ordering::Relaxed) {
            // Process IPC commands
            self.process_ipc_commands().await?;

            // Process any pending file changes (unless paused)
            let retry_interval = Duration::from_secs(self.config.daemon.scan_interval_seconds.clamp(5, 60));
            if self.recovery_pending && self.last_periodic_scan.elapsed() >= retry_interval
                || !self.recovery_pending && !self.is_paused && self.process_changes()
            {
                self.last_periodic_scan = Instant::now();
                self.ipc_state.state.store(DaemonStateInfo::Scanning);
                match self.rescan_and_restart_usn(true).await {
                    Ok(()) => self.recovery_pending = false,
                    Err(error) => {
                        warn!("Recovery scan incomplete, will retry: {error:#}");
                        self.recovery_pending = true;
                    }
                }
                self.ipc_state.state.store(DaemonStateInfo::Running);
            } else if !self.recovery_pending && !self.is_paused && self.file_watcher.is_some() {
                let watcher_error = self.file_watcher.as_ref().is_some_and(FileWatcher::take_rescan_request);
                let periodic = self.last_periodic_scan.elapsed()
                    >= Duration::from_secs(self.config.daemon.scan_interval_seconds.max(1));
                if watcher_error || periodic {
                    self.last_periodic_scan = Instant::now();
                    self.ipc_state.state.store(DaemonStateInfo::Scanning);
                    if let Err(error) = self.rescan_and_restart_usn(false).await {
                        warn!("Watched paths could not be reconciled: {error:#}");
                        self.recovery_pending = true;
                    }
                    self.ipc_state.state.store(DaemonStateInfo::Running);
                }
            }

            // Sleep for the configured poll interval
            tokio::time::sleep(Duration::from_millis(self.options.usn_poll_interval_ms)).await;
        }

        self.stop();

        Ok(())
    }

    /// Process incoming IPC commands.
    async fn process_ipc_commands(&mut self) -> Result<()> {
        // Capture verbose flag before borrowing receiver to avoid borrow conflicts
        let verbose = self.verbose();

        // Process all available commands without blocking
        while let Some(command) = self.ipc_receiver.as_mut().and_then(|receiver| receiver.try_recv().ok()) {
            match command {
                IpcToDaemon::Stop => {
                    info!("Received stop command via IPC");
                    self.shutdown.store(true, Ordering::Relaxed);
                }
                IpcToDaemon::Rescan => {
                    info!("Received rescan command via IPC");
                    self.ipc_state.state.store(DaemonStateInfo::Scanning);
                    if let Err(error) = self.rescan_and_restart_usn(false).await {
                        warn!("Requested rescan incomplete, will retry: {error:#}");
                        self.recovery_pending = true;
                    }
                    // Update stats after rescan
                    if let Some(ref database) = self.database {
                        self.ipc_state.update_from_database(database);
                    }
                    self.ipc_state.state.store(DaemonStateInfo::Running);
                }
                IpcToDaemon::Pause => {
                    info!("Received pause command via IPC");
                    self.is_paused = true;
                    self.ipc_state.is_paused.store(true, Ordering::Relaxed);
                }
                IpcToDaemon::Resume => {
                    info!("Received resume command via IPC");
                    self.is_paused = false;
                    self.ipc_state.is_paused.store(false, Ordering::Relaxed);
                }
                IpcToDaemon::Prune => {
                    info!("Received prune command via IPC");
                    self.ipc_state.state.store(DaemonStateInfo::Scanning);
                    if let Some(ref database) = self.database {
                        match prune_missing_entries(database, verbose) {
                            Ok(stats) => {
                                info!(
                                    "Prune completed: removed {} files and {} directories",
                                    stats.files_removed, stats.directories_removed
                                );
                            }
                            Err(error) => {
                                error!("Prune failed: {}", error);
                            }
                        }
                        // Update stats after prune
                        self.ipc_state.update_from_database(database);
                    }
                    self.ipc_state.state.store(DaemonStateInfo::Running);
                }
            }
        }

        Ok(())
    }

    async fn rescan_and_restart_usn(&mut self, clean: bool) -> Result<()> {
        let mut scan_config = self.config.clone();
        if clean {
            scan_config.daemon.force_clean_scan = true;
        }
        let result = run_scan(None, &scan_config).await;
        let cache_result = if result.is_ok() {
            self.build_path_cache()
        } else {
            Ok(())
        };
        let drives: Vec<char> = self.volume_monitors.keys().copied().collect();
        let mut failed_monitors = 0usize;
        for drive in drives {
            let previous = self.volume_monitors.remove(&drive);
            if let Err(error) = self.start_usn_monitor(drive) {
                warn!("Could not restart USN monitor for {drive}: {error:#}");
                failed_monitors += 1;
                if let Some(monitor) = previous {
                    self.volume_monitors.insert(drive, monitor);
                }
            }
        }
        let non_ntfs_paths = self.get_non_ntfs_paths();
        // Keep a healthy file watcher alive across scans so events queued during the walk are retained and replayed.
        if self.file_watcher.is_none()
            && !non_ntfs_paths.is_empty()
            && let Err(error) = self.start_file_watcher(non_ntfs_paths)
        {
            warn!("File watcher could not start: {error:#}");
            failed_monitors += 1;
        }
        if let Some(ref database) = self.database {
            self.ipc_state.update_from_database(database);
        }
        self.update_monitored_volumes_count();
        result?;
        cache_result?;
        if failed_monitors > 0 {
            anyhow::bail!("{failed_monitors} USN monitor(s) could not restart");
        }
        Ok(())
    }

    /// Check if an initial scan should be performed.
    fn should_perform_initial_scan(&self) -> Result<bool> {
        let Some(ref database) = self.database else {
            return Ok(true);
        };

        let stats = database.get_stats()?;
        Ok(stats.total_files == 0 && stats.total_directories == 0)
    }

    /// Start all monitors (USN Journal and file watcher).
    fn start_monitors(&mut self) {
        // Get drives that are referenced in configured paths
        let configured_drives = self.get_configured_drives();

        // Detect NTFS volumes and start USN monitors only for configured drives
        let ntfs_drives = detect_ntfs_volumes();
        for drive_letter in ntfs_drives {
            if !configured_drives.contains(&drive_letter) {
                debug!("Skipping USN monitor for {drive_letter}:\\ (not in configured paths)");
                continue;
            }
            if let Err(error) = self.start_usn_monitor(drive_letter) {
                warn!("Failed to start USN monitor for {drive_letter}:\\ - {error}");
            }
        }

        // Start file watcher for non-NTFS paths if not already armed before the initial scan.
        if self.file_watcher.is_none() {
            let non_ntfs_paths = self.get_non_ntfs_paths();
            if !non_ntfs_paths.is_empty()
                && let Err(error) = self.start_file_watcher(non_ntfs_paths)
            {
                warn!("File watcher could not start, will retry: {error:#}");
                self.recovery_pending = true;
            }
        }
    }

    /// Start USN Journal monitor for a specific drive.
    fn start_usn_monitor(&mut self, drive_letter: char) -> Result<()> {
        // Get last USN from database
        let last_usn = self
            .database
            .as_ref()
            .and_then(|db| db.get_volume_last_usn(drive_letter).ok())
            .flatten()
            .unwrap_or(0);

        info!("Starting USN monitor for {drive_letter}:");

        let usn_monitor = UsnMonitor::new(drive_letter, last_usn)?;

        let monitor = VolumeMonitor {
            usn_monitor: Some(usn_monitor),
            last_usn,
        };

        self.volume_monitors.insert(drive_letter, monitor);

        Ok(())
    }

    /// Start file watcher for non-NTFS paths.
    fn start_file_watcher(&mut self, paths: Vec<PathBuf>) -> Result<()> {
        let watcher = FileWatcher::with_scope(
            paths,
            IndexScope::from_daemon_config(&self.config.daemon),
            self.options.watcher_debounce_ms,
            true,
        );
        let watched_paths = watcher.watched_paths();

        info!("Starting file watcher for {} paths", watched_paths.len());
        for path in watched_paths {
            info!("Watching {}", path.display());
        }

        // Start the watcher and get the event receiver
        let (receiver, _shutdown) = watcher.start()?;

        self.watcher_receiver = Some(receiver);
        self.file_watcher = Some(watcher);

        Ok(())
    }

    /// Update the IPC status gauge with the total number of active USN monitors and OS watches.
    fn update_monitored_volumes_count(&self) {
        let watched = self
            .file_watcher
            .as_ref()
            .map_or(0, FileWatcher::registered_watch_count);
        let total = self.volume_monitors.len() + watched;
        self.ipc_state.monitored_volumes.store(total as u64, Ordering::Relaxed);
    }

    /// Extract drive letters from configured paths.
    ///
    /// Returns a list of uppercase drive letters that are referenced in the daemon's configured paths.
    /// This is used to filter which NTFS volumes should have USN monitors started.
    fn get_configured_drives(&self) -> Vec<char> {
        let mut drives: Vec<char> = self
            .config
            .daemon
            .paths
            .iter()
            .filter_map(|path_str| extract_drive_letter_from_str(path_str))
            .collect();

        drives.sort_unstable();
        drives.dedup();
        drives
    }

    /// Get paths that are not on NTFS volumes.
    fn get_non_ntfs_paths(&self) -> Vec<PathBuf> {
        let ntfs_drives: Vec<char> = self.volume_monitors.keys().copied().collect();

        self.config
            .daemon
            .paths
            .iter()
            .filter_map(|path_str| {
                let path = PathBuf::from(path_str);
                let first_char = path_str.chars().next()?;

                // Skip if this is on an NTFS drive we're monitoring
                if ntfs_drives.contains(&first_char.to_ascii_uppercase()) {
                    return None;
                }

                // Include UNC paths and non-NTFS drives
                Some(path)
            })
            .collect()
    }

    /// Process any pending file changes.
    fn process_changes(&mut self) -> bool {
        // Collect raw USN changes first to avoid borrow issues
        let mut raw_changes: Vec<DriveUsnBatch> = Vec::new();
        let mut needs_rescan = false;

        for monitor in self.volume_monitors.values_mut() {
            if let Some(ref mut usn) = monitor.usn_monitor {
                match usn.read_changes() {
                    Ok((changes, next_usn)) => {
                        if !changes.is_empty() {
                            debug!("Processing {} USN changes", changes.len());
                            let drive_letter = usn.get_drive_letter();
                            raw_changes.push(DriveUsnBatch {
                                drive_letter,
                                changes,
                                next_usn,
                            });
                            monitor.last_usn = next_usn;
                        }
                    }
                    Err(error) => {
                        error!("Error reading USN changes: {}", error);
                        needs_rescan = true;
                    }
                }
            }
        }

        // Now process the collected changes (no longer borrowing volume_monitors)
        let mut usn_updates: Vec<DriveUsnUpdate> = Vec::new();
        // Collect deletions by volume for batch processing via MFT reference
        let mut deletions_by_drive: HashMap<char, Vec<u64>> = HashMap::new();

        for batch in raw_changes {
            let drive_letter = batch.drive_letter;
            for change in &batch.changes {
                // Handle deletions specially - use MFT reference directly since path may not be resolvable
                if change.is_delete() {
                    if change.is_directory {
                        let key = DirectoryCacheKey {
                            drive_letter,
                            mft_reference: change.file_reference,
                        };
                        let path = self.path_cache.get(&key).cloned().or_else(|| {
                            let volume_id = self.get_volume_id_for_path(Path::new(&format!("{drive_letter}:")))?;
                            self.database
                                .as_ref()?
                                .get_path_by_mft_reference(volume_id, change.file_reference)
                                .ok()?
                        });
                        if let Some(path) = path {
                            needs_rescan |= !self.update_directory_index(drive_letter, change, &path);
                        } else {
                            needs_rescan = true;
                        }
                    }
                    debug!("File deleted: {} (MFT ref {})", change.name, change.file_reference);
                    continue;
                }

                if change.is_rename_old() {
                    continue;
                }

                // For non-delete events, try to resolve the path from parent reference and name
                let full_path = self.resolve_usn_path_inner(drive_letter, change.parent_reference, &change.name);

                if let Some(path) = full_path {
                    if !self.should_index_path(&path) {
                        needs_rescan |= !self.handle_out_of_scope_change(drive_letter, change, path);
                        continue;
                    }
                    if change.is_directory {
                        needs_rescan |= !self.update_directory_index(drive_letter, change, &path);
                    } else if change.is_rename_new() {
                        needs_rescan |= !self.update_renamed_file(change, &path);
                    }
                    // Convert to event
                    if let Some(event) = Self::usn_change_to_event(change, &path) {
                        needs_rescan |= !self.handle_change_event_with_reference(event, Some(change.file_reference));
                    }
                } else if change.is_create() || change.is_rename_new() {
                    warn!("Cannot resolve USN path for {}, requesting rescan", change.name);
                    needs_rescan = true;
                }
            }
            deletions_by_drive.insert(drive_letter, deleted_references(&batch.changes));
            usn_updates.push(DriveUsnUpdate {
                drive_letter,
                usn: batch.next_usn,
            });
        }

        needs_rescan |= self.apply_usn_deletions(&deletions_by_drive);

        // Update USN values in database
        if !needs_rescan && let Some(ref db) = self.database {
            for update in usn_updates {
                if let Err(error) = db.update_volume_usn_by_drive(update.drive_letter, update.usn) {
                    error!("Failed to update USN for drive {}: {}", update.drive_letter, error);
                    needs_rescan = true;
                }
            }
        }

        needs_rescan |= self.process_watcher_events();
        needs_rescan
    }

    fn handle_out_of_scope_change(&mut self, drive_letter: char, change: &UsnChange, path: String) -> bool {
        if change.is_directory {
            let key = DirectoryCacheKey {
                drive_letter,
                mft_reference: change.file_reference,
            };
            if change.is_rename_new()
                && let Some(old_path) = self.path_cache.get(&key).cloned()
            {
                if let Some(ref database) = self.database
                    && let Err(error) = database.delete_files_under_path(&old_path)
                {
                    warn!("Could not remove directory moved out of scope: {error:#}");
                    return false;
                }
                self.rebase_cached_paths(drive_letter, &old_path, &path);
            }
            self.path_cache.insert(key, path);
        } else if change.is_rename_new()
            && let Some(volume_id) = self.get_volume_id_for_path(Path::new(&path))
            && let Some(ref database) = self.database
            && let Err(error) = database.delete_files_by_mft_references(volume_id, &[change.file_reference])
        {
            warn!("Could not remove file moved out of scope: {error:#}");
            return false;
        }
        true
    }

    fn update_renamed_file(&self, change: &UsnChange, path: &str) -> bool {
        let Some(volume_id) = self.get_volume_id_for_path(Path::new(path)) else {
            return false;
        };
        let Some(ref database) = self.database else {
            return false;
        };
        match database.get_path_by_mft_reference(volume_id, change.file_reference) {
            Ok(Some(old_path)) if old_path != path => {
                if let Err(error) = database.update_file_path(&old_path, path, &change.name) {
                    error!("Failed to move indexed path {}: {}", old_path, error);
                    return false;
                }
            }
            Err(error) => {
                error!("Failed to find old path for renamed file {}: {}", path, error);
                return false;
            }
            _ => {}
        }
        true
    }

    fn process_watcher_events(&mut self) -> bool {
        let mut watcher_events: Vec<FileChangeEvent> = Vec::new();
        if let Some(ref mut receiver) = self.watcher_receiver {
            while let Ok(event) = receiver.try_recv() {
                watcher_events.push(event);
            }
        }

        if watcher_events.is_empty() {
            return false;
        }

        let mut needs_rescan = false;
        for event in watcher_events {
            needs_rescan |= !self.handle_change_event_with_reference(event, None);
        }

        if let Some(ref database) = self.database {
            self.ipc_state.update_from_database(database);
        }

        needs_rescan
    }

    fn apply_usn_deletions(&self, deletions_by_drive: &HashMap<char, Vec<u64>>) -> bool {
        let Some(ref database) = self.database else {
            return !deletions_by_drive.is_empty();
        };
        let mut needs_rescan = false;
        for (drive_letter, references) in deletions_by_drive {
            let Some(volume_id) = self.get_volume_id_for_path(Path::new(&format!("{drive_letter}:"))) else {
                error!("No indexed volume for USN deletions on {}:", drive_letter);
                needs_rescan = true;
                continue;
            };
            match database.delete_files_by_mft_references(volume_id, references) {
                Ok(deleted) if deleted > 0 => debug!("Deleted {} files from {}:", deleted, drive_letter),
                Ok(_) => {}
                Err(error) => {
                    error!("Failed to delete files by MFT reference: {}", error);
                    needs_rescan = true;
                }
            }
        }
        needs_rescan
    }

    /// Convert a USN change to a file change event.
    fn usn_change_to_event(change: &UsnChange, full_path: &str) -> Option<FileChangeEvent> {
        let path = PathBuf::from(full_path);

        if change.is_create() {
            Some(FileChangeEvent::Created(path))
        } else if change.is_delete() {
            Some(FileChangeEvent::Deleted(path))
        } else if change.is_rename_new() {
            Some(FileChangeEvent::Created(path))
        } else if change.is_modify() {
            Some(FileChangeEvent::Modified(path))
        } else {
            None
        }
    }

    /// Resolve a full path from USN change data (inner version that borrows `path_cache`).
    fn resolve_usn_path_inner(&self, drive_letter: char, parent_reference: u64, name: &str) -> Option<String> {
        let key = DirectoryCacheKey {
            drive_letter,
            mft_reference: parent_reference,
        };
        // Try to get parent path from cache
        if let Some(parent_path) = self.path_cache.get(&key) {
            return Some(format!("{parent_path}\\{name}"));
        }

        // If parent is root (reference 5), construct path directly
        if parent_reference == 5 {
            return Some(format!("{drive_letter}:\\{name}"));
        }

        if let Some(volume_id) = self.get_volume_id_for_path(Path::new(&format!("{drive_letter}:")))
            && let Some(ref database) = self.database
            && let Ok(Some(parent_path)) = database.get_path_by_mft_reference(volume_id, parent_reference)
        {
            return Some(format!("{parent_path}\\{name}"));
        }

        trace!(
            "Could not resolve path for file '{}' with parent ref {}",
            name, parent_reference
        );
        None
    }

    fn update_directory_index(&mut self, drive_letter: char, change: &UsnChange, path: &str) -> bool {
        let key = DirectoryCacheKey {
            drive_letter,
            mft_reference: change.file_reference,
        };
        if change.is_delete() {
            if let Some(ref database) = self.database
                && let Err(error) = database.delete_files_under_path(path)
            {
                error!("Failed to remove deleted directory subtree {}: {}", path, error);
                return false;
            }
            self.path_cache.retain(|cached_key, cached_path| {
                cached_key.drive_letter != drive_letter || !Self::path_is_within(cached_path, path)
            });
        } else if change.is_rename_new() {
            let old_path = self.path_cache.get(&key).cloned().or_else(|| {
                let volume_id = self.get_volume_id_for_path(Path::new(path))?;
                self.database
                    .as_ref()?
                    .get_path_by_mft_reference(volume_id, change.file_reference)
                    .ok()?
            });
            if let Some(old_path) = old_path
                && old_path != path
            {
                let moved_into_scope = !self.should_index_path(&old_path);
                if let Some(ref database) = self.database
                    && let Err(error) = database.rebase_files_under_path(&old_path, path, &change.name)
                {
                    error!("Failed to rebase renamed directory {}: {}", old_path, error);
                    return false;
                }
                self.rebase_cached_paths(drive_letter, &old_path, path);
                if moved_into_scope {
                    self.path_cache.insert(key, path.to_string());
                    return false;
                }
            }
            self.path_cache.insert(key, path.to_string());
        } else if change.is_create() {
            self.path_cache.insert(key, path.to_string());
        }
        true
    }

    fn path_is_within(candidate: &str, parent: &str) -> bool {
        candidate == parent
            || candidate
                .strip_prefix(parent)
                .is_some_and(|suffix| suffix.starts_with(['\\', '/']))
    }

    fn rebase_cached_paths(&mut self, drive_letter: char, old_path: &str, new_path: &str) {
        for (cached_key, cached_path) in &mut self.path_cache {
            if cached_key.drive_letter == drive_letter
                && Self::path_is_within(cached_path, old_path)
                && let Some(suffix) = cached_path.strip_prefix(old_path)
            {
                *cached_path = format!("{new_path}{suffix}");
            }
        }
    }

    fn should_index_path(&self, path: &str) -> bool {
        let scope = IndexScope::from_daemon_config(&self.config.daemon);
        let volume_mount =
            find_most_specific_volume(path, &self.volume_mounts).map(|volume| volume.mount_point.as_str());
        scope.is_included(path, volume_mount)
    }

    /// Handle a file change event by updating the database.
    #[cfg(test)]
    fn handle_change_event(&self, event: FileChangeEvent) {
        let _ = self.handle_change_event_with_reference(event, None);
    }

    fn handle_change_event_with_reference(&self, event: FileChangeEvent, file_reference: Option<u64>) -> bool {
        let Some(ref db) = self.database else {
            return false;
        };

        match event {
            FileChangeEvent::Created(path) => self.handle_created_event(db, &path, file_reference),
            FileChangeEvent::Modified(path) => self.handle_modified_event(db, &path, file_reference),
            FileChangeEvent::Deleted(path) => Self::handle_deleted_event(db, &path),
            FileChangeEvent::Renamed { from, to } => self.handle_renamed_event(db, &from, &to, file_reference),
        }
    }

    /// Index a newly created file or populated directory tree.
    fn handle_created_event(&self, db: &Database, path: &Path, file_reference: Option<u64>) -> bool {
        debug!("File created: {}", path.display());
        let Some(path_str) = path.to_str() else {
            warn!("Skipping non-UTF-8 created path: {}", path.display());
            return true;
        };
        if !self.should_index_path(path_str) {
            return true;
        }

        let Some(volume_id) = self.get_volume_id_for_path(path) else {
            trace!("No volume found for path: {}", path.display());
            return false;
        };

        let Ok(metadata) = std::fs::metadata(path) else {
            return true;
        };
        let mut entry = FileEntry::from_metadata(volume_id, path, &metadata);
        entry.mft_reference = file_reference;

        if let Err(error) = db.insert_file(&entry) {
            error!("Failed to insert file {}: {}", path.display(), error);
            return false;
        }

        !metadata.is_dir() || self.index_directory_descendants(db, volume_id, path)
    }

    /// Refresh metadata for a modified entry, or delete it if it is already gone.
    fn handle_modified_event(&self, db: &Database, path: &Path, file_reference: Option<u64>) -> bool {
        debug!("File modified: {}", path.display());
        let Some(path_str) = path.to_str() else {
            warn!("Skipping non-UTF-8 modified path: {}", path.display());
            return true;
        };
        match std::fs::metadata(path) {
            Ok(metadata) => {
                if !self.should_index_path(path_str) {
                    return true;
                }
                let Some(volume_id) = self.get_volume_id_for_path(path) else {
                    trace!("No volume found for modified path: {}", path.display());
                    return false;
                };
                let mut entry = FileEntry::from_metadata(volume_id, path, &metadata);
                entry.mft_reference = file_reference;
                if let Err(error) = db.insert_file(&entry) {
                    error!("Failed to update modified file {}: {}", path.display(), error);
                    return false;
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::handle_deleted_event(db, path),
            Err(_) => true,
        }
    }

    /// Remove a deleted file or directory subtree from the index.
    fn handle_deleted_event(db: &Database, path: &Path) -> bool {
        debug!("File deleted: {}", path.display());
        let path_str = path.to_string_lossy();
        let is_directory = match db.get_file_by_path(&path_str) {
            Ok(entry) => entry.is_some_and(|entry| entry.is_directory),
            Err(error) => {
                error!("Failed to look up deleted path {}: {}", path.display(), error);
                return false;
            }
        };
        let result = if is_directory {
            db.delete_files_under_path(&path_str).map(|_| ())
        } else {
            db.delete_file_by_path(&path_str).map(|_| ())
        };
        if let Err(error) = result {
            error!("Failed to delete {}: {}", path.display(), error);
            return false;
        }
        true
    }

    /// Update the index for a renamed file or directory, walking incoming directories if needed.
    fn handle_renamed_event(&self, db: &Database, from: &Path, to: &Path, file_reference: Option<u64>) -> bool {
        debug!("File renamed: {} -> {}", from.display(), to.display());

        let from_str = from.to_string_lossy();
        let previous = match db.get_file_by_path(&from_str) {
            Ok(previous) => previous,
            Err(error) => {
                error!("Failed to look up renamed path {}: {}", from.display(), error);
                return false;
            }
        };
        let to_str = to.to_string_lossy();
        if !self.should_index_path(&to_str) {
            let is_directory = previous.is_some_and(|entry| entry.is_directory);
            let result = if is_directory {
                db.delete_files_under_path(&from_str).map(|_| ())
            } else {
                db.delete_file_by_path(&from_str).map(|_| ())
            };
            if let Err(error) = result {
                error!(
                    "Failed to remove out-of-scope renamed path {}: {}",
                    from.display(),
                    error
                );
                return false;
            }
            return true;
        }

        if previous.is_some_and(|entry| entry.is_directory) {
            return Self::rename_indexed_directory(db, from, to);
        }

        if let Err(error) = db.delete_file_by_path(&from_str) {
            error!("Failed to delete old path {}: {}", from.display(), error);
            return false;
        }

        let Some(volume_id) = self.get_volume_id_for_path(to) else {
            trace!("No volume found for renamed path: {}", to.display());
            return false;
        };

        if let Ok(metadata) = std::fs::metadata(to) {
            let mut entry = FileEntry::from_metadata(volume_id, to, &metadata);
            entry.mft_reference = file_reference;

            if let Err(error) = db.insert_file(&entry) {
                error!("Failed to insert renamed file {}: {}", to.display(), error);
                return false;
            }
            if metadata.is_dir() && !self.index_directory_descendants(db, volume_id, to) {
                return false;
            }
        }
        true
    }

    /// Walk and index all in-scope descendants of a newly arrived or renamed directory.
    fn index_directory_descendants(&self, db: &Database, volume_id: i64, root_dir: &Path) -> bool {
        let mut stack = vec![root_dir.to_path_buf()];
        let mut batch = Vec::new();
        let mut ok = true;

        while let Some(current_dir) = stack.pop() {
            let entries = match std::fs::read_dir(&current_dir) {
                Ok(entries) => entries,
                Err(error) => {
                    warn!("Could not walk incoming directory {}: {}", current_dir.display(), error);
                    ok = false;
                    continue;
                }
            };

            for entry_result in entries {
                let Ok(dir_entry) = entry_result else {
                    ok = false;
                    continue;
                };
                let child_path = dir_entry.path();
                let Some(child_str) = child_path.to_str() else {
                    warn!(
                        "Skipping non-UTF-8 path in incoming directory: {}",
                        child_path.display()
                    );
                    continue;
                };
                if !self.should_index_path(child_str) {
                    continue;
                }
                let Ok(metadata) = dir_entry.metadata() else {
                    continue;
                };
                let is_dir = metadata.is_dir();
                batch.push(FileEntry::from_metadata(volume_id, &child_path, &metadata));
                if is_dir {
                    stack.push(child_path);
                }
            }
        }

        for entry in &batch {
            if let Err(error) = db.insert_file(entry) {
                error!(
                    "Failed to index descendant {} of incoming directory {}: {}",
                    entry.full_path,
                    root_dir.display(),
                    error
                );
                return false;
            }
        }
        ok
    }

    /// Move an indexed directory and every entry under it to the renamed path in one transaction.
    fn rename_indexed_directory(db: &Database, from: &Path, to: &Path) -> bool {
        let from_str = from.to_string_lossy();
        let to_str = to.to_string_lossy();
        let new_name = filefind_common::scope::extract_entry_name(&to_str);

        match db.reconcile_moved_directory(&from_str, &to_str, new_name) {
            Ok(moved) => {
                debug!(
                    "Moved {} indexed entries from {} to {}",
                    moved,
                    from.display(),
                    to.display()
                );
                true
            }
            Err(error) => {
                error!(
                    "Failed to move indexed directory {} -> {}: {}",
                    from.display(),
                    to.display(),
                    error
                );
                false
            }
        }
    }

    /// Get the most specific indexed volume ID for `path` using component-aware mount matching.
    fn get_volume_id_for_path(&self, path: &std::path::Path) -> Option<i64> {
        let path_str = path.to_string_lossy();
        if let Some(volume) = find_most_specific_volume(&path_str, &self.volume_mounts) {
            return Some(volume.volume_id);
        }

        let db = self.database.as_ref()?;
        db.get_volume_id_for_path(&path_str).ok().flatten()
    }

    /// Build the path cache from the database for faster path resolution.
    fn build_path_cache(&mut self) -> Result<()> {
        let Some(ref db) = self.database else {
            return Ok(());
        };

        let volumes = db.get_all_volumes()?;
        self.volume_mounts = volumes
            .into_iter()
            .filter_map(|volume| {
                volume.id.map(|volume_id| VolumeMount {
                    volume_id,
                    mount_point: volume.mount_point,
                })
            })
            .collect();

        // Query all directories with MFT references
        self.path_cache.clear();
        let connection = db.connection();
        let mut stmt = connection.prepare(
            "SELECT volumes.mount_point, files.mft_reference, files.full_path FROM files \
             JOIN volumes ON files.volume_id = volumes.id \
             WHERE files.is_directory = 1 AND files.mft_reference IS NOT NULL",
        )?;

        let entries = stmt.query_map([], |row| {
            let mount_point: String = row.get(0)?;
            let mft_ref: i64 = row.get(1)?;
            let path: String = row.get(2)?;
            Ok(CachedDirectoryRow {
                drive_letter: mount_point.chars().next().map(|drive| drive.to_ascii_uppercase()),
                mft_reference: mft_ref as u64,
                path,
            })
        })?;

        for entry in entries {
            let row = entry?;
            if let Some(drive_letter) = row.drive_letter {
                self.path_cache.insert(
                    DirectoryCacheKey {
                        drive_letter,
                        mft_reference: row.mft_reference,
                    },
                    row.path,
                );
            }
        }

        info!("Built path cache with {} directory entries", self.path_cache.len());

        Ok(())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Ensure shutdown is signalled
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            foreground: false,
            rescan: false,
            usn_poll_interval_ms: DEFAULT_USN_POLL_INTERVAL_MS,
            watcher_debounce_ms: DEFAULT_WATCHER_DEBOUNCE_MS,
        }
    }
}

/// Start the daemon process.
///
/// This is the main entry point called from CLI.
pub fn start_daemon(options: &DaemonOptions, config: &Config) -> Result<()> {
    validate_configured_roots(config)?;
    let mut daemon = Daemon::new(config.clone(), options.clone());

    if options.foreground {
        if config.daemon.verbose {
            print_cyan!("Starting daemon in foreground mode...");
        }

        // Run in foreground with tokio runtime
        tokio::runtime::Runtime::new()?.block_on(async {
            // Set up Ctrl+C handler
            let shutdown = daemon.shutdown_handle();
            tokio::spawn(async move {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    error!("Failed to listen for Ctrl+C: {}", error);
                }
                info!("Received Ctrl+C, shutting down...");
                shutdown.store(true, Ordering::Relaxed);
            });

            daemon.run().await
        })
    } else {
        // Spawn a detached background process
        spawn_background_daemon(options.rescan)?;
        Ok(())
    }
}

/// Spawn the daemon as a detached background process.
///
/// This function spawns a new instance of the daemon executable with the `-f` (foreground)
/// flag, but detached from the current console. The spawned process runs independently
/// and survives after this process exits.
///
/// # Platform Support
///
/// - **Windows**: Uses `DETACHED_PROCESS` and `CREATE_NO_WINDOW` creation flags
/// - **Unix**: Uses a new process group with detached standard streams
fn spawn_background_daemon(rescan: bool) -> Result<()> {
    let client = IpcClient::new();
    if client.is_daemon_running() {
        anyhow::bail!("Daemon is already running");
    }

    let exe_path = std::env::current_exe().context("Failed to get current executable path")?;

    // Run in foreground mode in the detached process
    let mut command = Command::new(&exe_path);
    command.arg("start").arg("-f");

    if rescan {
        command.arg("--rescan");
    }

    // Detach from console and create without a window
    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    // Detach from the terminal's process group and standard streams
    #[cfg(not(windows))]
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command.spawn().context("Failed to spawn background daemon process")?;
    let pid = child.id();

    // Poll briefly so immediate startup failures (such as a bind conflict) surface to the caller.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if client.is_daemon_running() {
            break;
        }
        if let Some(status) = child.try_wait().context("Failed to check background daemon status")? {
            anyhow::bail!("Background daemon exited immediately with status {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    print_green!("Daemon started in background (PID: {})", pid);
    print_cyan!("Use 'filefindd status' to check daemon status");
    print_cyan!("Use 'filefindd stop' to stop the daemon");

    Ok(())
}

/// Stop the running daemon.
pub fn stop_daemon() {
    print_cyan!("Stopping daemon...");

    let client = IpcClient::new();

    if !client.is_daemon_running() {
        print_yellow!("Daemon is not running");
        return;
    }

    match client.stop_daemon() {
        Ok(()) => {
            print_green!("Stop command sent to daemon");
            // Wait a moment and check if it stopped
            std::thread::sleep(std::time::Duration::from_millis(500));
            if client.is_daemon_running() {
                print_yellow!("Daemon is still running - it may take a moment to shut down");
            } else {
                print_green!("Daemon stopped");
            }
        }
        Err(error) => {
            print_error!("Failed to stop daemon: {}", error);
        }
    }
}

/// Show daemon status.
pub fn show_status(config: &Config) -> Result<()> {
    println!("{}", "Filefind daemon status".bold());

    let client = IpcClient::new();

    // Try to get status from running daemon
    if let Ok(status) = client.get_status() {
        let state_str = match status.state {
            DaemonStateInfo::Running => "running".green(),
            DaemonStateInfo::Scanning => "scanning".cyan(),
            DaemonStateInfo::Starting => "starting".yellow(),
            DaemonStateInfo::Stopping => "stopping".yellow(),
            DaemonStateInfo::Stopped => "stopped".red(),
        };

        println!("  State:        {state_str}");
        println!("  Files:        {}", format_number(status.indexed_files));
        println!("  Directories:  {}", format_number(status.indexed_directories));
        println!("  Volumes:      {}", status.monitored_volumes);
        println!("  Uptime:       {}s", status.uptime_seconds);
        if status.is_paused {
            println!("  Paused:       {}", "yes".yellow());
        }
    } else {
        // Daemon not running - show database info if available
        println!("  State:        {}", "not running".red());

        let database_path = config.database_path();
        if database_path.exists() {
            let database = Database::open(&database_path)?;
            let stats = database.get_stats()?;

            println!();
            println!("  {} (from database)", "Index Statistics".bold());
            println!("  Files:        {}", format_number(stats.total_files));
            println!("  Directories:  {}", format_number(stats.total_directories));
            println!("  Volumes:      {}", stats.volume_count);
        } else {
            println!();
            println!("  Database not found at: {}", database_path.display());
            println!("  Run 'filefindd scan' to create the index.");
        }
    }

    println!();

    let database_path = config.database_path();
    if database_path.exists() {
        println!("  Database:     {}", database_path.display());
        if let Ok(metadata) = std::fs::metadata(&database_path) {
            println!("  DB size:      {}", cli_tools_common::format_size(metadata.len()));
        }
    }

    Ok(())
}

/// Detect available drives on Windows or show configured index roots and accessibility on Unix.
pub fn detect_drives(config: &Config) {
    #[cfg(windows)]
    {
        let _ = config;
        println!("{}", "Detected Drives".bold());
        println!();

        let volumes = detect_ntfs_volumes();
        if volumes.is_empty() {
            println!("  No NTFS volumes detected");
        } else {
            println!("  NTFS volumes (fast MFT scanning available):");
            for letter in &volumes {
                println!("    {letter}:\\");
            }
        }

        println!();
        print_cyan!("Run 'filefindd scan' to index all detected NTFS volumes");
        print_cyan!("Run 'filefindd scan <path>' to scan a specific directory");
    }

    #[cfg(not(windows))]
    {
        let config_path = Config::expected_config_path();
        println!("{}", "Configured Index Roots".bold());
        println!("  Config: {}", config_path.display());
        println!();

        if config.daemon.paths.is_empty() {
            println!("  No paths configured in [daemon].paths.");
            println!();
            println!("  Add explicit absolute directories to {}:", config_path.display());
            println!("    [daemon]");
            println!("    paths = [\"/Users/you/Documents\", \"/Volumes/Media\"]");
        } else {
            for root in &config.daemon.paths {
                let path = Path::new(root);
                let status = if !path.is_absolute() {
                    "invalid (must be absolute)".red().to_string()
                } else if path.is_dir() {
                    "accessible".green().to_string()
                } else {
                    "inaccessible".yellow().to_string()
                };
                println!("  {root}  [{status}]");
            }
        }

        println!();
        print_cyan!("Run 'filefindd scan' to index all configured paths");
        print_cyan!("Run 'filefindd scan <path>' to scan a specific directory");
    }
}

/// Show index statistics.
pub fn show_stats(config: &Config) -> Result<()> {
    let database_path = config.database_path();

    if !database_path.exists() {
        print_error!("Database not found at: {}", database_path.display());
        println!("Run 'filefindd scan' to create the index.");
        return Ok(());
    }

    let database = Database::open(&database_path)?;
    let stats = database.get_stats()?;

    println!("{}", "Filefind index".bold());
    println!("  Files:          {}", format_number(stats.total_files));
    println!("  Directories:    {}", format_number(stats.total_directories));
    println!("  Volumes:        {}", stats.volume_count);
    println!("  Total size:     {}", cli_tools_common::format_size(stats.total_size));
    println!();
    println!("  Database:       {}", database_path.display());

    // Show database file size
    if let Ok(metadata) = std::fs::metadata(&database_path) {
        println!("  DB file size:   {}", cli_tools_common::format_size(metadata.len()));
    }

    Ok(())
}

/// List indexed volumes.
pub fn list_volumes(database_path: &Path, detailed: bool) -> Result<()> {
    if !database_path.exists() {
        print_error!("Database not found at: {}", database_path.display());
        println!("Run 'filefindd scan' to create the index.");
        return Ok(());
    }

    let database = Database::open(database_path)?;
    let volumes = database.get_all_volumes()?;

    if volumes.is_empty() {
        println!("No volumes indexed yet.");
        println!("Run 'filefindd scan' to index your drives.");
        return Ok(());
    }

    println!("{}", "Indexed Volumes:".bold());

    for volume in &volumes {
        let status = if volume.is_online {
            "online".green()
        } else {
            "offline".red()
        };

        println!("  {} ({}) - {}", volume.mount_point.bold(), volume.volume_type, status);

        if detailed {
            if let Some(ref label) = volume.label {
                println!("    Label: {label}");
            }
            println!("    Serial: {}", volume.serial_number);
            if let Some(last_scan) = volume.last_scan_time {
                println!("    Last scan: {last_scan:?}");
            }
            if let Some(usn) = volume.last_usn {
                println!("    Last USN: {usn}");
            }
            println!();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use filefind_common::{FileEntry, IndexedVolume, VolumeType};
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_usn_scope_respects_roots_and_exclusions() {
        let mut config = Config::default();
        config.daemon.paths = vec!["C:\\Docs".into()];
        config.daemon.exclude = vec!["*.tmp".into()];
        let daemon = Daemon::new(config, DaemonOptions::default());

        assert!(daemon.should_index_path("c:\\docs\\report.txt"));
        assert!(!daemon.should_index_path("C:\\Documents\\report.txt"));
        assert!(!daemon.should_index_path("C:\\Docs\\draft.tmp"));
        assert!(!daemon.should_index_path("D:\\Docs\\report.txt"));
    }

    #[test]
    fn test_usn_directory_moved_out_of_scope_removes_indexed_subtree() {
        let mut config = Config::default();
        config.daemon.paths = vec!["C:\\Docs".into()];
        let mut daemon = Daemon::new(config, DaemonOptions::default());
        let database = Database::open_in_memory().expect("open database");
        let volume = IndexedVolume::new("USN-SCOPE".into(), "C:".into(), VolumeType::Ntfs);
        let volume_id = database.upsert_volume(&volume).expect("insert volume");
        for (name, path) in [("Old", "C:\\Docs\\Old"), ("child.txt", "C:\\Docs\\Old\\child.txt")] {
            database
                .insert_file(&FileEntry::new(volume_id, name.into(), path.into(), name == "Old"))
                .expect("insert entry");
        }
        daemon.database = Some(database);
        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 42,
            },
            "C:\\Docs\\Old".into(),
        );
        let change = UsnChange {
            usn: 1,
            file_reference: 42,
            parent_reference: 5,
            name: "Old".into(),
            reason: 0x0000_2000,
            attributes: 0x10,
            is_directory: true,
        };

        assert!(daemon.handle_out_of_scope_change('C', &change, "C:\\Other\\Old".into()));
        assert_eq!(
            daemon
                .path_cache
                .get(&DirectoryCacheKey {
                    drive_letter: 'C',
                    mft_reference: 42,
                })
                .map(String::as_str),
            Some("C:\\Other\\Old")
        );
        let database = daemon.database.as_ref().expect("database");
        assert!(
            database
                .search_by_path("C:\\Docs\\Old\\child.txt", 1)
                .expect("search")
                .is_empty()
        );
    }

    #[test]
    fn test_daemon_state_display() {
        assert_eq!(DaemonState::Stopped.to_string(), "stopped");
        assert_eq!(DaemonState::Starting.to_string(), "starting");
        assert_eq!(DaemonState::Running.to_string(), "running");
        assert_eq!(DaemonState::Stopping.to_string(), "stopping");
    }

    #[test]
    fn test_daemon_state_display_all_variants() {
        // Verify all variants have distinct display strings
        let states = [
            DaemonState::Stopped,
            DaemonState::Starting,
            DaemonState::Running,
            DaemonState::Stopping,
        ];

        let displays: Vec<String> = states.iter().map(std::string::ToString::to_string).collect();

        // All should be unique
        for (index, display) in displays.iter().enumerate() {
            for (index_other, other) in displays.iter().enumerate() {
                if index != index_other {
                    assert_ne!(display, other, "State displays should be unique");
                }
            }
        }
    }

    #[test]
    fn test_daemon_state_debug() {
        let state = DaemonState::Running;
        let debug_str = format!("{state:?}");
        assert!(debug_str.contains("Running"));
    }

    #[test]
    fn test_daemon_state_clone_copy() {
        let state = DaemonState::Starting;
        let copied = state;
        assert_eq!(state, copied);
    }

    #[test]
    fn test_daemon_state_equality() {
        assert_eq!(DaemonState::Stopped, DaemonState::Stopped);
        assert_ne!(DaemonState::Stopped, DaemonState::Running);
        assert_ne!(DaemonState::Starting, DaemonState::Stopping);
    }

    #[test]
    fn test_daemon_options_default() {
        let options = DaemonOptions::default();
        assert!(!options.rescan);
        assert_eq!(options.usn_poll_interval_ms, DEFAULT_USN_POLL_INTERVAL_MS);
        assert_eq!(options.watcher_debounce_ms, DEFAULT_WATCHER_DEBOUNCE_MS);
    }

    #[test]
    fn test_daemon_options_custom() {
        let options = DaemonOptions {
            foreground: true,
            rescan: true,
            usn_poll_interval_ms: 500,
            watcher_debounce_ms: 200,
        };

        assert!(options.foreground);
        assert!(options.rescan);
        assert_eq!(options.usn_poll_interval_ms, 500);
        assert_eq!(options.watcher_debounce_ms, 200);
    }

    #[test]
    fn test_daemon_new() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        assert_eq!(daemon.state(), DaemonState::Stopped);
        assert!(!daemon.is_running());
        assert!(daemon.database.is_none());
        assert!(daemon.volume_monitors.is_empty());
        assert!(daemon.file_watcher.is_none());
    }

    #[test]
    fn test_daemon_new_with_rescan_option() {
        let config = Config::default();
        let options = DaemonOptions {
            rescan: true,
            ..Default::default()
        };
        let daemon = Daemon::new(config, options);

        assert!(daemon.options.rescan);
        assert_eq!(daemon.state(), DaemonState::Stopped);
    }

    #[test]
    fn test_daemon_options_watcher_debounce_range() {
        // Test that custom debounce intervals work
        let options = DaemonOptions {
            watcher_debounce_ms: 1,
            ..Default::default()
        };
        assert_eq!(options.watcher_debounce_ms, 1);

        let options = DaemonOptions {
            watcher_debounce_ms: 10000,
            ..Default::default()
        };
        assert_eq!(options.watcher_debounce_ms, 10000);
    }

    #[test]
    fn test_daemon_options_usn_poll_interval_range() {
        // Test that custom poll intervals work
        let options = DaemonOptions {
            usn_poll_interval_ms: 100,
            ..Default::default()
        };
        assert_eq!(options.usn_poll_interval_ms, 100);

        let options = DaemonOptions {
            usn_poll_interval_ms: 5000,
            ..Default::default()
        };
        assert_eq!(options.usn_poll_interval_ms, 5000);
    }

    #[test]
    fn test_daemon_shutdown_handle() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let shutdown = daemon.shutdown_handle();
        assert!(!shutdown.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_daemon_shutdown_handle_set() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let shutdown = daemon.shutdown_handle();

        // Initially not set
        assert!(!shutdown.load(std::sync::atomic::Ordering::Relaxed));

        // Set shutdown
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);

        // Verify it's set
        assert!(shutdown.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_daemon_initial_state() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // Verify initial state is Stopped
        assert_eq!(daemon.state(), DaemonState::Stopped);
        assert!(!daemon.is_running());

        // Verify no monitors are set up
        assert!(daemon.volume_monitors.is_empty());
        assert!(daemon.file_watcher.is_none());
        assert!(daemon.watcher_receiver.is_none());

        // Verify path cache is empty
        assert!(daemon.path_cache.is_empty());
    }

    #[test]
    fn test_daemon_is_paused_initially_false() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        assert!(!daemon.is_paused);
    }

    #[test]
    fn test_daemon_options_debug() {
        let options = DaemonOptions::default();
        let debug_str = format!("{options:?}");
        assert!(debug_str.contains("DaemonOptions"));
        assert!(debug_str.contains("rescan"));
    }

    #[test]
    fn test_volume_monitor_struct() {
        // VolumeMonitor is private, but we can test it exists via its usage in Daemon
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // Volume monitors should be empty initially
        assert!(daemon.volume_monitors.is_empty());
    }

    #[test]
    fn test_daemon_state_not_running_when_stopped() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        assert_eq!(daemon.state(), DaemonState::Stopped);
        assert!(!daemon.is_running());
    }

    #[test]
    fn test_daemon_ipc_state_initialized() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // IPC state should be initialized
        assert_eq!(daemon.ipc_state.state.load(), filefind_common::DaemonStateInfo::Stopped);
        assert!(!daemon.ipc_state.is_paused.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_get_configured_drives_empty() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, [] as [char; 0]);
    }

    #[test]
    fn test_get_configured_drives_single_drive() {
        let mut config = Config::default();
        config.daemon.paths = vec!["C:\\Users".to_string()];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, vec!['C']);
    }

    #[test]
    fn test_get_configured_drives_multiple_drives() {
        let mut config = Config::default();
        config.daemon.paths = vec![
            "C:\\Users".to_string(),
            "D:\\Projects".to_string(),
            "E:\\Data".to_string(),
        ];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, vec!['C', 'D', 'E']);
    }

    #[test]
    fn test_get_configured_drives_deduplicates() {
        let mut config = Config::default();
        config.daemon.paths = vec![
            "C:\\Users".to_string(),
            "C:\\Projects".to_string(),
            "D:\\Data".to_string(),
            "C:\\Documents".to_string(),
        ];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, vec!['C', 'D']);
    }

    #[test]
    fn test_get_configured_drives_lowercase_normalized() {
        let mut config = Config::default();
        config.daemon.paths = vec!["c:\\Users".to_string(), "d:\\Projects".to_string()];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, vec!['C', 'D']);
    }

    #[test]
    fn test_get_configured_drives_ignores_unc_paths() {
        let mut config = Config::default();
        config.daemon.paths = vec![
            "C:\\Users".to_string(),
            "\\\\server\\share".to_string(),
            "\\\\192.168.1.1\\data".to_string(),
        ];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, vec!['C']);
    }

    #[test]
    fn test_get_configured_drives_drive_letter_only() {
        let mut config = Config::default();
        config.daemon.paths = vec!["C:".to_string(), "D:\\".to_string()];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let drives = daemon.get_configured_drives();
        assert_eq!(drives, vec!['C', 'D']);
    }

    // --- stop() tests ---

    #[test]
    fn test_stop_already_stopped_daemon() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // Daemon starts in Stopped state
        assert_eq!(daemon.state(), DaemonState::Stopped);

        // Stopping an already stopped daemon should be a no-op
        daemon.stop();
        assert_eq!(daemon.state(), DaemonState::Stopped);
    }

    #[test]
    fn test_stop_sets_shutdown_signal() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // Manually set state to Running to test stop behavior
        daemon.state = DaemonState::Running;
        daemon.ipc_state.state.store(DaemonStateInfo::Running);

        let shutdown = daemon.shutdown_handle();
        assert!(!shutdown.load(std::sync::atomic::Ordering::Relaxed));

        daemon.stop();

        assert!(shutdown.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(daemon.state(), DaemonState::Stopped);
        assert_eq!(daemon.ipc_state.state.load(), DaemonStateInfo::Stopped);
    }

    #[test]
    fn test_stop_clears_volume_monitors() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // Add a fake volume monitor without a USN monitor
        daemon.volume_monitors.insert(
            'C',
            VolumeMonitor {
                usn_monitor: None,
                last_usn: 100,
            },
        );
        daemon.state = DaemonState::Running;

        assert!(!daemon.volume_monitors.is_empty());
        daemon.stop();
        assert!(daemon.volume_monitors.is_empty());
        assert_eq!(daemon.state(), DaemonState::Stopped);
    }

    #[test]
    fn test_stop_transitions_through_stopping_state() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);
        daemon.state = DaemonState::Running;

        daemon.stop();

        // After stop completes, state should be Stopped (it transitioned through Stopping)
        assert_eq!(daemon.state(), DaemonState::Stopped);
        assert_eq!(daemon.ipc_state.state.load(), DaemonStateInfo::Stopped);
    }

    // --- should_perform_initial_scan() tests ---

    #[test]
    fn test_should_perform_initial_scan_no_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // No database set => should scan
        let result = daemon.should_perform_initial_scan().expect("should not error");
        assert!(result);
    }

    #[test]
    fn test_should_perform_initial_scan_empty_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        daemon.database = Some(database);

        // Empty database => should scan
        let result = daemon.should_perform_initial_scan().expect("should not error");
        assert!(result);
    }

    #[test]
    fn test_should_perform_initial_scan_populated_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        // Add a volume and a file
        let volume = IndexedVolume {
            id: None,
            serial_number: "SERIAL-001".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        let entry = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "test.txt".to_string(),
            full_path: "C:\\test.txt".to_string(),
            size: 10,
            is_directory: false,
            created_time: None,
            modified_time: None,
            mft_reference: None,
        };
        database.insert_file(&entry).expect("Failed to insert file");

        daemon.database = Some(database);

        // Database has data => should NOT scan
        let result = daemon.should_perform_initial_scan().expect("should not error");
        assert!(!result);
    }

    #[test]
    fn test_should_perform_initial_scan_only_directories() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let volume = IndexedVolume {
            id: None,
            serial_number: "SERIAL-002".to_string(),
            label: None,
            mount_point: "D:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        let dir_entry = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "docs".to_string(),
            full_path: "D:\\docs".to_string(),
            size: 0,
            is_directory: true,
            created_time: None,
            modified_time: None,
            mft_reference: None,
        };
        database.insert_file(&dir_entry).expect("Failed to insert directory");

        daemon.database = Some(database);

        // Has directories but no files => should NOT scan (total_files=0 but total_directories=1)
        let result = daemon.should_perform_initial_scan().expect("should not error");
        assert!(!result);
    }

    // --- get_non_ntfs_paths() tests ---

    #[test]
    fn test_get_non_ntfs_paths_empty_config() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let paths = daemon.get_non_ntfs_paths();
        assert_eq!(paths, [] as [std::path::PathBuf; 0]);
    }

    #[test]
    fn test_get_non_ntfs_paths_with_monitored_ntfs_drive() {
        let mut config = Config::default();
        config.daemon.paths = vec!["C:\\Users".to_string()];
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // Simulate that C: is being monitored via USN
        daemon.volume_monitors.insert(
            'C',
            VolumeMonitor {
                usn_monitor: None,
                last_usn: 0,
            },
        );

        let paths = daemon.get_non_ntfs_paths();
        assert_eq!(paths, [] as [std::path::PathBuf; 0]);
    }

    #[test]
    fn test_get_non_ntfs_paths_unc_paths() {
        let mut config = Config::default();
        config.daemon.paths = vec!["\\\\server\\share".to_string(), "\\\\192.168.1.1\\data".to_string()];
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let paths = daemon.get_non_ntfs_paths();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("\\\\server\\share"));
        assert_eq!(paths[1], PathBuf::from("\\\\192.168.1.1\\data"));
    }

    #[test]
    fn test_get_non_ntfs_paths_mixed() {
        let mut config = Config::default();
        config.daemon.paths = vec![
            "C:\\Users".to_string(),
            "\\\\server\\share".to_string(),
            "D:\\Data".to_string(),
        ];
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // Only C: is monitored
        daemon.volume_monitors.insert(
            'C',
            VolumeMonitor {
                usn_monitor: None,
                last_usn: 0,
            },
        );

        let paths = daemon.get_non_ntfs_paths();
        // D:\Data and \\server\share should be returned (C: is monitored)
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&PathBuf::from("\\\\server\\share")));
        assert!(paths.contains(&PathBuf::from("D:\\Data")));
    }

    #[test]
    fn test_get_non_ntfs_paths_all_monitored() {
        let mut config = Config::default();
        config.daemon.paths = vec!["C:\\Users".to_string(), "D:\\Data".to_string()];
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        daemon.volume_monitors.insert(
            'C',
            VolumeMonitor {
                usn_monitor: None,
                last_usn: 0,
            },
        );
        daemon.volume_monitors.insert(
            'D',
            VolumeMonitor {
                usn_monitor: None,
                last_usn: 0,
            },
        );

        let paths = daemon.get_non_ntfs_paths();
        assert_eq!(paths, [] as [std::path::PathBuf; 0]);
    }

    // --- usn_change_to_event() tests ---

    /// Helper to create a `UsnChange` with the specified reason flags.
    fn make_usn_change(reason: u32) -> UsnChange {
        UsnChange {
            usn: 1000,
            file_reference: 42,
            parent_reference: 5,
            name: "test.txt".to_string(),
            reason,
            attributes: 0x20,
            is_directory: false,
        }
    }

    #[test]
    fn test_usn_change_to_event_create() {
        let change = make_usn_change(0x0000_0100); // USN_REASON_FILE_CREATE
        let event = Daemon::usn_change_to_event(&change, "C:\\test.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Created(path) => assert_eq!(path, PathBuf::from("C:\\test.txt")),
            other => panic!("Expected Created event, got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_delete() {
        let change = make_usn_change(0x0000_0200); // USN_REASON_FILE_DELETE
        let event = Daemon::usn_change_to_event(&change, "C:\\deleted.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Deleted(path) => assert_eq!(path, PathBuf::from("C:\\deleted.txt")),
            other => panic!("Expected Deleted event, got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_rename_new() {
        let change = make_usn_change(0x0000_2000); // USN_REASON_RENAME_NEW_NAME
        let event = Daemon::usn_change_to_event(&change, "C:\\renamed.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Created(path) => assert_eq!(path, PathBuf::from("C:\\renamed.txt")),
            other => panic!("Expected Created event (from rename), got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_modify_overwrite() {
        let change = make_usn_change(0x0000_0001); // USN_REASON_DATA_OVERWRITE
        let event = Daemon::usn_change_to_event(&change, "C:\\modified.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Modified(path) => assert_eq!(path, PathBuf::from("C:\\modified.txt")),
            other => panic!("Expected Modified event, got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_modify_extend() {
        let change = make_usn_change(0x0000_0002); // USN_REASON_DATA_EXTEND
        let event = Daemon::usn_change_to_event(&change, "C:\\extended.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Modified(path) => assert_eq!(path, PathBuf::from("C:\\extended.txt")),
            other => panic!("Expected Modified event, got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_modify_truncation() {
        let change = make_usn_change(0x0000_0004); // USN_REASON_DATA_TRUNCATION
        let event = Daemon::usn_change_to_event(&change, "C:\\truncated.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Modified(path) => assert_eq!(path, PathBuf::from("C:\\truncated.txt")),
            other => panic!("Expected Modified event, got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_unrecognized_reason() {
        // Security change only - not create, delete, rename, or modify
        let change = make_usn_change(0x0000_0800); // USN_REASON_SECURITY_CHANGE
        let event = Daemon::usn_change_to_event(&change, "C:\\secure.txt");
        assert!(event.is_none());
    }

    #[test]
    fn test_usn_change_to_event_priority_create_over_modify() {
        // Both create and modify flags set - create should win (checked first)
        let change = make_usn_change(0x0000_0100 | 0x0000_0001); // FILE_CREATE | DATA_OVERWRITE
        let event = Daemon::usn_change_to_event(&change, "C:\\both.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Created(path) => assert_eq!(path, PathBuf::from("C:\\both.txt")),
            other => panic!("Expected Created event (create takes priority), got {other:?}"),
        }
    }

    #[test]
    fn test_usn_change_to_event_delete_over_rename() {
        // Both delete and rename flags - delete should win (checked first)
        let change = make_usn_change(0x0000_0200 | 0x0000_2000); // FILE_DELETE | RENAME_NEW_NAME
        let event = Daemon::usn_change_to_event(&change, "C:\\gone.txt");
        assert!(event.is_some());
        match event.expect("expected event") {
            FileChangeEvent::Deleted(path) => assert_eq!(path, PathBuf::from("C:\\gone.txt")),
            other => panic!("Expected Deleted event (delete checked before rename), got {other:?}"),
        }
    }

    // --- resolve_usn_path_inner() tests ---

    #[test]
    fn test_resolve_usn_path_inner_cache_hit() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 100,
            },
            "C:\\Users\\Documents".to_string(),
        );

        let result = daemon.resolve_usn_path_inner('C', 100, "report.pdf");
        assert_eq!(result, Some("C:\\Users\\Documents\\report.pdf".to_string()));
    }

    #[test]
    fn test_resolve_usn_path_inner_reference_shared_by_drives() {
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 100,
            },
            "C:\\Data".to_string(),
        );
        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'D',
                mft_reference: 100,
            },
            "D:\\Backup".to_string(),
        );

        assert_eq!(
            daemon.resolve_usn_path_inner('C', 100, "file.txt").as_deref(),
            Some("C:\\Data\\file.txt")
        );
        assert_eq!(
            daemon.resolve_usn_path_inner('D', 100, "file.txt").as_deref(),
            Some("D:\\Backup\\file.txt")
        );
    }

    #[test]
    fn test_resolve_usn_path_inner_uncached_parent_in_database() {
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        let database = Database::open_in_memory().expect("open database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "PARENT".into(),
            label: None,
            mount_point: "C:".into(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("insert volume");
        let mut directory = FileEntry::new(volume_id, "parent".into(), "C:\\parent".into(), true);
        directory.mft_reference = Some(123);
        database.insert_file(&directory).expect("insert parent");
        daemon.database = Some(database);

        assert_eq!(
            daemon.resolve_usn_path_inner('C', 123, "new.txt").as_deref(),
            Some("C:\\parent\\new.txt")
        );
        assert!(daemon.resolve_usn_path_inner('D', 123, "new.txt").is_none());
    }

    #[test]
    fn test_usn_directory_create_rename_and_delete_subtree() {
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        let database = Database::open_in_memory().expect("open database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "USN-DIRECTORY".into(),
            label: None,
            mount_point: "C:".into(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("insert volume");
        for (name, path) in [
            ("old", "C:\\old"),
            ("child", "C:\\old\\child"),
            ("file.txt", "C:\\old\\child\\file.txt"),
            ("other.txt", "C:\\old_extra\\other.txt"),
        ] {
            database
                .insert_file(&FileEntry::new(
                    volume_id,
                    name.into(),
                    path.into(),
                    name != "file.txt" && name != "other.txt",
                ))
                .expect("insert entry");
        }
        daemon.database = Some(database);

        let mut directory = make_usn_change(0x0000_0100);
        directory.is_directory = true;
        directory.file_reference = 100;
        daemon.update_directory_index('C', &directory, "C:\\old");
        directory.file_reference = 200;
        daemon.update_directory_index('C', &directory, "C:\\old\\child");
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 200, "file.txt").as_deref(),
            Some("C:\\old\\child\\file.txt")
        );

        directory.file_reference = 100;
        directory.reason = 0x0000_2000;
        directory.name = "new".into();
        daemon.update_directory_index('C', &directory, "C:\\new");
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 200, "file.txt").as_deref(),
            Some("C:\\new\\child\\file.txt")
        );
        let database = daemon.database.as_ref().expect("database");
        assert_eq!(
            database
                .search_by_path("C:\\new\\child\\file.txt", 10)
                .expect("search")
                .len(),
            1
        );

        directory.reason = 0x0000_0200;
        daemon.update_directory_index('C', &directory, "C:\\new");
        assert!(!daemon.path_cache.contains_key(&DirectoryCacheKey {
            drive_letter: 'C',
            mft_reference: 200,
        }));
        let database = daemon.database.as_ref().expect("database");
        assert!(
            database
                .search_by_path("C:\\new\\child\\file.txt", 10)
                .expect("search")
                .is_empty()
        );
        assert_eq!(
            database
                .search_by_path("C:\\old_extra\\other.txt", 10)
                .expect("search")
                .len(),
            1
        );
    }

    #[test]
    fn test_resolve_usn_path_inner_root_parent() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // Parent reference 5 is the MFT root directory
        let result = daemon.resolve_usn_path_inner('C', 5, "boot.ini");
        assert_eq!(result, Some("C:\\boot.ini".to_string()));
    }

    #[test]
    fn test_resolve_usn_path_inner_root_parent_other_drive() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let result = daemon.resolve_usn_path_inner('D', 5, "data.db");
        assert_eq!(result, Some("D:\\data.db".to_string()));
    }

    #[test]
    fn test_resolve_usn_path_inner_cache_miss() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // Unknown parent reference, not root
        let result = daemon.resolve_usn_path_inner('C', 999, "unknown.txt");
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_usn_path_inner_multiple_cache_entries() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 10,
            },
            "C:\\Users".to_string(),
        );
        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 20,
            },
            "C:\\Users\\Documents".to_string(),
        );
        daemon.path_cache.insert(
            DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 30,
            },
            "C:\\Program Files".to_string(),
        );

        assert_eq!(
            daemon.resolve_usn_path_inner('C', 10, "file1.txt"),
            Some("C:\\Users\\file1.txt".to_string())
        );
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 20, "file2.doc"),
            Some("C:\\Users\\Documents\\file2.doc".to_string())
        );
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 30, "app.exe"),
            Some("C:\\Program Files\\app.exe".to_string())
        );
    }

    // --- handle_change_event() tests ---

    #[test]
    fn test_handle_change_event_no_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        // Should not panic when database is None
        daemon.handle_change_event(FileChangeEvent::Created(PathBuf::from("C:\\test.txt")));
        daemon.handle_change_event(FileChangeEvent::Modified(PathBuf::from("C:\\test.txt")));
        daemon.handle_change_event(FileChangeEvent::Deleted(PathBuf::from("C:\\test.txt")));
        daemon.handle_change_event(FileChangeEvent::Renamed {
            from: PathBuf::from("C:\\old.txt"),
            to: PathBuf::from("C:\\new.txt"),
        });
    }

    #[test]
    fn test_handle_change_event_modified_with_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        daemon.database = Some(database);

        // Modified events are just logged, no database update
        daemon.handle_change_event(FileChangeEvent::Modified(PathBuf::from("C:\\test.txt")));
        // Should not panic or error
    }

    #[test]
    fn test_handle_change_event_delete_with_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-DEL".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        let entry = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "deleteme.txt".to_string(),
            full_path: "C:\\deleteme.txt".to_string(),
            size: 100,
            is_directory: false,
            created_time: None,
            modified_time: None,
            mft_reference: None,
        };
        database.insert_file(&entry).expect("Failed to insert file");

        daemon.database = Some(database);

        // Delete the file
        daemon.handle_change_event(FileChangeEvent::Deleted(PathBuf::from("C:\\deleteme.txt")));

        // Verify the file was deleted from the database
        let db = daemon.database.as_ref().expect("database should exist");
        let results = db.search_by_name("deleteme.txt", 10).expect("search failed");
        assert!(results.is_empty(), "File should have been deleted from database");
    }

    #[test]
    fn test_handle_change_event_delete_nonexistent_file() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        daemon.database = Some(database);

        // Deleting a file that doesn't exist should not panic
        daemon.handle_change_event(FileChangeEvent::Deleted(PathBuf::from("C:\\nonexistent.txt")));
    }

    #[test]
    fn test_handle_change_event_create_existing_file() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-CREATE".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        database.upsert_volume(&volume).expect("Failed to upsert volume");
        daemon.database = Some(database);

        // Creating a file requires the file to exist on disk for metadata.
        // Use a temp file to test the Created event path.
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let temp_file = temp_dir.path().join("created.txt");
        std::fs::write(&temp_file, "hello").expect("Failed to write temp file");

        // The drive letter won't match our volume mount point, so volume_id lookup will fail
        // (no volume for the temp path's drive), but the code path is still exercised without panic.
        daemon.handle_change_event(FileChangeEvent::Created(temp_file));
    }

    #[test]
    #[cfg(windows)]
    fn test_usn_created_file_can_be_deleted_by_reference() {
        let temp_dir = TempDir::new().expect("create temp directory");
        let file_path = temp_dir.path().join("indexed.txt");
        std::fs::write(&file_path, "contents").expect("create test file");
        let mount_point = file_path.to_string_lossy().chars().take(2).collect::<String>();
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        let database = Database::open_in_memory().expect("open database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-USN-CREATE".to_string(),
            label: None,
            mount_point,
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("insert volume");
        daemon.database = Some(database);

        daemon.handle_change_event_with_reference(FileChangeEvent::Created(file_path.clone()), Some(9876));

        let database = daemon.database.as_ref().expect("database exists");
        assert_eq!(
            database
                .delete_files_by_mft_references(volume_id, &[9876])
                .expect("delete file"),
            1
        );
        assert!(
            database
                .search_by_path(&file_path.to_string_lossy(), 10)
                .expect("search file")
                .is_empty()
        );
    }

    // --- get_volume_id_for_path() tests ---

    #[test]
    fn test_get_volume_id_for_path_no_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);

        let result = daemon.get_volume_id_for_path(Path::new("C:\\test.txt"));
        assert!(result.is_none());
    }

    #[test]
    fn test_get_volume_id_for_path_matching_volume() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-VOL1".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");
        daemon.database = Some(database);

        let result = daemon.get_volume_id_for_path(Path::new("C:\\Users\\file.txt"));
        assert_eq!(result, Some(volume_id));
    }

    #[test]
    fn test_get_volume_id_for_path_no_matching_volume() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-VOL2".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        database.upsert_volume(&volume).expect("Failed to upsert volume");
        daemon.database = Some(database);

        // D: drive is not indexed
        let result = daemon.get_volume_id_for_path(Path::new("D:\\test.txt"));
        assert!(result.is_none());
    }

    #[test]
    fn test_get_volume_id_for_path_case_insensitive() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-CASE".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");
        daemon.database = Some(database);

        // Lowercase c should match C:
        let result = daemon.get_volume_id_for_path(Path::new("c:\\test.txt"));
        assert_eq!(result, Some(volume_id));
    }

    #[test]
    fn test_get_volume_id_for_path_multiple_volumes() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let vol_c = IndexedVolume {
            id: None,
            serial_number: "SN-C".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let c_id = database.upsert_volume(&vol_c).expect("Failed to upsert C:");

        let vol_d = IndexedVolume {
            id: None,
            serial_number: "SN-D".to_string(),
            label: None,
            mount_point: "D:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let d_id = database.upsert_volume(&vol_d).expect("Failed to upsert D:");

        daemon.database = Some(database);

        assert_eq!(daemon.get_volume_id_for_path(Path::new("C:\\file.txt")), Some(c_id));
        assert_eq!(daemon.get_volume_id_for_path(Path::new("D:\\file.txt")), Some(d_id));
        assert!(daemon.get_volume_id_for_path(Path::new("E:\\file.txt")).is_none());
    }

    // --- build_path_cache() tests ---

    #[test]
    fn test_build_path_cache_no_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // Should succeed with no database
        daemon
            .build_path_cache()
            .expect("build_path_cache should succeed without database");
        assert!(daemon.path_cache.is_empty());
    }

    #[test]
    fn test_build_path_cache_empty_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");
        daemon.database = Some(database);

        daemon.build_path_cache().expect("build_path_cache should succeed");
        assert!(daemon.path_cache.is_empty());
    }

    #[test]
    fn test_build_path_cache_with_directories() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-CACHE".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        // Insert directory with MFT reference
        let dir = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "Users".to_string(),
            full_path: "C:\\Users".to_string(),
            size: 0,
            is_directory: true,
            created_time: None,
            modified_time: None,
            mft_reference: Some(100),
        };
        database.insert_file(&dir).expect("Failed to insert directory");

        let dir2 = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "Documents".to_string(),
            full_path: "C:\\Users\\Documents".to_string(),
            size: 0,
            is_directory: true,
            created_time: None,
            modified_time: None,
            mft_reference: Some(200),
        };
        database.insert_file(&dir2).expect("Failed to insert directory");

        // Insert a file (should NOT be in the cache since it's not a directory)
        let file = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "readme.txt".to_string(),
            full_path: "C:\\readme.txt".to_string(),
            size: 50,
            is_directory: false,
            created_time: None,
            modified_time: None,
            mft_reference: Some(300),
        };
        database.insert_file(&file).expect("Failed to insert file");

        daemon.database = Some(database);

        daemon.build_path_cache().expect("build_path_cache should succeed");

        // Only directories with mft_reference should be in cache
        assert_eq!(daemon.path_cache.len(), 2);
        assert_eq!(
            daemon.path_cache.get(&DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 100,
            }),
            Some(&"C:\\Users".to_string())
        );
        assert_eq!(
            daemon.path_cache.get(&DirectoryCacheKey {
                drive_letter: 'C',
                mft_reference: 200,
            }),
            Some(&"C:\\Users\\Documents".to_string())
        );
        assert!(!daemon.path_cache.contains_key(&DirectoryCacheKey {
            drive_letter: 'C',
            mft_reference: 300,
        }));
    }

    #[test]
    fn test_build_path_cache_ignores_directories_without_mft_reference() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-NOMFT".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        let dir = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "NoMft".to_string(),
            full_path: "C:\\NoMft".to_string(),
            size: 0,
            is_directory: true,
            created_time: None,
            modified_time: None,
            mft_reference: None, // No MFT reference
        };
        database.insert_file(&dir).expect("Failed to insert directory");

        daemon.database = Some(database);

        daemon.build_path_cache().expect("build_path_cache should succeed");
        assert!(daemon.path_cache.is_empty());
    }

    // --- process_ipc_commands() tests ---

    #[tokio::test]
    async fn test_process_ipc_commands_no_receiver() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        // No IPC receiver set => should be a no-op
        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");
    }

    #[tokio::test]
    async fn test_process_ipc_commands_stop() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let (sender, receiver) = mpsc::channel(32);
        daemon.ipc_receiver = Some(receiver);

        sender.send(IpcToDaemon::Stop).await.expect("Failed to send");

        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");

        // Shutdown should have been signaled
        assert!(daemon.shutdown.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_process_ipc_commands_pause_and_resume() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let (sender, receiver) = mpsc::channel(32);
        daemon.ipc_receiver = Some(receiver);

        // Send Pause
        sender.send(IpcToDaemon::Pause).await.expect("Failed to send");

        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");

        assert!(daemon.is_paused);
        assert!(daemon.ipc_state.is_paused.load(std::sync::atomic::Ordering::Relaxed));

        // Send Resume
        sender.send(IpcToDaemon::Resume).await.expect("Failed to send");

        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");

        assert!(!daemon.is_paused);
        assert!(!daemon.ipc_state.is_paused.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_process_ipc_commands_prune_no_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let (sender, receiver) = mpsc::channel(32);
        daemon.ipc_receiver = Some(receiver);

        // Send Prune without a database - should not panic
        sender.send(IpcToDaemon::Prune).await.expect("Failed to send");

        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");
    }

    #[tokio::test]
    async fn test_process_ipc_commands_empty_channel() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let (_sender, receiver) = mpsc::channel::<IpcToDaemon>(32);
        daemon.ipc_receiver = Some(receiver);

        // No messages in channel => should be a no-op
        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");

        assert!(!daemon.shutdown.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!daemon.is_paused);
    }

    #[tokio::test]
    async fn test_process_ipc_commands_multiple_commands() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let (sender, receiver) = mpsc::channel(32);
        daemon.ipc_receiver = Some(receiver);

        // Queue multiple commands
        sender.send(IpcToDaemon::Pause).await.expect("Failed to send");
        sender.send(IpcToDaemon::Resume).await.expect("Failed to send");

        daemon
            .process_ipc_commands()
            .await
            .expect("process_ipc_commands should succeed");

        // After Pause then Resume, should not be paused
        assert!(!daemon.is_paused);
    }

    // --- Drop impl tests ---

    #[test]
    fn test_daemon_drop_sets_shutdown() {
        let shutdown;
        {
            let config = Config::default();
            let options = DaemonOptions::default();
            let daemon = Daemon::new(config, options);
            shutdown = daemon.shutdown_handle();
            assert!(!shutdown.load(std::sync::atomic::Ordering::Relaxed));
            // daemon is dropped here
        }
        assert!(shutdown.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_daemon_drop_already_shutdown() {
        let shutdown;
        {
            let config = Config::default();
            let options = DaemonOptions::default();
            let daemon = Daemon::new(config, options);
            shutdown = daemon.shutdown_handle();
            shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            // daemon is dropped here - should not panic even if shutdown already set
        }
        assert!(shutdown.load(std::sync::atomic::Ordering::Relaxed));
    }

    // --- list_volumes() tests ---

    #[test]
    fn test_list_volumes_no_database() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let nonexistent_path = temp_dir.path().join("nonexistent.db");

        // Should not panic, prints error message
        let result = list_volumes(&nonexistent_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_volumes_empty_database() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let db_path = temp_dir.path().join("test.db");

        // Create an empty database
        let _database = Database::open(&db_path).expect("Failed to create database");

        let result = list_volumes(&db_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_volumes_with_volumes() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let db_path = temp_dir.path().join("test.db");

        let database = Database::open(&db_path).expect("Failed to create database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-LIST".to_string(),
            label: Some("TestDrive".to_string()),
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: Some(12345),
        };
        database.upsert_volume(&volume).expect("Failed to upsert volume");
        drop(database);

        // Non-detailed
        let result = list_volumes(&db_path, false);
        assert!(result.is_ok());

        // Detailed
        let result = list_volumes(&db_path, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_volumes_detailed_with_all_fields() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let db_path = temp_dir.path().join("detailed.db");

        let database = Database::open(&db_path).expect("Failed to create database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-DETAIL".to_string(),
            label: Some("DetailedVol".to_string()),
            mount_point: "D:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: false,
            last_scan_time: Some(std::time::SystemTime::now()),
            last_usn: Some(99999),
        };
        database.upsert_volume(&volume).expect("Failed to upsert volume");

        // Also add a volume without optional fields
        let volume2 = IndexedVolume {
            id: None,
            serial_number: "SN-NOLABEL".to_string(),
            label: None,
            mount_point: "E:".to_string(),
            volume_type: VolumeType::Local,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        database.upsert_volume(&volume2).expect("Failed to upsert volume2");
        drop(database);

        let result = list_volumes(&db_path, true);
        assert!(result.is_ok());
    }

    // --- show_stats() tests ---

    #[test]
    fn test_show_stats_no_database() {
        let mut config = Config::default();
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        config.daemon.database_path = Some(temp_dir.path().join("nonexistent.db"));

        let result = show_stats(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_stats_with_database() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let db_path = temp_dir.path().join("stats.db");

        let database = Database::open(&db_path).expect("Failed to create database");
        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-STATS".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        let entry = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "stats_test.txt".to_string(),
            full_path: "C:\\stats_test.txt".to_string(),
            size: 2048,
            is_directory: false,
            created_time: None,
            modified_time: None,
            mft_reference: None,
        };
        database.insert_file(&entry).expect("Failed to insert file");
        drop(database);

        let mut config = Config::default();
        config.daemon.database_path = Some(db_path);

        let result = show_stats(&config);
        assert!(result.is_ok());
    }

    // --- detect_drives() test ---

    #[test]
    fn test_detect_drives_does_not_panic() {
        // Just verify it runs without panic (output depends on system)
        detect_drives(&Config::default());
    }

    // --- DaemonState / DaemonOptions edge cases ---

    #[test]
    fn test_daemon_options_clone() {
        let options = DaemonOptions {
            foreground: true,
            rescan: true,
            usn_poll_interval_ms: 250,
            watcher_debounce_ms: 500,
        };
        let cloned = options.clone();
        assert_eq!(cloned.foreground, options.foreground);
        assert_eq!(cloned.rescan, options.rescan);
        assert_eq!(cloned.usn_poll_interval_ms, options.usn_poll_interval_ms);
        assert_eq!(cloned.watcher_debounce_ms, options.watcher_debounce_ms);
    }

    #[test]
    fn test_daemon_verbose_flag() {
        let mut config = Config::default();
        config.daemon.verbose = true;
        let options = DaemonOptions::default();
        let daemon = Daemon::new(config, options);
        assert!(daemon.verbose());

        let config2 = Config::default();
        let options2 = DaemonOptions::default();
        let daemon2 = Daemon::new(config2, options2);
        assert!(!daemon2.verbose());
    }

    #[test]
    fn test_daemon_is_running_various_states() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        assert!(!daemon.is_running());

        daemon.state = DaemonState::Starting;
        assert!(!daemon.is_running());

        daemon.state = DaemonState::Running;
        assert!(daemon.is_running());

        daemon.state = DaemonState::Stopping;
        assert!(!daemon.is_running());

        daemon.state = DaemonState::Stopped;
        assert!(!daemon.is_running());
    }

    // --- VolumeMonitor struct tests ---

    #[test]
    fn test_volume_monitor_without_usn() {
        let monitor = VolumeMonitor {
            usn_monitor: None,
            last_usn: 0,
        };
        assert!(monitor.usn_monitor.is_none());
        assert_eq!(monitor.last_usn, 0);
    }

    #[test]
    fn test_volume_monitor_last_usn_tracking() {
        let mut monitor = VolumeMonitor {
            usn_monitor: None,
            last_usn: 0,
        };
        monitor.last_usn = 12345;
        assert_eq!(monitor.last_usn, 12345);

        monitor.last_usn = 99999;
        assert_eq!(monitor.last_usn, 99999);
    }

    // --- handle_change_event rename path ---

    #[test]
    fn test_handle_change_event_rename_with_database() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-REN".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        let entry = FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: "old_name.txt".to_string(),
            full_path: "C:\\old_name.txt".to_string(),
            size: 200,
            is_directory: false,
            created_time: None,
            modified_time: None,
            mft_reference: None,
        };
        database.insert_file(&entry).expect("Failed to insert file");

        daemon.database = Some(database);

        // Rename event - the old path should be deleted from the DB.
        // The new path won't be inserted because the file doesn't exist on disk,
        // but the old entry deletion is exercised.
        daemon.handle_change_event(FileChangeEvent::Renamed {
            from: PathBuf::from("C:\\old_name.txt"),
            to: PathBuf::from("C:\\new_name.txt"),
        });

        let db = daemon.database.as_ref().expect("database should exist");
        let results = db.search_by_name("old_name.txt", 10).expect("search failed");
        assert!(results.is_empty(), "Old file should have been deleted from database");
    }

    fn daemon_with_indexed_directory() -> Daemon {
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        let database = Database::open_in_memory().expect("open database");
        let volume_id = database
            .upsert_volume(&IndexedVolume {
                id: None,
                serial_number: "SN-WATCHED".to_string(),
                label: None,
                mount_point: "C:".to_string(),
                volume_type: VolumeType::Network,
                is_online: true,
                last_scan_time: None,
                last_usn: None,
            })
            .expect("insert volume");
        for (full_path, is_directory) in [
            (r"C:\old_dir", true),
            (r"C:\old_dir\a.txt", false),
            (r"C:\old_dir\sub", true),
            (r"C:\old_dir\sub\b.txt", false),
            (r"C:\other\c.txt", false),
        ] {
            let name = full_path.rsplit('\\').next().expect("file name");
            database
                .insert_file(&FileEntry {
                    id: None,
                    volume_id,
                    parent_id: None,
                    name: name.to_string(),
                    full_path: full_path.to_string(),
                    is_directory,
                    size: 1,
                    created_time: None,
                    modified_time: None,
                    mft_reference: None,
                })
                .expect("insert entry");
        }
        daemon.database = Some(database);
        daemon
    }

    fn indexed_paths(daemon: &Daemon) -> Vec<String> {
        let database = daemon.database.as_ref().expect("database");
        let mut paths: Vec<String> = database
            .search_by_path("", 100)
            .expect("list entries")
            .into_iter()
            .map(|entry| entry.full_path)
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn test_watcher_directory_rename_moves_its_descendants() {
        let daemon = daemon_with_indexed_directory();

        assert!(daemon.handle_change_event_with_reference(
            FileChangeEvent::Renamed {
                from: PathBuf::from(r"C:\old_dir"),
                to: PathBuf::from(r"C:\new_dir"),
            },
            None
        ));

        assert_eq!(
            indexed_paths(&daemon),
            vec![
                r"C:\new_dir",
                r"C:\new_dir\a.txt",
                r"C:\new_dir\sub",
                r"C:\new_dir\sub\b.txt",
                r"C:\other\c.txt",
            ]
        );
        let renamed = daemon
            .database
            .as_ref()
            .expect("database")
            .get_file_by_path(r"C:\new_dir")
            .expect("look up renamed directory")
            .expect("renamed directory is indexed");
        assert_eq!(renamed.name, "new_dir");
        assert!(renamed.is_directory);
    }

    #[test]
    fn test_watcher_directory_delete_removes_its_descendants() {
        let daemon = daemon_with_indexed_directory();

        assert!(
            daemon.handle_change_event_with_reference(FileChangeEvent::Deleted(PathBuf::from(r"C:\old_dir")), None)
        );

        assert_eq!(indexed_paths(&daemon), vec![r"C:\other\c.txt"]);
    }

    #[test]
    fn test_watcher_file_rename_moves_the_entry_when_the_file_exists() {
        let temp_dir = TempDir::new().expect("create temp directory");
        let old_path = temp_dir.path().join("old.txt");
        let new_path = temp_dir.path().join("new.txt");
        std::fs::write(&new_path, "renamed contents").expect("write renamed file");
        let old_path_string = old_path.to_string_lossy().to_string();
        let mount_point = if cfg!(windows) {
            old_path_string.chars().take(2).collect::<String>()
        } else {
            "/".to_string()
        };
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        let database = Database::open_in_memory().expect("open database");
        let volume_id = database
            .upsert_volume(&IndexedVolume {
                id: None,
                serial_number: "SN-FILE-RENAME".to_string(),
                label: None,
                mount_point,
                volume_type: VolumeType::Network,
                is_online: true,
                last_scan_time: None,
                last_usn: None,
            })
            .expect("insert volume");
        database
            .insert_file(&FileEntry {
                id: None,
                volume_id,
                parent_id: None,
                name: "old.txt".to_string(),
                full_path: old_path_string.clone(),
                is_directory: false,
                size: 3,
                created_time: None,
                modified_time: None,
                mft_reference: None,
            })
            .expect("insert entry");
        daemon.database = Some(database);
        daemon.build_path_cache().expect("build path cache");

        assert!(daemon.handle_change_event_with_reference(
            FileChangeEvent::Renamed {
                from: old_path,
                to: new_path.clone(),
            },
            None
        ));

        let database = daemon.database.as_ref().expect("database");
        assert!(
            database
                .get_file_by_path(&old_path_string)
                .expect("look up old")
                .is_none()
        );
        let renamed = database
            .get_file_by_path(&new_path.to_string_lossy())
            .expect("look up new")
            .expect("renamed file is indexed");
        assert_eq!(renamed.name, "new.txt");
        assert_eq!(renamed.size, "renamed contents".len() as u64);
    }

    // --- Integration-style: build_path_cache then resolve ---

    #[test]
    fn test_build_path_cache_then_resolve() {
        let config = Config::default();
        let options = DaemonOptions::default();
        let mut daemon = Daemon::new(config, options);

        let database = Database::open_in_memory().expect("Failed to create in-memory database");

        let volume = IndexedVolume {
            id: None,
            serial_number: "SN-INT".to_string(),
            label: None,
            mount_point: "C:".to_string(),
            volume_type: VolumeType::Ntfs,
            is_online: true,
            last_scan_time: None,
            last_usn: None,
        };
        let volume_id = database.upsert_volume(&volume).expect("Failed to upsert volume");

        // Insert directories with MFT references
        let entries = vec![
            FileEntry {
                id: None,
                volume_id,
                parent_id: None,
                name: "Windows".to_string(),
                full_path: "C:\\Windows".to_string(),
                size: 0,
                is_directory: true,
                created_time: None,
                modified_time: None,
                mft_reference: Some(50),
            },
            FileEntry {
                id: None,
                volume_id,
                parent_id: None,
                name: "System32".to_string(),
                full_path: "C:\\Windows\\System32".to_string(),
                size: 0,
                is_directory: true,
                created_time: None,
                modified_time: None,
                mft_reference: Some(60),
            },
        ];
        for entry in &entries {
            database.insert_file(entry).expect("Failed to insert directory");
        }

        daemon.database = Some(database);
        daemon.build_path_cache().expect("build_path_cache should succeed");

        // Now resolve paths using the cache
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 50, "notepad.exe"),
            Some("C:\\Windows\\notepad.exe".to_string())
        );
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 60, "cmd.exe"),
            Some("C:\\Windows\\System32\\cmd.exe".to_string())
        );
        // Unknown parent should return None
        assert!(daemon.resolve_usn_path_inner('C', 999, "missing.dll").is_none());
        // Root parent should still work
        assert_eq!(
            daemon.resolve_usn_path_inner('C', 5, "pagefile.sys"),
            Some("C:\\pagefile.sys".to_string())
        );
    }
}

#[cfg(test)]
mod test_multi_root_ownership {
    use filefind_common::types::{IndexedVolume, VolumeType};

    use super::*;

    #[test]
    fn multiple_unix_roots_and_unc_shares_keep_distinct_volume_ids() {
        let mut daemon = Daemon::new(Config::default(), DaemonOptions::default());
        let database = Database::open_in_memory().unwrap();
        let unix_a = database
            .upsert_volume(&IndexedVolume::new(
                "UNIX-A".into(),
                "/data/a".into(),
                VolumeType::Local,
            ))
            .unwrap();
        let unix_b = database
            .upsert_volume(&IndexedVolume::new(
                "UNIX-B".into(),
                "/data/b".into(),
                VolumeType::Local,
            ))
            .unwrap();
        let unc_a = database
            .upsert_volume(&IndexedVolume::new(
                "UNC-A".into(),
                r"\\nas\media".into(),
                VolumeType::Network,
            ))
            .unwrap();
        let unc_b = database
            .upsert_volume(&IndexedVolume::new(
                "UNC-B".into(),
                r"\\nas\backup".into(),
                VolumeType::Network,
            ))
            .unwrap();

        daemon.database = Some(database);
        daemon.build_path_cache().unwrap();

        assert_eq!(
            daemon.get_volume_id_for_path(Path::new("/data/a/file.txt")),
            Some(unix_a)
        );
        assert_eq!(
            daemon.get_volume_id_for_path(Path::new("/data/b/file.txt")),
            Some(unix_b)
        );
        assert_eq!(daemon.get_volume_id_for_path(Path::new("/data/a-extra/file.txt")), None);
        assert_eq!(
            daemon.get_volume_id_for_path(Path::new(r"\\nas\media\film.mkv")),
            Some(unc_a)
        );
        assert_eq!(
            daemon.get_volume_id_for_path(Path::new(r"\\nas\backup\dump.tar")),
            Some(unc_b)
        );
    }

    #[test]
    fn should_index_path_applies_exclude_patterns_and_hidden_rules() {
        let mut config = Config::default();
        config.daemon.paths = vec!["/data/work".into()];
        config.daemon.exclude = vec!["target".into()];
        config.daemon.exclude_patterns = vec!["*.tmp".into(), "~$*".into()];
        let daemon = Daemon::new(config, DaemonOptions::default());

        assert!(daemon.should_index_path("/data/work/src/main.rs"));
        assert!(!daemon.should_index_path("/data/work/draft.tmp"));
        assert!(!daemon.should_index_path("/data/work/~$sheet.xlsx"));
        assert!(!daemon.should_index_path("/data/work/.hidden"));
        assert!(!daemon.should_index_path("/data/work/target/debug"));
        assert!(!daemon.should_index_path("/data/work-extra/main.rs"));
    }
}

#[cfg(test)]
mod test_watcher_event_reconciliation {
    use std::fs;

    use filefind_common::types::{IndexedVolume, VolumeType};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn modified_event_refreshes_size_and_deletes_vanished_file() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let file_path = root.join("notes.txt");
        fs::write(&file_path, "hi").unwrap();

        let mut config = Config::default();
        config.daemon.paths = vec![root.to_string_lossy().into_owned()];
        let mut daemon = Daemon::new(config, DaemonOptions::default());
        let database = Database::open_in_memory().unwrap();
        let volume_id = database
            .upsert_volume(&IndexedVolume::new(
                "MOD-VOL".into(),
                root.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();
        database
            .insert_file(&FileEntry {
                id: None,
                volume_id,
                parent_id: None,
                name: "notes.txt".into(),
                full_path: file_path.to_string_lossy().into_owned(),
                is_directory: false,
                size: 2,
                created_time: None,
                modified_time: None,
                mft_reference: None,
            })
            .unwrap();
        daemon.database = Some(database);
        daemon.build_path_cache().unwrap();

        fs::write(&file_path, "much longer updated contents").unwrap();
        assert!(daemon.handle_change_event_with_reference(FileChangeEvent::Modified(file_path.clone()), None));

        let updated = daemon
            .database
            .as_ref()
            .unwrap()
            .get_file_by_path(&file_path.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(updated.size, 28);

        fs::remove_file(&file_path).unwrap();
        assert!(daemon.handle_change_event_with_reference(FileChangeEvent::Modified(file_path.clone()), None));
        assert!(
            daemon
                .database
                .as_ref()
                .unwrap()
                .get_file_by_path(&file_path.to_string_lossy())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn created_directory_event_indexes_nested_descendants_and_respects_exclusions() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let incoming = root.join("incoming");
        fs::create_dir_all(incoming.join("sub")).unwrap();
        fs::write(incoming.join("a.txt"), "alpha").unwrap();
        fs::write(incoming.join("sub/b.txt"), "beta").unwrap();
        fs::write(incoming.join("ignored.tmp"), "skip").unwrap();
        fs::write(incoming.join(".hidden"), "skip").unwrap();

        let mut config = Config::default();
        config.daemon.paths = vec![root.to_string_lossy().into_owned()];
        config.daemon.exclude_patterns = vec!["*.tmp".into()];
        let mut daemon = Daemon::new(config, DaemonOptions::default());
        let database = Database::open_in_memory().unwrap();
        database
            .upsert_volume(&IndexedVolume::new(
                "INCOMING-VOL".into(),
                root.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();
        daemon.database = Some(database);
        daemon.build_path_cache().unwrap();

        assert!(daemon.handle_change_event_with_reference(FileChangeEvent::Created(incoming.clone()), None));

        let db = daemon.database.as_ref().unwrap();
        assert!(db.get_file_by_path(&incoming.to_string_lossy()).unwrap().is_some());
        assert!(
            db.get_file_by_path(&incoming.join("a.txt").to_string_lossy())
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_file_by_path(&incoming.join("sub").to_string_lossy())
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_file_by_path(&incoming.join("sub/b.txt").to_string_lossy())
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_file_by_path(&incoming.join("ignored.tmp").to_string_lossy())
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_file_by_path(&incoming.join(".hidden").to_string_lossy())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn failed_watcher_event_requests_reconciliation_and_success_refreshes_ipc_counts() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let good_file = root.join("good.txt");
        fs::write(&good_file, "ok").unwrap();

        let mut config = Config::default();
        config.daemon.paths = vec![root.to_string_lossy().into_owned()];
        let mut daemon = Daemon::new(config, DaemonOptions::default());
        let database = Database::open_in_memory().unwrap();
        database
            .upsert_volume(&IndexedVolume::new(
                "IPC-COUNT-VOL".into(),
                root.to_string_lossy().into_owned(),
                VolumeType::Local,
            ))
            .unwrap();
        daemon.database = Some(database);
        daemon.build_path_cache().unwrap();

        let (sender, receiver) = mpsc::channel(8);
        sender.try_send(FileChangeEvent::Created(good_file)).unwrap();
        daemon.watcher_receiver = Some(receiver);

        assert!(!daemon.process_watcher_events());
        assert_eq!(daemon.ipc_state.get_status().indexed_files, 1);

        // Sending a created event for an unindexed volume fails application and requests reconciliation.
        let mut orphan_daemon = Daemon::new(Config::default(), DaemonOptions::default());
        orphan_daemon.database = Some(Database::open_in_memory().unwrap());
        let (orphan_tx, orphan_rx) = mpsc::channel(8);
        orphan_tx
            .try_send(FileChangeEvent::Created(PathBuf::from("/unindexed/file.txt")))
            .unwrap();
        orphan_daemon.watcher_receiver = Some(orphan_rx);
        assert!(orphan_daemon.process_watcher_events());
    }
}
