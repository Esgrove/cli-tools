//! File system watcher for non-NTFS drives.
//!
//! This module provides file system monitoring for drives that don't support MFT/USN Journal access
//! (network drives, FAT32, exFAT, etc.) using the `notify` crate which wraps platform-native file watching APIs.
//!
//! On Windows, this uses `ReadDirectoryChangesW`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use filefind_common::IndexScope;
#[cfg(test)]
use filefind_common::scope::matches_exclude_rules;
use filefind_common::scope::path_to_utf8;
use filefind_common::types::FileChangeEvent;
use filefind_common::{extract_drive_letter, get_persistent_drive_mapping, is_unc_path, normalize_drive_root};

mod pending_events;

use pending_events::PendingEvents;

/// Default debounce duration to coalesce rapid file changes.
const DEFAULT_DEBOUNCE_MS: u64 = 1000;

/// Default number of concurrent directory scan tasks.
/// Optimized for HDDs (NCQ depth) and network shares (latency hiding).
const DEFAULT_SCAN_CONCURRENCY: usize = 32;

/// File system watcher for monitoring directories.
pub struct FileWatcher {
    /// The paths being watched.
    watched_paths: Vec<PathBuf>,

    /// Patterns to exclude from watching.
    exclude_patterns: Vec<String>,

    /// Filename glob patterns to exclude from watching.
    exclude_globs: Vec<String>,

    /// Debounce duration in milliseconds.
    debounce_ms: u64,

    /// Whether to watch recursively.
    recursive: bool,

    /// Shutdown flag.
    shutdown: Arc<AtomicBool>,

    /// A watcher error means changes may have been missed.
    rescan_needed: Arc<AtomicBool>,

    /// Number of configured paths successfully registered with the OS watcher.
    registered_watches: Arc<AtomicUsize>,
}

/// Entry from a directory scan.
#[derive(Debug, Clone)]
pub struct ScanEntry {
    /// Full path to the file or directory.
    pub path: PathBuf,

    /// File or directory name.
    pub name: String,

    /// Whether this is a directory.
    pub is_directory: bool,

    /// File size in bytes (0 for directories).
    pub size: u64,

    /// Last modified time.
    pub modified: Option<std::time::SystemTime>,

    /// Creation time.
    pub created: Option<std::time::SystemTime>,
}

impl FileWatcher {
    /// Create a new file watcher.
    #[cfg(test)]
    #[must_use]
    pub fn new(paths: Vec<PathBuf>, exclude_patterns: Vec<String>, debounce_ms: u64, recursive: bool) -> Self {
        Self::with_scope(
            paths,
            IndexScope::new(Vec::new(), exclude_patterns, Vec::new()),
            debounce_ms,
            recursive,
        )
    }

