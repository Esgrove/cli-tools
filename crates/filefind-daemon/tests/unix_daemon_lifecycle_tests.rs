//! End-to-end subprocess lifecycle tests for `filefindd` on Unix (macOS and Linux).
//!
//! Spawns the compiled `filefindd` binary with private `HOME`, `FILEFIND_CONFIG`, `FILEFIND_DB`,
//! and `FILEFIND_IPC_PATH` locations outside two temporary roots.
//! The host index and running daemon are never touched.

#![cfg(not(windows))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use cli_tools_common::test_support::TempTree;
use filefind_common::{DaemonStateInfo, Database, IpcClient};
use tempfile::TempDir;

/// RAII guard that guarantees a spawned `filefindd` child process is terminated and reaped on drop.
struct ChildGuard {
    child: Option<Child>,
}

/// Isolated test environment with private config, database, socket, and two separate roots.
struct DaemonSandbox {
    /// Short private directory under `/tmp` so Unix domain socket paths stay well under 104 bytes on macOS.
    runtime_dir: TempDir,
    root_a: TempTree,
    root_b: TempTree,
    config_path: PathBuf,
    database_path: PathBuf,
    socket_path: PathBuf,
}

impl ChildGuard {
    /// Wrap a spawned child process for automatic cleanup.
    const fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Wait for the child to exit before `timeout`, returning whether it exited cleanly with status 0.
    fn wait_until_exit(&mut self, timeout: Duration) -> bool {
        let Some(ref mut child) = self.child else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                self.child = None;
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl DaemonSandbox {
    /// Build a sandbox with two populated roots and a configured `filefind.toml`.
    fn new() -> Self {
        let runtime_dir = tempfile::Builder::new()
            .prefix("ff-")
            .tempdir_in("/tmp")
            .expect("create short runtime dir");
        let root_a = TempTree::new();
        let root_b = TempTree::new();

        let first_root = dunce::canonicalize(root_a.path()).expect("canonicalize first root");
        let second_root = dunce::canonicalize(root_b.path()).expect("canonicalize second root");

        let home_dir = runtime_dir.path().join("h");
        fs::create_dir_all(&home_dir).expect("create home dir");
        let config_path = runtime_dir.path().join("filefind.toml");
        let database_path = runtime_dir.path().join("filefind.db");
        let socket_path = runtime_dir.path().join("d.sock");

        let config_toml = format!(
            "[daemon]\n\
             paths = [{:?}, {:?}]\n\
             exclude = [\"ignored_dir\"]\n\
             exclude_patterns = [\"*.tmp\"]\n\
             scan_interval_seconds = 3600\n\
             database_path = {:?}\n",
            first_root.to_string_lossy(),
            second_root.to_string_lossy(),
            database_path.to_string_lossy(),
        );
        fs::write(&config_path, config_toml).expect("write config");

        Self {
            runtime_dir,
            root_a,
            root_b,
            config_path,
            database_path,
            socket_path,
        }
    }

    fn root_a_path(&self) -> PathBuf {
        dunce::canonicalize(self.root_a.path()).expect("canonicalize root_a")
    }

    fn root_b_path(&self) -> PathBuf {
        dunce::canonicalize(self.root_b.path()).expect("canonicalize root_b")
    }

    /// Build a `filefindd` command completely isolated from host state.
    fn command(&self, args: &[&str]) -> Command {
        let home_dir = self.runtime_dir.path().join("h");
        let mut command = Command::new(env!("CARGO_BIN_EXE_filefindd"));
        command
            .args(args)
            .env("HOME", &home_dir)
            .env("XDG_CONFIG_HOME", &home_dir)
            .env("XDG_DATA_HOME", &home_dir)
            .env("XDG_CACHE_HOME", &home_dir)
            .env("XDG_RUNTIME_DIR", self.runtime_dir.path())
            .env("FILEFIND_CONFIG", &self.config_path)
            .env("FILEFIND_DB", &self.database_path)
            .env("FILEFIND_IPC_PATH", &self.socket_path)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        command
    }

    /// Run a short-lived `filefindd` subcommand to completion.
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run filefindd")
    }

    /// Spawn `filefindd start -f` as a foreground child guarded by [`ChildGuard`].
    fn spawn_foreground(&self) -> ChildGuard {
        let child = self
            .command(&["start", "-f"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn foreground filefindd");
        ChildGuard::new(child)
    }

    fn ipc_client(&self) -> IpcClient {
        IpcClient::new().with_pipe_path(self.socket_path.clone())
    }

    fn open_database(&self) -> Database {
        Database::open(&self.database_path).expect("open daemon database")
    }
}

/// Poll `predicate` until it returns `true` or `timeout` elapses.
fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    predicate()
}

/// Assert that a `filefindd` invocation succeeded, printing stdout and stderr otherwise.
fn assert_ok(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn empty_and_malformed_configs_fail_fast_with_clear_errors() {
    let runtime = tempfile::Builder::new().prefix("ff-cfg-").tempdir_in("/tmp").unwrap();
    let home = runtime.path().join("h");
    fs::create_dir_all(&home).unwrap();
    let missing_config = runtime.path().join("missing.toml");
    let broken_config = runtime.path().join("broken.toml");
    fs::write(&broken_config, "[daemon\npaths = [").unwrap();

    let empty_output = Command::new(env!("CARGO_BIN_EXE_filefindd"))
        .args(["start", "-f"])
        .env("HOME", &home)
        .env("FILEFIND_CONFIG", &missing_config)
        .env("FILEFIND_DB", runtime.path().join("filefind.db"))
        .env("FILEFIND_IPC_PATH", runtime.path().join("d.sock"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!empty_output.status.success());
    let stderr = String::from_utf8_lossy(&empty_output.stderr);
    assert!(stderr.contains("No index paths configured"), "{stderr}");
    assert!(stderr.contains("missing.toml"), "{stderr}");

    let broken_output = Command::new(env!("CARGO_BIN_EXE_filefindd"))
        .args(["scan"])
        .env("HOME", &home)
        .env("FILEFIND_CONFIG", &broken_config)
        .env("FILEFIND_DB", runtime.path().join("filefind.db"))
        .env("FILEFIND_IPC_PATH", runtime.path().join("d.sock"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!broken_output.status.success());
    let broken_stderr = String::from_utf8_lossy(&broken_output.stderr);
    assert!(broken_stderr.contains("broken.toml"), "{broken_stderr}");
}

/// Run the initial scan and return `(first_volume_id, second_volume_id)` after verifying exclusions.
fn verify_initial_scan_two_roots(sandbox: &DaemonSandbox, first_root: &Path, second_root: &Path) -> (i64, i64) {
    let scan_output = sandbox.run(&["scan"]);
    assert_ok(&scan_output, "filefindd scan");

    let database = sandbox.open_database();
    let volumes = database.get_all_volumes().expect("list indexed volumes");
    assert_eq!(volumes.len(), 2, "expected one volume per configured Unix root");
    let alpha_entry = database
        .get_file_by_path(&first_root.join("alpha.txt").to_string_lossy())
        .expect("query alpha.txt")
        .expect("alpha.txt indexed");
    let beta_entry = database
        .get_file_by_path(&second_root.join("beta.txt").to_string_lossy())
        .expect("query beta.txt")
        .expect("beta.txt indexed");
    assert_ne!(
        alpha_entry.volume_id, beta_entry.volume_id,
        "two Unix roots must not collapse onto one volume ID"
    );
    for excluded in [
        first_root.join("ignored_dir/skip.txt"),
        first_root.join(".hidden.txt"),
        second_root.join("scratch.tmp"),
    ] {
        assert!(
            database
                .get_file_by_path(&excluded.to_string_lossy())
                .expect("query excluded path")
                .is_none()
        );
    }
    (alpha_entry.volume_id, beta_entry.volume_id)
}

#[test]
fn unix_daemon_full_lifecycle_watches_two_roots_and_reconciles_across_restart() {
    let sandbox = DaemonSandbox::new();
    sandbox.root_a.file_with("alpha.txt", "initial-a");
    sandbox.root_a.file_with("old_pack/nested/clip.mkv", "video");
    sandbox.root_a.file_with("ignored_dir/skip.txt", "skip");
    sandbox.root_a.file_with(".hidden.txt", "hidden");
    sandbox.root_b.file_with("beta.txt", "hi");
    sandbox.root_b.file_with("scratch.tmp", "temp");

    let first_root = sandbox.root_a_path();
    let second_root = sandbox.root_b_path();

    // 1. One-time scan indexes both roots with distinct volumes and applies exclusions.
    let (first_volume_id, second_volume_id) = verify_initial_scan_two_roots(&sandbox, &first_root, &second_root);

    // 2. Start foreground daemon and wait until IPC reports Running with both watches active.
    let mut daemon = sandbox.spawn_foreground();
    let client = sandbox.ipc_client();
    assert!(
        wait_until(Duration::from_secs(15), || {
            client
                .get_status()
                .is_ok_and(|status| status.state == DaemonStateInfo::Running && status.monitored_volumes == 2)
        }),
        "daemon did not reach Running with 2 monitored roots"
    );

    // 3. A duplicate start must fail without displacing the live daemon.
    let duplicate = sandbox.run(&["start", "-f"]);
    assert!(!duplicate.status.success(), "duplicate daemon must fail to bind");
    assert!(client.is_daemon_running(), "first daemon must remain reachable");

    // 4. Live mutations across both roots (create, size modify, directory rename, excluded create).
    std::thread::sleep(Duration::from_millis(400));
    fs::write(first_root.join("live_added.txt"), "live").unwrap();
    fs::write(second_root.join("beta.txt"), "updated-longer-beta-contents").unwrap();
    fs::rename(first_root.join("old_pack"), first_root.join("new_pack")).unwrap();
    fs::write(second_root.join("live_ignored.tmp"), "ignore").unwrap();

    let expected_new_clip = first_root
        .join("new_pack/nested/clip.mkv")
        .to_string_lossy()
        .into_owned();
    let expected_old_pack = first_root.join("old_pack").to_string_lossy().into_owned();
    let expected_live_added = first_root.join("live_added.txt").to_string_lossy().into_owned();
    let expected_beta = second_root.join("beta.txt").to_string_lossy().into_owned();

    assert!(
        wait_until(Duration::from_secs(15), || {
            let db = sandbox.open_database();
            let live_ok = db
                .get_file_by_path(&expected_live_added)
                .ok()
                .flatten()
                .is_some_and(|entry| entry.volume_id == first_volume_id);
            let beta_ok = db
                .get_file_by_path(&expected_beta)
                .ok()
                .flatten()
                .is_some_and(|entry| entry.size == 28 && entry.volume_id == second_volume_id);
            let clip_ok = db.get_file_by_path(&expected_new_clip).ok().flatten().is_some();
            let old_gone = db.get_file_by_path(&expected_old_pack).ok().flatten().is_none();
            live_ok && beta_ok && clip_ok && old_gone
        }),
        "watcher did not reconcile live create, modify, and directory rename into SQLite"
    );
    assert!(
        sandbox
            .open_database()
            .get_file_by_path(&second_root.join("live_ignored.tmp").to_string_lossy())
            .unwrap()
            .is_none()
    );

    // 5. Status and stop commands succeed and clean up the socket.
    let status_output = sandbox.run(&["status"]);
    assert_ok(&status_output, "filefindd status");
    assert!(String::from_utf8_lossy(&status_output.stdout).contains("running"));

    assert_ok(&sandbox.run(&["stop"]), "filefindd stop");
    assert!(daemon.wait_until_exit(Duration::from_secs(10)));
    assert!(wait_until(Duration::from_secs(5), || !sandbox.socket_path.exists()));

    // 6. Offline edits while stopped are reconciled on restart without passing --rescan.
    verify_offline_edits_reconcile_on_restart(&sandbox, &first_root, &second_root, second_volume_id);
}

/// Verify that restarting the daemon without `--rescan` reconciles edits made while stopped.
fn verify_offline_edits_reconcile_on_restart(
    sandbox: &DaemonSandbox,
    first_root: &Path,
    second_root: &Path,
    second_volume_id: i64,
) {
    fs::remove_file(first_root.join("alpha.txt")).expect("remove alpha.txt while stopped");
    fs::write(second_root.join("offline_added.txt"), "offline").expect("write offline file while stopped");

    let client = sandbox.ipc_client();
    let mut restarted = sandbox.spawn_foreground();
    assert!(
        wait_until(Duration::from_secs(15), || {
            client
                .get_status()
                .is_ok_and(|status| status.state == DaemonStateInfo::Running)
        }),
        "restarted daemon did not reach Running"
    );

    let alpha_path = first_root.join("alpha.txt").to_string_lossy().into_owned();
    let offline_path = second_root.join("offline_added.txt").to_string_lossy().into_owned();
    assert!(
        wait_until(Duration::from_secs(10), || {
            let db = sandbox.open_database();
            let alpha_gone = db.get_file_by_path(&alpha_path).ok().flatten().is_none();
            let offline_indexed = db
                .get_file_by_path(&offline_path)
                .ok()
                .flatten()
                .is_some_and(|entry| entry.volume_id == second_volume_id);
            alpha_gone && offline_indexed
        }),
        "restart did not reconcile offline edits"
    );

    let final_stop = sandbox.run(&["stop"]);
    assert_ok(&final_stop, "second filefindd stop");
    assert!(
        restarted.wait_until_exit(Duration::from_secs(10)),
        "restarted daemon did not exit cleanly"
    );
}

#[test]
fn detect_reports_configured_unix_roots() {
    let sandbox = DaemonSandbox::new();
    let output = sandbox.run(&["detect"]);
    assert_ok(&output, "filefindd detect");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Configured Index Roots"), "{stdout}");
    assert!(stdout.contains("accessible"), "{stdout}");
    let _keep: &Path = sandbox.root_a.path();
}
