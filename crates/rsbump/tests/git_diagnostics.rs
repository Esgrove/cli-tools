//! Verify that Git diagnostics never let `rsbump` bypass the dirty tree guard.

use std::path::Path;
use std::process::Command;

use cli_tools_common::test_support::TempTree;

const MANIFEST: &str = "[package]\nname = 'test-project'\nrust-version = '1.56'\n";

/// Run rsbump with Git trace output enabled and an optional force flag.
fn run(root: &Path, flags: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rsbump"))
        .arg(root.join("Cargo.toml"))
        .args(flags)
        .env("GIT_TRACE", "1")
        .output()
        .expect("execute the rsbump test binary")
}

#[test]
fn trace_output_does_not_hide_a_dirty_repository() {
    let tree = TempTree::new();
    tree.file_with("Cargo.toml", MANIFEST);
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(tree.path())
            .status()
            .unwrap()
            .success()
    );

    let output = run(tree.path(), &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("Skipped, git repository has uncommitted changes"));
    assert_eq!(tree.read("Cargo.toml"), MANIFEST);
}

#[test]
fn git_probe_errors_stop_edits_even_with_force() {
    let tree = TempTree::new();
    tree.file_with("Cargo.toml", MANIFEST);
    tree.file_with(".git", "not a valid gitfile\n");

    let output = run(tree.path(), &["--force"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("invalid gitfile format"));
    assert_eq!(tree.read("Cargo.toml"), MANIFEST);
}