    /// Create a new file watcher with the full shared [`IndexScope`].
    #[must_use]
    pub fn with_scope(paths: Vec<PathBuf>, scope: IndexScope, debounce_ms: u64, recursive: bool) -> Self {
        Self {
            watched_paths: paths.into_iter().map(|path| normalize_drive_root(&path)).collect(),
            exclude_patterns: scope.exclude,
            exclude_globs: scope.exclude_patterns,
            debounce_ms,
            recursive,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Create a new file watcher for a single path.
    #[cfg(test)]
    #[must_use]
    pub fn for_path(path: PathBuf) -> Self {
        Self {
            watched_paths: vec![path],
            exclude_patterns: Vec::new(),
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Add a path to watch.
    #[cfg(test)]
    pub fn add_path(&mut self, path: PathBuf) {
        if !self.watched_paths.contains(&path) {
            self.watched_paths.push(path);
        }
    }

    /// Add an exclusion pattern.
    #[cfg(test)]
    pub fn add_exclude_pattern(&mut self, pattern: String) {
        if !self.exclude_patterns.contains(&pattern) {
            self.exclude_patterns.push(pattern);
        }
    }

    /// Check if a path should be excluded based on the exclusion patterns.
    #[cfg(test)]
    fn should_exclude(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        matches_exclude_rules(&path_str, &self.exclude_patterns)
            || filefind_common::scope::matches_exclude_globs(&path_str, &self.exclude_globs)
    }

    /// Start watching for file changes with debouncing.
    ///
    /// Returns a channel receiver for change events and a shutdown handle.
    ///
    /// Events for the same path within `debounce_ms` milliseconds are coalesced into one change.
    /// The old and new names of a rename are paired into one rename change.
    pub fn start(&self) -> Result<(mpsc::Receiver<FileChangeEvent>, Arc<AtomicBool>)> {
        let (event_tx, event_rx) = mpsc::channel(1000);
        let shutdown = self.shutdown.clone();
        let watched_paths = self.watched_paths.clone();
        let watched_roots: Vec<String> = watched_paths
            .iter()
            .filter_map(|path| path.to_str().map(ToString::to_string))
            .collect();
        let scope = IndexScope::new(watched_roots, self.exclude_patterns.clone(), self.exclude_globs.clone());
        let debounce_ms = self.debounce_ms;
        let recursive = self.recursive;

        // Create a channel for notify events
        let (notify_tx, mut notify_rx) = mpsc::channel(1000);

        // Create the watcher
        let watcher_shutdown = shutdown.clone();
        let rescan_needed = Arc::clone(&self.rescan_needed);
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if watcher_shutdown.load(Ordering::Relaxed) {
                return;
            }

            match res {
                Ok(event) => {
                    // Use blocking send since we're in a sync callback
                    if notify_tx.blocking_send(event).is_err() {
                        rescan_needed.store(true, Ordering::Release);
                    }
                }
                Err(error) => {
                    error!("File watcher error: {}", error);
                    rescan_needed.store(true, Ordering::Release);
                }
            }
        })
        .context("Failed to create file watcher")?;

        // Add paths to watch and record how many actually registered.
        let registered = Self::watch_configured_paths(&mut watcher, &watched_paths, recursive);
        self.registered_watches.store(registered, Ordering::Release);

        // Spawn async task to process events with debouncing
        let process_shutdown = shutdown.clone();
        let debounce_duration = Duration::from_millis(debounce_ms);
        tokio::spawn(async move {
            // Keep watcher alive
            let _watcher = watcher;

            info!("File watcher started (debounce: {}ms)", debounce_ms);

            let mut pending_events = PendingEvents::new();
            let mut last_flush = std::time::Instant::now();

            while !process_shutdown.load(Ordering::Relaxed) {
                let poll_interval = Duration::from_millis((debounce_ms / 2).clamp(50, 1000));
                match tokio::time::timeout(poll_interval, notify_rx.recv()).await {
                    Ok(Some(event)) => {
                        pending_events.ingest_with_scope(event, &scope, std::time::Instant::now());
                    }
                    Ok(None) => break,
                    Err(_) => { /* Timeout – check for events to flush */ }
                }

                if Self::flush_pending_events(&mut pending_events, &mut last_flush, debounce_duration, &event_tx).await
                    == Err(())
                {
                    break;
                }
            }

            // Flush any remaining pending events on shutdown
            for change in pending_events.take_all() {
                let _ = event_tx.send(change).await;
            }

            info!("File watcher stopped");
        });

        Ok((event_rx, shutdown))
    }

    /// Register each configured path with the underlying `notify` watcher and return how many succeeded.
    fn watch_configured_paths(watcher: &mut RecommendedWatcher, paths: &[PathBuf], recursive: bool) -> usize {
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };

        let mut registered = 0usize;
        for path in paths {
            if path.exists() {
                match watcher.watch(path, mode) {
                    Ok(()) => {
                        info!("Watching path: {}", path.display());
                        registered += 1;
                    }
                    Err(error) => warn!("Failed to watch path {}: {}", path.display(), error),
                }
            } else {
                Self::log_inaccessible_path(path);
            }
        }
        registered
    }

    /// Log a descriptive warning for a path that could not be watched.
    ///
    /// Distinguishes between UNC network paths, mapped network drives (including offline persistent mappings),
    /// and regular local paths so the user gets an actionable message instead of a generic "does not exist".
    fn log_inaccessible_path(path: &Path) {
        if is_unc_path(path) {
            warn!(
                "Network path is not reachable (host may be offline), skipping: {}",
                path.display()
            );
            return;
        }

        // Check for a drive-letter path that is a mapped or persistent network drive
        if let Some(drive_letter) = extract_drive_letter(path)
            && let Some(remote_path) = get_persistent_drive_mapping(drive_letter)
        {
            warn!(
                "Mapped network drive is offline ({} -> {}), skipping: {}",
                drive_letter,
                remote_path,
                path.display()
            );
            return;
        }

        warn!("Path does not exist, skipping: {}", path.display());
    }

    /// Send debounced events that have aged past `debounce_duration`.
    ///
    /// Returns `Err(())` when the receiver has been dropped and the caller should stop the event loop.
    async fn flush_pending_events(
        pending_events: &mut PendingEvents,
        last_flush: &mut std::time::Instant,
        debounce_duration: Duration,
        event_tx: &mpsc::Sender<FileChangeEvent>,
    ) -> std::result::Result<(), ()> {
        let now = std::time::Instant::now();
        if now.duration_since(*last_flush) < debounce_duration {
            return Ok(());
        }

        for change in pending_events.take_due(now, debounce_duration) {
            debug!("File change detected (debounced): {}", change);
            if event_tx.send(change).await.is_err() {
                return Err(());
            }
        }

        *last_flush = now;
        Ok(())
    }

    /// Stop the file watcher.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Return whether the watcher reported potentially missed changes since the last check.
    pub fn take_rescan_request(&self) -> bool {
        self.rescan_needed.swap(false, Ordering::AcqRel)
    }

    /// Return the number of configured paths successfully registered with the OS watcher.
    #[must_use]
    pub fn registered_watch_count(&self) -> usize {
        self.registered_watches.load(Ordering::Acquire)
    }

    /// Get the paths being watched.
    #[must_use]
    pub fn watched_paths(&self) -> &[PathBuf] {
        &self.watched_paths
    }

    /// Get the exclusion patterns.
    #[cfg(test)]
    #[must_use]
    pub fn exclude_patterns(&self) -> &[String] {
        &self.exclude_patterns
    }
}

impl Default for FileWatcher {
    fn default() -> Self {
        Self {
            watched_paths: Vec::new(),
            exclude_patterns: Vec::new(),
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ScanEntry {
    /// Convert to a `FileEntry` for database storage, consuming self.
    #[must_use]
    pub fn into_file_entry(self, volume_id: i64) -> filefind_common::types::FileEntry {
        filefind_common::types::FileEntry {
            id: None,
            volume_id,
            parent_id: None,
            name: self.name,
            full_path: self
                .path
                .into_os_string()
                .into_string()
                .unwrap_or_else(|os_string| os_string.to_string_lossy().into_owned()),
            is_directory: self.is_directory,
            size: self.size,
            created_time: self.created,
            modified_time: self.modified,
            mft_reference: None,
        }
    }
}

/// Perform an initial directory scan to populate the index.
///
/// This walks the directory tree and returns all files and directories found.
/// Uses parallel scanning with tokio for better performance.
#[cfg(test)]
pub async fn scan_directory(root: &Path, exclude_patterns: &[String]) -> Result<Vec<ScanEntry>> {
    let scope = IndexScope::new(
        vec![root.to_string_lossy().into_owned()],
        exclude_patterns.to_vec(),
        Vec::new(),
    );
    scan_directory_with_scope_and_concurrency(root, &scope, DEFAULT_SCAN_CONCURRENCY).await
}

/// Perform an initial directory scan using the shared [`IndexScope`].
pub async fn scan_directory_with_scope(root: &Path, scope: &IndexScope) -> Result<Vec<ScanEntry>> {
    scan_directory_with_scope_and_concurrency(root, scope, DEFAULT_SCAN_CONCURRENCY).await
}

/// Perform an initial directory scan with the shared [`IndexScope`] and configurable concurrency.
async fn scan_directory_with_scope_and_concurrency(
    root: &Path,
    scope: &IndexScope,
    max_concurrency: usize,
) -> Result<Vec<ScanEntry>> {
    debug!("{} Starting file scan", root.display());

    let root = normalize_drive_root(root);
    let root_str: Arc<str> = Arc::from(path_to_utf8(&root)?);
    let _root_directory = tokio::fs::read_dir(&root)
        .await
        .with_context(|| format!("Cannot scan directory root {}", root.display()))?;
    let scope = Arc::new(scope.clone());
    let entries: Arc<tokio::sync::Mutex<Vec<ScanEntry>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrency));

    // Add the root directory itself
    if let Ok(metadata) = tokio::fs::metadata(&root).await {
        let root_entry = ScanEntry {
            path: root.clone(),
            name: root
                .file_name()
                .map_or_else(|| root_str.to_string(), |name| name.to_string_lossy().into_owned()),
            is_directory: metadata.is_dir(),
            size: 0,
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        };
        entries.lock().await.push(root_entry);
    }

    // Process directories with bounded concurrency using a semaphore
    let mut dirs_to_process = vec![root.clone()];
    let mut failed_directories = 0usize;

    while !dirs_to_process.is_empty() {
        // Process current batch with concurrency limit
        let batch: Vec<_> = std::mem::take(&mut dirs_to_process);
        let mut tasks = Vec::with_capacity(batch.len());

        for dir in batch {
            let entries = entries.clone();
            let scope = scope.clone();
            let root_str = root_str.clone();
            let semaphore = semaphore.clone();

            tasks.push(tokio::spawn(async move {
                // Acquire permit before scanning (limits concurrent I/O)
                let permit = semaphore.acquire().await.context("Directory scan semaphore closed")?;
                let result = scan_single_directory(dir, root_str, scope, entries).await;
                drop(permit);
                result
            }));
        }

        // Collect results (subdirectories to process next)
        for task in tasks {
            match task.await {
                Ok(Ok(subdirectories)) => dirs_to_process.extend(subdirectories),
                Ok(Err(error)) => {
                    warn!("Directory scan failed: {error:#}");
                    failed_directories += 1;
                }
                Err(error) => {
                    warn!("Directory scan task failed: {error}");
                    failed_directories += 1;
                }
            }
        }
    }

    if failed_directories > 0 {
        warn!(
            "Skipped {} unreadable subdirectories under {}; remaining readable entries were indexed",
            failed_directories,
            root.display()
        );
    }

    let mut result = match Arc::try_unwrap(entries) {
        Ok(entries) => entries.into_inner(),
        Err(entries) => entries.lock().await.clone(),
    };

    // Sort by path for consistent ordering
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

/// Check if a path matches any of the exclusion patterns.
#[cfg(test)]
pub fn matches_exclude_patterns(path_str: &str, exclude_patterns: &[String]) -> bool {
    matches_exclude_rules(path_str, exclude_patterns)
}

/// Scan a single directory and return its subdirectories.
async fn scan_single_directory(
    dir: PathBuf,
    root_str: Arc<str>,
    scope: Arc<IndexScope>,
    entries: Arc<tokio::sync::Mutex<Vec<ScanEntry>>>,
) -> Result<Vec<PathBuf>> {
    let mut read_dir = tokio::fs::read_dir(&dir)
        .await
        .with_context(|| format!("Error reading directory {}", dir.display()))?;

    let mut subdirs = Vec::new();
    let mut local_entries = Vec::new();

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .with_context(|| format!("Error listing {}", dir.display()))?
    {
        let path = entry.path();
        let Ok(path_str) = path_to_utf8(&path) else {
            warn!("Skipping non-UTF-8 path during scan: {}", path.display());
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
            warn!("Skipping non-UTF-8 entry name during scan: {}", path.display());
            continue;
        };

        if scope.is_excluded(path_str, Some(&root_str)) {
            continue;
        }

        let metadata = entry
            .metadata()
            .await
            .with_context(|| format!("Error reading metadata for {}", path.display()))?;

        let is_directory = metadata.is_dir();

        let scan_entry = ScanEntry {
            path: path.clone(),
            name,
            is_directory,
            size: if metadata.is_file() { metadata.len() } else { 0 },
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        };

        local_entries.push(scan_entry);

        if is_directory {
            subdirs.push(path);
        }
    }

    // Add local entries to the shared collection
    {
        let mut guard = entries.lock().await;
        guard.extend(local_entries);
    }

    Ok(subdirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[test]
    fn test_rename_event_becomes_a_rename_change() {
        let event = Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )))
        .add_path(PathBuf::from("old.txt"))
        .add_path(PathBuf::from("new.txt"));
        let mut pending_events = PendingEvents::new();

        pending_events.ingest(event, &[], std::time::Instant::now());

        let changes = pending_events.take_all();
        let described: Vec<String> = changes.iter().map(ToString::to_string).collect();
        assert_eq!(changes.len(), 1, "unexpected changes: {described:?}");
        assert!(
            matches!(
                changes.first(),
                Some(FileChangeEvent::Renamed { from, to }) if from == Path::new("old.txt") && to == Path::new("new.txt")
            ),
            "unexpected changes: {described:?}"
        );
    }

