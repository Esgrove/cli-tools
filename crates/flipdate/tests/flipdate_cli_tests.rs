//! End to end tests for the `flipdate` binary.
//!
//! Each test runs the compiled binary on a nested temporary tree and checks the exit code, the output,
//! and the exact tree left on disk.
//! The user config during tests resolves to `crates/common/tests/fixtures/sample_config.toml`,
//! which turns every `flip_date` option off, so each test passes the flags it relies on.
//! The filefind index variables point inside the tree, so no run touches the user's index.

use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;
use filefind_common::test_support::IndexFixture;

/// Run the binary with the given arguments and an isolated home directory.
fn run_flipdate(tree: &TempTree, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flipdate"))
        .args(args)
        .current_dir(tree.path())
        .env("HOME", tree.path())
        .env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .env("FILEFIND_DB", tree.join("home/filefind.db"))
        .env("FILEFIND_CONFIG", tree.join("home/filefind.toml"))
        .output()
        .expect("flipdate should start")
}

/// Standard output as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Realistic library with dated files at several depths, plus files that must stay as they are.
fn library() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("media/Report 31.12.2020.txt", "report");
    tree.file_with("media/music/Song 01.05.2021.mp3", "song");
    tree.file_with("media/music/live/Clip 15.06.2022.mp4", "clip");
    tree.file_with("media/music/live/photos/Photo 2.3.2023.jpg", "photo");
    tree.file_with("media/music/live/photos/plain.txt", "plain");
    tree.file_with("media/.hidden/Secret 01.02.2020.txt", "secret");
    tree.file_with("media/music/ok 2020.01.02.txt", "ok");
    tree
}

#[test]
fn dryrun_lists_planned_renames_and_changes_nothing() {
    let tree = library();
    let before = tree.snapshot();

    let output = run_flipdate(&tree, &["media", "--recurse", "--print"]);

    assert!(output.status.success(), "{output:?}");
    let printed = stdout(&output);
    for expected in [
        "Report 2020.12.31.txt",
        "Song 2021.05.01.mp3",
        "Clip 2022.06.15.mp4",
        "Photo 2023.03.02.jpg",
    ] {
        assert!(printed.contains(expected), "missing {expected} in:\n{printed}");
    }
    assert!(
        !printed.contains("Secret"),
        "hidden files should be skipped:\n{printed}"
    );
    assert_eq!(tree.snapshot(), before);
}

#[test]
fn print_mode_omits_a_rename_blocked_by_the_canonical_file() {
    let tree = TempTree::new();
    tree.file_with("media/Report 31.12.2020.txt", "incoming report");
    tree.file_with("media/Report 2020.12.31.txt", "existing report");
    tree.file_with("media/Song 01.05.2021.mp3", "song");
    let before = tree.snapshot();

    let preview = run_flipdate(&tree, &["media", "--print"]);

    assert!(preview.status.success(), "{preview:?}");
    let printed = stdout(&preview);
    assert!(printed.contains("Song 2021.05.01.mp3"), "{printed}");
    assert!(!printed.contains("Report"), "{printed}");
    assert!(String::from_utf8_lossy(&preview.stderr).contains("File already exists"));
    assert_eq!(tree.snapshot(), before);

    let run = run_flipdate(&tree, &["media"]);

    assert!(run.status.success(), "{run:?}");
    assert_eq!(tree.read("media/Report 31.12.2020.txt"), "incoming report");
    assert_eq!(tree.read("media/Report 2020.12.31.txt"), "existing report");
    assert_eq!(tree.read("media/Song 2021.05.01.mp3"), "song");
}

