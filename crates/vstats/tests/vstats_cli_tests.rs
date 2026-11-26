//! Exit status of video statistics runs with an isolated `ffprobe`.

#![cfg(unix)]

use std::fs::{Permissions, set_permissions};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

/// Run the CLI with fake tools and no user configuration.
fn run(tree: &TempTree) -> Output {
    let probe = tree.file_with(
        "bin/ffprobe",
        r"#!/bin/sh
for argument; do input=$argument; done
case $input in
    */good.mp4) printf 'codec_name=h264\nwidth=1920\nheight=1080\nduration=10\n';;
    *) echo 'test probe failure' >&2; exit 1;;
esac
",
    );
    set_permissions(probe, Permissions::from_mode(0o755)).expect("fake ffprobe should be executable");
    Command::new(env!("CARGO_BIN_EXE_vstats"))
        .arg(tree.dir("videos"))
        .env("PATH", tree.join("bin"))
        .env("HOME", tree.dir("home"))
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("vstats should start and exit")
}

#[test]
fn all_probes_fail() {
    let tree = TempTree::with_entries(&["videos/bad.mp4"]);
    let output = run(&tree);

    assert!(String::from_utf8_lossy(&output.stdout).contains("could not be probed"));
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn empty_and_partially_successful_scans_succeed() {
    let tree = TempTree::new();
    assert!(run(&tree).status.success());

    tree.file("videos/good.mp4");
    tree.file("videos/bad.mp4");
    let output = run(&tree);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("good.mp4"));
}