    #[test]
    fn test_watcher_rescan_request_is_consumed() {
        let watcher = FileWatcher::for_path(PathBuf::from("missing"));
        watcher.rescan_needed.store(true, Ordering::Release);
        assert!(watcher.take_rescan_request());
        assert!(!watcher.take_rescan_request());
    }

    #[tokio::test]
    async fn test_scan_single_directory_reports_missing_directory() {
        let temp = tempdir().expect("create temp directory");
        let missing = temp.path().join("missing");
        let entries = Arc::new(tokio::sync::Mutex::new(Vec::new()));

        assert!(
            scan_single_directory(missing, Arc::from(""), Arc::new(IndexScope::default()), entries)
                .await
                .is_err()
        );
    }

    #[test]
    fn test_should_exclude() {
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec![
                "*.tmp".to_string(),
                "*\\node_modules\\*".to_string(),
                "Thumbs.db".to_string(),
            ],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        assert!(watcher.should_exclude(Path::new("test.tmp")));
        assert!(watcher.should_exclude(Path::new("C:\\project\\node_modules\\pkg")));
        assert!(watcher.should_exclude(Path::new("C:\\photos\\Thumbs.db")));
        assert!(!watcher.should_exclude(Path::new("document.pdf")));
    }

    #[test]
    fn test_should_exclude_suffix_pattern() {
        // Pattern "*pattern" matches paths ending with "pattern"
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec!["*.bak".to_string(), "*.tmp".to_string()],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        // Should match - ends with .bak
        assert!(watcher.should_exclude(Path::new("file.bak")));
        assert!(watcher.should_exclude(Path::new("C:\\folder\\data.bak")));
        assert!(watcher.should_exclude(Path::new("test.tmp")));

        // Should not match - different extension
        assert!(!watcher.should_exclude(Path::new("file.txt")));
        assert!(!watcher.should_exclude(Path::new("file.bak.txt")));
    }

