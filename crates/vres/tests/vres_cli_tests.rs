//! Exit status of resolution labeling runs with an isolated `ffprobe`.

#![cfg(unix)]

use std::fs::{Permissions, set_permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

/// Run the CLI with a fake probe and no user configuration.
fn run(tree: &TempTree, input: &Path) -> Output {
    let probe = tree.file_with(
        "bin/ffprobe",
        r"#!/bin/sh
for argument; do input=$argument; done
case $input in
    */bad.mp4) echo 'test probe failure' >&2; exit 1;;
    *) printf 'width=1920\nheight=1080\n';;
esac
",
    );
    set_permissions(probe, Permissions::from_mode(0o755)).expect("fake ffprobe should be executable");
    Command::new(env!("CARGO_BIN_EXE_vres"))
        .arg(input)
        .env("PATH", tree.join("bin"))
        .env("HOME", tree.dir("home"))
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("vres should start and exit")
}

#[test]
fn all_probes_fail() {
    let tree = TempTree::with_entries(&["videos/bad.mp4", "videos/skipped.1080p.mp4"]);
    let output = run(&tree, &tree.join("videos"));

    assert!(String::from_utf8_lossy(&output.stderr).contains("test probe failure"));
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn all_renames_fail() {
    let tree = TempTree::with_entries(&["videos/blocked.mp4", "videos/blocked.1080p.mp4"]);
    let output = run(&tree, &tree.join("videos"));

    assert!(String::from_utf8_lossy(&output.stdout).contains("Renamed 0 files"));
    assert!(!output.status.success(), "{output:?}");
    assert!(tree.exists("videos/blocked.mp4"));
}

#[test]
fn up_to_date_files_and_partially_successful_runs_succeed() {
    let tree = TempTree::with_entries(&["videos/skipped.1080p.mp4"]);
    assert!(run(&tree, &tree.join("videos/skipped.1080p.mp4")).status.success());
    assert!(run(&tree, &tree.join("videos")).status.success());

    tree.file("videos/good.mp4");
    tree.file("videos/bad.mp4");
    let output = run(&tree, &tree.join("videos"));
    assert!(output.status.success(), "{output:?}");
    assert!(tree.exists("videos/good.1080p.mp4"));
}