#[test]
fn recursive_run_renames_every_level_and_a_second_run_changes_nothing() {
    let tree = library();

    let output = run_flipdate(&tree, &["media", "--recurse"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        tree.files(),
        vec![
            "media/.hidden/Secret 01.02.2020.txt",
            "media/Report 2020.12.31.txt",
            "media/music/Song 2021.05.01.mp3",
            "media/music/live/Clip 2022.06.15.mp4",
            "media/music/live/photos/Photo 2023.03.02.jpg",
            "media/music/live/photos/plain.txt",
            "media/music/ok 2020.01.02.txt",
        ]
    );
    assert_eq!(tree.read("media/music/live/photos/Photo 2023.03.02.jpg"), "photo");

    let after_first_run = tree.snapshot();
    let output = run_flipdate(&tree, &["media", "--recurse"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(tree.snapshot(), after_first_run);
}

#[test]
fn without_recurse_only_the_top_level_is_renamed() {
    let tree = library();

    let output = run_flipdate(&tree, &["media"]);

    assert!(output.status.success(), "{output:?}");
    assert!(tree.exists("media/Report 2020.12.31.txt"));
    assert!(tree.exists("media/music/Song 01.05.2021.mp3"));
}

#[test]
fn directory_mode_renames_nested_directories() {
    let tree = TempTree::new();
    tree.file_with("trips/Trip 24.12.2019/Day 01.02.2020/notes.txt", "notes");

    let output = run_flipdate(&tree, &["trips", "--dir", "--recurse"]);

    assert!(output.status.success(), "{output:?}");
    assert_eq!(tree.read("trips/2019-12-24 Trip/2020-02-01 Day/notes.txt"), "notes");
}

#[test]
fn force_keeps_both_files_that_map_to_the_same_name() {
    let tree = TempTree::new();
    tree.file_with("files/x 01.02.2020.txt", "first");
    tree.file_with("files/x 1.2.2020.txt", "second");

    let output = run_flipdate(&tree, &["files", "--force"]);

    assert!(output.status.success(), "{output:?}");
    let mut contents: Vec<String> = tree.files().iter().map(|relative| tree.read(relative)).collect();
    contents.sort();
    assert_eq!(contents, vec!["first", "second"]);
}

/// Check that a blocked rename leaves an unrelated swap untouched.
#[test]
fn a_failed_rename_restores_staged_inputs_from_an_unrelated_swap() {
    let tree = TempTree::new();
    tree.file_with("files/A 2005.12.23.jpg", "blocked source");
    tree.file_with("files/A 2023.12.05.jpg/keep.txt", "existing directory");
    tree.file_with("files/B 2005.12.23.jpg", "first swap input");
    tree.file_with("files/B 2023.12.05.jpg", "second swap input");
    let before = tree.snapshot();

    let output = run_flipdate(&tree, &["files", "--swap", "--force"]);

    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("A 2005.12.23.jpg"), "{error}");
    assert_eq!(tree.snapshot(), before);
}

/// Verify that recovery reports the temporary path instead of replacing a completed rename.
#[test]
fn a_staged_input_keeps_its_recovery_path_when_its_name_is_occupied() {
    let tree = TempTree::new();
    tree.file_with("files/05.12.2023.jpg", "first chain input");
    tree.file_with("files/2023.12.05.jpg", "second chain input");
    tree.dir("files/2005.12.23.jpg");

    let output = run_flipdate(&tree, &["files", "--swap", "--force"]);

    assert!(!output.status.success(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("2005.12.23.jpg"), "{error}");
    assert!(error.contains("2023.12.05.jpg.flipdate-tmp"), "{error}");
    assert_eq!(tree.read("files/2023.12.05.jpg"), "first chain input");
    assert_eq!(tree.read("files/2023.12.05.jpg.flipdate-tmp"), "second chain input");
}

#[test]
fn missing_path_is_an_error() {
    let tree = TempTree::new();

    let output = run_flipdate(&tree, &["does-not-exist"]);

    assert!(!output.status.success(), "{output:?}");
    assert_eq!(tree.snapshot(), Vec::<String>::new());
}

#[test]
fn renames_move_the_entries_of_an_existing_filefind_index() {
    let tree = TempTree::new();
    let report = tree.file_with("media/Report 31.12.2020.txt", "report");
    tree.file_with("media/Song 01.05.2021.mp3", "song");
    let index = IndexFixture::create(&tree.join("home/filefind.db"), &tree.join("media"), &[report]);

    let output = run_flipdate(&tree, &["--recurse", "media"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert!(tree.exists("media/Song 2021.05.01.mp3"));
    assert_eq!(index.indexed_paths(), vec!["Report 2020.12.31.txt"]);
}