    #[test]
    fn test_should_exclude_contains_pattern() {
        // Pattern "*pattern*" matches paths containing "pattern"
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec!["*cache*".to_string(), "*node_modules*".to_string()],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        // Should match - contains "cache"
        assert!(watcher.should_exclude(Path::new("C:\\mycache\\file.txt")));
        assert!(watcher.should_exclude(Path::new("cache_dir")));
        assert!(watcher.should_exclude(Path::new("filecache")));

        // Should match - contains "node_modules"
        assert!(watcher.should_exclude(Path::new("C:\\project\\node_modules\\pkg")));

        // Should not match
        assert!(!watcher.should_exclude(Path::new("C:\\important\\file.txt")));
    }

    #[test]
    fn test_should_exclude_whole_component_pattern() {
        // Pattern without wildcards matches whole path components only
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec!["Thumbs.db".to_string(), ".git".to_string()],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        assert!(watcher.should_exclude(Path::new("Thumbs.db")));
        assert!(watcher.should_exclude(Path::new("C:\\photos\\Thumbs.db")));
        assert!(watcher.should_exclude(Path::new("C:\\project\\.git\\config")));
        assert!(watcher.should_exclude(Path::new("thumbs.db")));

        assert!(!watcher.should_exclude(Path::new("file.txt")));
        assert!(!watcher.should_exclude(Path::new("C:\\project\\.gitignore")));
        assert!(!watcher.should_exclude(Path::new("C:\\photos\\OldThumbs.db")));
    }

