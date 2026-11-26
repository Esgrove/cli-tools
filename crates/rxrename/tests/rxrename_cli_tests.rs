//! End to end tests for the `rxrename` binary.
//!
//! Each test runs the compiled binary on a nested temporary tree and checks the exit code, the output,
//! and the exact tree left on disk.
//! The home directory points at the temporary tree, so the real user config and trash are never touched.

use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

/// Run the binary with the given arguments and an isolated home directory.
fn run_rxrename(tree: &TempTree, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rxrename"))
        .args(args)
        .current_dir(tree.path())
        .env("HOME", tree.path())
        .env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .expect("rxrename should start")
}

/// Standard output as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Download folder with duplicates at several depths, some of which replace an older copy.
fn downloads() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("downloads/top_1.mp4", "new top");
    tree.file_with("downloads/top.mp4", "old top");
    tree.file_with("downloads/music/song_1.mp3", "new song");
    tree.file_with("downloads/music/live/clip_1.mkv", "new clip");
    tree.file_with("downloads/music/live/clip.mkv", "old clip");
    tree.file_with("downloads/music/live/photos/photo_1.jpg", "new photo");
    tree.file_with("downloads/music/live/photos/keep_10.jpg", "keep");
    tree.file_with("downloads/.cache/hidden_1.txt", "hidden");
    tree
}

/// Expected tree after one real run over `downloads`.
const RENAMED: [&str; 11] = [
    "downloads/",
    "downloads/.cache/",
    "downloads/.cache/hidden_1.txt: hidden",
    "downloads/music/",
    "downloads/music/live/",
    "downloads/music/live/clip.mkv: new clip",
    "downloads/music/live/photos/",
    "downloads/music/live/photos/keep_10.jpg: keep",
    "downloads/music/live/photos/photo.jpg: new photo",
    "downloads/music/song.mp3: new song",
    "downloads/top.mp4: new top",
];

#[test]
fn dryrun_lists_planned_changes_and_changes_nothing() {
    let tree = downloads();
    let before = tree.snapshot();

    let output = run_rxrename(&tree, &["downloads", "--dryrun"]);

    assert!(output.status.success(), "{output:?}");
    let printed = stdout(&output);
    assert!(printed.contains("4 files would be renamed"), "{printed}");
    assert!(printed.contains("2 files would be deleted"), "{printed}");
    assert!(printed.contains("photo_1.jpg"), "{printed}");
    assert!(!printed.contains("hidden_1.txt"), "{printed}");
    assert_eq!(tree.snapshot(), before);
}

#[test]
fn real_run_renames_every_level_and_a_second_run_changes_nothing() {
    let tree = downloads();

    let output = run_rxrename(&tree, &["downloads"]);

    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("4 files renamed"), "{}", stdout(&output));
    assert_eq!(tree.snapshot(), RENAMED);

    let output = run_rxrename(&tree, &["downloads"]);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("0 files renamed"), "{}", stdout(&output));
    assert_eq!(tree.snapshot(), RENAMED);
}

#[test]
fn blocked_suffix_chain_keeps_both_sources() {
    let tree = TempTree::with_entries(&["downloads/x.txt/"]);
    tree.file_with("downloads/x_1.txt", "newer");
    tree.file_with("downloads/x_1_1.txt", "newest");
    let before = tree.snapshot();

    let preview = run_rxrename(&tree, &["downloads", "--dryrun"]);
    assert!(preview.status.success(), "{preview:?}");
    let printed = stdout(&preview);
    assert!(printed.contains("0 files would be renamed"), "{printed}");
    assert!(printed.contains("0 files would be deleted"), "{printed}");

    let output = run_rxrename(&tree, &["downloads"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(tree.snapshot(), before);
    let printed = stdout(&output);
    assert!(printed.contains("0 files renamed"), "{printed}");
    assert!(printed.contains("0 files deleted"), "{printed}");
}

#[test]
fn preview_reuses_a_vacated_suffix_chain_name_with_different_case() {
    let tree = TempTree::new();
    tree.file_with("downloads/Clip_1.txt", "newer");
    tree.file_with("downloads/clip_1_1.txt", "newest");
    let before = tree.snapshot();

    let preview = run_rxrename(&tree, &["downloads", "--dryrun"]);

    assert!(preview.status.success(), "{preview:?}");
    let printed = stdout(&preview);
    assert!(printed.contains("2 files would be renamed"), "{printed}");
    assert!(printed.contains("0 files would be deleted"), "{printed}");
    assert_eq!(tree.snapshot(), before);

    let run = run_rxrename(&tree, &["downloads"]);

    assert!(run.status.success(), "{run:?}");
    assert_eq!(tree.read("downloads/Clip.txt"), "newer");
    assert_eq!(tree.read("downloads/clip_1.txt"), "newest");
}

#[test]
fn missing_path_is_an_error() {
    let tree = TempTree::new();

    let output = run_rxrename(&tree, &["does-not-exist"]);

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(tree.snapshot(), Vec::<String>::new());
}

#[test]
fn a_file_as_the_root_is_an_error() {
    let tree = TempTree::new();
    tree.file_with("clip_1.mp4", "new");

    let output = run_rxrename(&tree, &["clip_1.mp4"]);

    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Not a directory"),
        "{output:?}"
    );
    assert_eq!(tree.snapshot(), vec!["clip_1.mp4: new"]);
}
