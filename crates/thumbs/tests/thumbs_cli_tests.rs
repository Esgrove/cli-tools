//! Exit status of thumbnail runs with isolated media tools.

#![cfg(unix)]

use std::fs::{Permissions, set_permissions};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

/// Run the CLI with fake tools and no user configuration.
fn run(tree: &TempTree, arguments: &[&str]) -> Output {
    let probe = tree.file_with(
        "bin/ffprobe",
        "#!/bin/sh\nprintf 'codec_name=h264\nwidth=1920\nheight=1080\nduration=100\n'\n",
    );
    let encoder = tree.file_with(
        "bin/ffmpeg",
        r#"#!/bin/sh
for argument; do output=$argument; done
case $output in
    -version) exit 0;;
    */good.jpg) echo 'test thumbnail' > "$output";;
    *) echo 'test thumbnail failure' >&2; exit 1;;
esac
"#,
    );
    for tool in [probe, encoder] {
        set_permissions(tool, Permissions::from_mode(0o755)).expect("fake media tool should be executable");
    }
    Command::new(env!("CARGO_BIN_EXE_thumbs"))
        .arg(tree.dir("videos"))
        .args(arguments)
        .env("PATH", tree.join("bin"))
        .env("HOME", tree.dir("home"))
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("thumbs should start and exit")
}

#[test]
fn all_thumbnail_generations_fail() {
    let tree = TempTree::with_entries(&["videos/bad.mp4", "videos/skipped.mp4", "videos/Screens/skipped.jpg"]);
    let output = run(&tree, &[]);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("0 successful, 1 failed, 1 skipped"));
}

#[test]
fn skips_dry_runs_and_partial_success_are_successful() {
    let tree = TempTree::with_entries(&["videos/skipped.mp4", "videos/Screens/skipped.jpg"]);
    assert!(run(&tree, &[]).status.success());

    tree.file("videos/bad.mp4");
    assert!(run(&tree, &["--print"]).status.success());
    assert!(!tree.exists("videos/Screens/bad.jpg"));

    tree.file("videos/good.mp4");
    let output = run(&tree, &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(tree.exists("videos/Screens/good.jpg"));
}