    #[test]
    fn test_should_exclude_empty_patterns() {
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec![],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        // With no patterns, nothing should be excluded
        assert!(!watcher.should_exclude(Path::new("any_file.txt")));
        assert!(!watcher.should_exclude(Path::new("C:\\Windows\\System32")));
        assert!(!watcher.should_exclude(Path::new(".hidden")));
    }

    #[test]
    fn test_should_exclude_multiple_patterns_first_match_wins() {
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec!["*.log".to_string(), "*temp*".to_string(), "secret.txt".to_string()],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        // Each pattern can independently match
        assert!(watcher.should_exclude(Path::new("app.log")));
        assert!(watcher.should_exclude(Path::new("C:\\temp\\file.txt")));
        assert!(watcher.should_exclude(Path::new("secret.txt")));

        // None match
        assert!(!watcher.should_exclude(Path::new("important.txt")));
    }

    #[test]
    fn test_scan_entry_to_file_entry() {
        let entry = ScanEntry {
            path: PathBuf::from("C:\\test\\document.pdf"),
            name: "document.pdf".to_string(),
            is_directory: false,
            size: 1024,
            modified: None,
            created: None,
        };

        let file_entry = entry.into_file_entry(1);
        assert_eq!(file_entry.name, "document.pdf");
        assert_eq!(file_entry.volume_id, 1);
        assert!(!file_entry.is_directory);
        assert_eq!(file_entry.size, 1024);
    }

    #[test]
    fn test_scan_entry_to_file_entry_with_timestamps() {
        let now = SystemTime::now();
        let created = now;
        let modified = now;

        let entry = ScanEntry {
            path: PathBuf::from("C:\\folder\\file.txt"),
            name: "file.txt".to_string(),
            is_directory: false,
            size: 500,
            modified: Some(modified),
            created: Some(created),
        };

        let file_entry = entry.into_file_entry(42);
        assert_eq!(file_entry.volume_id, 42);
        assert!(file_entry.created_time.is_some());
        assert!(file_entry.modified_time.is_some());
        assert_eq!(file_entry.size, 500);
    }

    #[test]
    fn test_scan_entry_to_file_entry_directory() {
        let entry = ScanEntry {
            path: PathBuf::from("C:\\folder"),
            name: "folder".to_string(),
            is_directory: true,
            size: 0,
            modified: None,
            created: None,
        };

        let file_entry = entry.into_file_entry(1);
        assert!(file_entry.is_directory);
        assert_eq!(file_entry.size, 0);
        assert_eq!(file_entry.full_path, "C:\\folder");
    }

    #[test]
    fn test_scan_entry_clone() {
        let entry = ScanEntry {
            path: PathBuf::from("C:\\test.txt"),
            name: "test.txt".to_string(),
            is_directory: false,
            size: 100,
            modified: Some(SystemTime::now()),
            created: None,
        };

        let cloned = entry.clone();
        assert_eq!(entry.name, cloned.name);
        assert_eq!(entry.path, cloned.path);
        assert_eq!(entry.size, cloned.size);
        assert_eq!(entry.is_directory, cloned.is_directory);
    }

    #[test]
    fn test_scan_entry_debug() {
        let entry = ScanEntry {
            path: PathBuf::from("C:\\debug.txt"),
            name: "debug.txt".to_string(),
            is_directory: false,
            size: 0,
            modified: None,
            created: None,
        };

        let debug_str = format!("{entry:?}");
        assert!(debug_str.contains("ScanEntry"));
        assert!(debug_str.contains("debug.txt"));
    }

    #[tokio::test]
    async fn test_scan_directory() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        // Create some test files
        fs::create_dir_all(root.join("subdir")).unwrap();
        fs::write(root.join("file1.txt"), "content").unwrap();
        fs::write(root.join("subdir").join("file2.txt"), "content").unwrap();

        let entries = scan_directory(root, &[]).await.unwrap();

        // Debug output
        eprintln!("Scanned {} entries from {}", entries.len(), root.display());
        for entry in &entries {
            eprintln!("  - {} (dir={})", entry.name, entry.is_directory);
        }

        // Check that we found the expected files
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"file1.txt"), "Should find file1.txt, got: {names:?}");
        assert!(names.contains(&"file2.txt"), "Should find file2.txt, got: {names:?}");
        assert!(names.contains(&"subdir"), "Should find subdir, got: {names:?}");

        // Verify file entry properties
        let file1 = entries.iter().find(|entry| entry.name == "file1.txt").unwrap();
        assert!(!file1.is_directory);
        assert_eq!(file1.size, 7); // "content" is 7 bytes

        let subdir = entries.iter().find(|entry| entry.name == "subdir").unwrap();
        assert!(subdir.is_directory);
    }

    #[tokio::test]
    async fn test_scan_directory_empty() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        // Empty directory - but the root itself is included
        let entries = scan_directory(root, &[]).await.unwrap();

        // Should include the root directory itself
        assert!(!entries.is_empty());
    }

    #[tokio::test]
    async fn test_scan_directory_with_exclusions() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        // Create files
        fs::write(root.join("keep.txt"), "keep").unwrap();
        fs::write(root.join("skip.tmp"), "skip").unwrap();
        fs::write(root.join("another.txt"), "another").unwrap();

        let entries = scan_directory(root, &["*.tmp".to_string()]).await.unwrap();

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"keep.txt"));
        assert!(names.contains(&"another.txt"));
        assert!(!names.contains(&"skip.tmp"));
    }

    #[tokio::test]
    async fn test_scan_directory_nested_structure() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        // Create nested structure
        fs::create_dir_all(root.join("a").join("b").join("c")).unwrap();
        fs::write(root.join("a").join("file_a.txt"), "a").unwrap();
        fs::write(root.join("a").join("b").join("file_b.txt"), "b").unwrap();
        fs::write(root.join("a").join("b").join("c").join("file_c.txt"), "c").unwrap();

        let entries = scan_directory(root, &[]).await.unwrap();

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
        assert!(names.contains(&"file_a.txt"));
        assert!(names.contains(&"file_b.txt"));
        assert!(names.contains(&"file_c.txt"));
    }

    #[tokio::test]
    async fn test_scan_directory_file_sizes() {
        let temp = tempdir().unwrap();
        let root = temp.path();

        fs::write(root.join("small.txt"), "x").unwrap();
        fs::write(root.join("medium.txt"), "x".repeat(1000)).unwrap();
        fs::write(root.join("empty.txt"), "").unwrap();

        let entries = scan_directory(root, &[]).await.unwrap();

        let small = entries.iter().find(|entry| entry.name == "small.txt").unwrap();
        assert_eq!(small.size, 1);

        let medium = entries.iter().find(|entry| entry.name == "medium.txt").unwrap();
        assert_eq!(medium.size, 1000);

        let empty = entries.iter().find(|entry| entry.name == "empty.txt").unwrap();
        assert_eq!(empty.size, 0);
    }

    #[test]
    fn test_file_watcher_new() {
        let watcher = FileWatcher::new(
            vec![PathBuf::from("C:\\Test1"), PathBuf::from("D:\\Test2")],
            vec!["*.bak".to_string()],
            200,
            true,
        );

        assert_eq!(watcher.watched_paths().len(), 2);
        assert_eq!(watcher.exclude_patterns().len(), 1);
        assert!(!watcher.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn test_file_watcher_for_path() {
        let path = PathBuf::from("C:\\SinglePath");
        let watcher = FileWatcher::for_path(path.clone());

        assert_eq!(watcher.watched_paths().len(), 1);
        assert_eq!(watcher.watched_paths()[0], path);
        assert_eq!(watcher.exclude_patterns(), [] as [String; 0]);
    }

    #[test]
    fn test_file_watcher_add_path() {
        let mut watcher = FileWatcher::new(vec![], vec![], DEFAULT_DEBOUNCE_MS, true);

        watcher.add_path(PathBuf::from("C:\\Path1"));
        assert_eq!(watcher.watched_paths.len(), 1);

        watcher.add_path(PathBuf::from("D:\\Path2"));
        assert_eq!(watcher.watched_paths.len(), 2);

        // Adding duplicate path should not increase count
        watcher.add_path(PathBuf::from("C:\\Path1"));
        assert_eq!(watcher.watched_paths.len(), 2);
    }

    #[test]
    fn test_file_watcher_add_exclude_pattern() {
        let mut watcher = FileWatcher::new(vec![], vec![], DEFAULT_DEBOUNCE_MS, true);

        watcher.add_exclude_pattern("*.tmp".to_string());
        assert_eq!(watcher.exclude_patterns.len(), 1);

        watcher.add_exclude_pattern("*.bak".to_string());
        assert_eq!(watcher.exclude_patterns.len(), 2);

        // Adding duplicate pattern should not increase count
        watcher.add_exclude_pattern("*.tmp".to_string());
        assert_eq!(watcher.exclude_patterns.len(), 2);
    }

    #[test]
    fn test_file_watcher_watched_paths() {
        let watcher = FileWatcher::new(
            vec![PathBuf::from("C:\\A"), PathBuf::from("D:\\B")],
            vec![],
            DEFAULT_DEBOUNCE_MS,
            true,
        );
        let paths = watcher.watched_paths();

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&PathBuf::from("C:\\A")));
        assert!(paths.contains(&PathBuf::from("D:\\B")));
    }

    #[test]
    fn test_file_watcher_stop() {
        let watcher = FileWatcher::new(vec![], vec![], DEFAULT_DEBOUNCE_MS, true);

        assert!(!watcher.shutdown.load(Ordering::Relaxed));

        watcher.stop();

        assert!(watcher.shutdown.load(Ordering::Relaxed));
    }

    #[test]
    fn test_scan_entry_with_unicode_name() {
        let entry = ScanEntry {
            path: PathBuf::from("C:\\文档\\文件.txt"),
            name: "文件.txt".to_string(),
            is_directory: false,
            size: 100,
            modified: None,
            created: None,
        };

        let file_entry = entry.into_file_entry(1);
        assert_eq!(file_entry.name, "文件.txt");
        assert!(file_entry.full_path.contains("文档"));
    }

    #[test]
    fn test_scan_entry_with_long_path() {
        let long_name = "a".repeat(200);
        let long_path = format!("C:\\{long_name}");

        let entry = ScanEntry {
            path: PathBuf::from(&long_path),
            name: long_name,
            is_directory: false,
            size: 0,
            modified: None,
            created: None,
        };

        let file_entry = entry.into_file_entry(1);
        assert_eq!(file_entry.name.len(), 200);
        assert_eq!(file_entry.full_path, long_path);
    }

    #[test]
    fn test_should_exclude_is_case_insensitive() {
        let watcher = FileWatcher {
            watched_paths: vec![],
            exclude_patterns: vec!["*.TMP".to_string(), "README.md".to_string()],
            exclude_globs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            recursive: true,
            shutdown: Arc::new(AtomicBool::new(false)),
            rescan_needed: Arc::new(AtomicBool::new(false)),
            registered_watches: Arc::new(AtomicUsize::new(0)),
        };

        assert!(watcher.should_exclude(Path::new("file.TMP")));
        assert!(watcher.should_exclude(Path::new("README.md")));
        assert!(watcher.should_exclude(Path::new("file.tmp")));
        assert!(watcher.should_exclude(Path::new("readme.md")));
        assert!(watcher.should_exclude(Path::new("README.MD")));
    }

    #[test]
    fn test_matches_exclude_patterns_suffix() {
        let patterns = vec!["*.tmp".to_string(), "*.log".to_string()];

        assert!(matches_exclude_patterns("file.tmp", &patterns));
        assert!(matches_exclude_patterns("C:\\Users\\test.log", &patterns));
        assert!(!matches_exclude_patterns("file.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_prefix() {
        let patterns = vec!["temp*".to_string()];

        assert!(matches_exclude_patterns("temp_file.txt", &patterns));
        assert!(matches_exclude_patterns("temporary", &patterns));
        assert!(!matches_exclude_patterns("file_temp.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_contains() {
        let patterns = vec!["*cache*".to_string()];

        assert!(matches_exclude_patterns("my_cache_dir", &patterns));
        assert!(matches_exclude_patterns("C:\\cache\\file.txt", &patterns));
        assert!(!matches_exclude_patterns("file.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_whole_component() {
        let patterns = vec!["node_modules".to_string()];

        assert!(matches_exclude_patterns("C:\\project\\node_modules\\pkg", &patterns));
        assert!(matches_exclude_patterns("node_modules", &patterns));
        assert!(!matches_exclude_patterns("C:\\project\\modules", &patterns));
        assert!(!matches_exclude_patterns(
            "C:\\project\\my_node_modules_backup",
            &patterns
        ));
    }

    #[test]
    fn test_matches_exclude_patterns_does_not_match_inside_file_names() {
        let patterns = vec!["Epic".to_string(), "GOG".to_string(), "Rockstar".to_string()];

        assert!(matches_exclude_patterns(
            "D:\\Games\\Epic\\Fortnite\\game.exe",
            &patterns
        ));
        assert!(matches_exclude_patterns("D:\\GOG", &patterns));
        assert!(matches_exclude_patterns("D:\\games\\rockstar\\gta.exe", &patterns));

        assert!(!matches_exclude_patterns(
            "Z:\\Data\\Holiday.TheEpicJourney.Part.One.1080p.mp4",
            &patterns
        ));
        assert!(!matches_exclude_patterns(
            "X:\\Videos\\Epic.Sample.Clip.1080p.mp4",
            &patterns
        ));
        assert!(!matches_exclude_patterns(
            "X:\\Videos\\Epic Games Trailer\\clip.mp4",
            &patterns
        ));
        assert!(!matches_exclude_patterns("X:\\Music\\Rockstar.Remix.mp3", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_multi_component() {
        let patterns = vec!["C:\\Windows".to_string(), "Vortex Mods\\cache".to_string()];

        assert!(matches_exclude_patterns("C:\\Windows", &patterns));
        assert!(matches_exclude_patterns("C:\\Windows\\System32\\cmd.exe", &patterns));
        assert!(matches_exclude_patterns("c:/windows/system32", &patterns));
        assert!(matches_exclude_patterns(
            "D:\\Games\\Vortex Mods\\cache\\file.bin",
            &patterns
        ));

        assert!(!matches_exclude_patterns("C:\\WindowsApps\\app.exe", &patterns));
        assert!(!matches_exclude_patterns("D:\\Windows\\file.txt", &patterns));
        assert!(!matches_exclude_patterns(
            "D:\\Games\\Vortex Mods\\mods\\file.bin",
            &patterns
        ));
    }

    #[test]
    fn test_matches_exclude_patterns_trailing_separator_and_unc() {
        let patterns = vec!["\\\\server\\share\\".to_string(), "#Recycle\\".to_string()];

        assert!(matches_exclude_patterns("\\\\server\\share\\docs\\file.txt", &patterns));
        assert!(matches_exclude_patterns("X:\\#Recycle\\Videos\\file.mp4", &patterns));
        assert!(!matches_exclude_patterns("\\\\server\\shared\\file.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_lone_wildcard_matches_everything() {
        let patterns = vec!["*".to_string()];

        assert!(matches_exclude_patterns("C:\\anything.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_double_wildcard_matches_everything() {
        let patterns = vec!["**".to_string()];

        assert!(matches_exclude_patterns("C:\\anything.txt", &patterns));
        assert!(matches_exclude_patterns("", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_contains_non_ascii() {
        let patterns = vec!["*äöü*".to_string(), "*日本*".to_string()];

        assert!(matches_exclude_patterns("C:\\Daten\\ÄÖÜ\\file.txt", &patterns));
        assert!(matches_exclude_patterns("C:\\日本語\\file.txt", &patterns));
        assert!(!matches_exclude_patterns("C:\\Daten\\aou\\file.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_non_ascii_prefix_and_suffix() {
        let patterns = vec!["*.été".to_string(), "日本*".to_string()];

        assert!(matches_exclude_patterns("C:\\photos\\vacances.ÉTÉ", &patterns));
        assert!(matches_exclude_patterns("日本語\\file.txt", &patterns));
        assert!(!matches_exclude_patterns("C:\\日本\\file.txt", &patterns));
    }

    #[test]
    fn test_matches_exclude_patterns_empty() {
        let patterns: Vec<String> = vec![];

        assert!(!matches_exclude_patterns("any_file.txt", &patterns));
        assert!(!matches_exclude_patterns("", &patterns));
    }
}

#[cfg(test)]
mod test_watcher_renames {
    use std::time::{Duration, Instant};

    use super::*;

    /// Collect debounced changes until `satisfied` returns true or the deadline passes.
    async fn collect_changes_until(
        receiver: &mut mpsc::Receiver<FileChangeEvent>,
        satisfied: impl Fn(&[FileChangeEvent]) -> bool,
    ) -> Vec<FileChangeEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut changes = Vec::new();
        while !satisfied(&changes) && Instant::now() < deadline {
            if let Ok(Some(change)) = tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
                changes.push(change);
            }
        }
        changes
    }

    #[tokio::test]
    async fn native_rename_events_become_rename_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("old.txt"), "contents").unwrap();
        std::fs::create_dir(root.join("old dir")).unwrap();
        std::fs::write(root.join("old dir").join("inside.txt"), "inside").unwrap();
        let watcher = FileWatcher::new(vec![root.clone()], Vec::new(), 100, true);
        let (mut receiver, _shutdown) = watcher.start().unwrap();
        // Give the platform watcher a moment to arm and drain any setup events.
        tokio::time::sleep(Duration::from_millis(500)).await;
        while receiver.try_recv().is_ok() {}

        std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        std::fs::rename(root.join("old dir"), root.join("new dir")).unwrap();

        let observed = |changes: &[FileChangeEvent], from: &str, to: &str| {
            let paired = changes.iter().any(|change| {
                matches!(change, FileChangeEvent::Renamed { from: old, to: new }
                    if old == &root.join(from) && new == &root.join(to))
            });
            let split_on_macos = cfg!(target_os = "macos")
                && changes
                    .iter()
                    .any(|change| matches!(change, FileChangeEvent::Created(path) if path == &root.join(to)))
                && changes
                    .iter()
                    .any(|change| matches!(change, FileChangeEvent::Deleted(path) if path == &root.join(from)));
            paired || split_on_macos
        };

        let changes = collect_changes_until(&mut receiver, |changes| {
            observed(changes, "old.txt", "new.txt") && observed(changes, "old dir", "new dir")
        })
        .await;
        watcher.stop();
        let described: Vec<String> = changes.iter().map(ToString::to_string).collect();
        for (from, to) in [("old.txt", "new.txt"), ("old dir", "new dir")] {
            assert!(
                observed(&changes, from, to),
                "{from} -> {to} was not reported as a rename: {described:?}"
            );
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod test_unreadable_child_directory {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn unreadable_subdirectory_does_not_discard_readable_siblings() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let blocked = root.join("blocked");
        let readable = root.join("readable");
        fs::create_dir_all(&blocked).unwrap();
        fs::create_dir_all(&readable).unwrap();
        fs::write(blocked.join("secret.txt"), "secret").unwrap();
        fs::write(readable.join("visible.txt"), "visible").unwrap();

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let result = scan_directory(&root, &[]).await;
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();

        let entries = result.expect("unreadable child directory should not fail entire root scan");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"visible.txt"), "{names:?}");
    }
}
