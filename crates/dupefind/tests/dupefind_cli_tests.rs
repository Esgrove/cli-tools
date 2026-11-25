//! End to end tests that run the `dupefind` binary against nested directory trees.
//!
//! They cover print mode leaving the tree untouched, moving duplicates out of a nested tree without losing files,
//! repeated runs, and the exit status for paths that cannot be scanned.
//! The child process inherits `CARGO_MANIFEST_DIR`,
//! so it reads the sample config fixture instead of the user config.

use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

/// Run the `dupefind` binary with the given arguments.
fn dupefind(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dupefind"))
        .args(arguments)
        .output()
        .expect("dupefind should run")
}

/// Standard output and error of a finished run as one lossy string.
fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Tree with the same show in three nested directories and one unrelated file.
fn tree_with_duplicates() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("shows/season1/Some.Show.mp4", "one");
    tree.file_with("shows/season1/extra/Some.Show.1080p.mp4", "two");
    tree.file_with("archive/2020/old/deeper/Some.Show.mkv", "three");
    tree.file_with("shows/Other.Thing.mp4", "unique");
    tree.file_with("shows/notes.txt", "not a video");
    tree.file_with(".hidden/Some.Show.mp4", "hidden");
    tree
}

/// Contents of every file in the tree, sorted.
fn contents(tree: &TempTree) -> Vec<String> {
    let mut contents: Vec<String> = tree.files().iter().map(|relative| tree.read(relative)).collect();
    contents.sort();
    contents
}

/// Files outside the duplicates directory.
fn files_outside_duplicates(tree: &TempTree) -> Vec<String> {
    tree.files()
        .into_iter()
        .filter(|path| !path.starts_with("Duplicates/"))
        .collect()
}

#[test]
fn print_mode_reports_the_group_and_changes_nothing() {
    let tree = tree_with_duplicates();
    let before = tree.snapshot();
    let root = tree.path().to_string_lossy().into_owned();

    let output = dupefind(&[&root, "--recurse", "--print"]);

    let text = output_text(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("Some.Show.mp4"), "{text}");
    assert!(text.contains("Some.Show.mkv"), "{text}");
    assert!(
        !text.contains("Other.Thing"),
        "a unique file is not a duplicate: {text}"
    );
    assert_eq!(tree.snapshot(), before);
}

#[test]
fn print_mode_without_recursion_finds_nothing_in_subdirectories() {
    let tree = tree_with_duplicates();
    let before = tree.snapshot();
    let root = tree.path().to_string_lossy().into_owned();

    let output = dupefind(&[&root, "-p"]);

    let text = output_text(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("No duplicates found"), "{text}");
    assert_eq!(tree.snapshot(), before);
}

#[test]
fn moving_keeps_the_best_copy_in_place_and_loses_nothing() {
    let tree = tree_with_duplicates();
    let before = contents(&tree);
    let root = tree.path().to_string_lossy().into_owned();

    let output = dupefind(&[&root, "-r", "-m"]);

    let text = output_text(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(contents(&tree), before, "no file contents may be lost");
    assert_eq!(
        files_outside_duplicates(&tree),
        vec![
            ".hidden/Some.Show.mp4",
            "shows/Other.Thing.mp4",
            "shows/notes.txt",
            "shows/season1/extra/Some.Show.1080p.mp4",
        ],
        "only the 1080p copy and unrelated files stay: {:?}",
        tree.snapshot()
    );
}

#[test]
fn a_second_move_run_changes_nothing() {
    let tree = tree_with_duplicates();
    let root = tree.path().to_string_lossy().into_owned();

    let first = dupefind(&[&root, "-r", "-m"]);
    assert!(first.status.success(), "{}", output_text(&first));
    let after_first_run = tree.snapshot();
    let second = dupefind(&[&root, "-r", "-m"]);

    assert!(second.status.success(), "{}", output_text(&second));
    assert_eq!(tree.snapshot(), after_first_run);
}

#[test]
fn a_missing_path_fails() {
    let tree = TempTree::new();
    let missing = tree.join("missing").to_string_lossy().into_owned();

    let output = dupefind(&[&missing, "-p"]);

    assert!(!output.status.success(), "{}", output_text(&output));
}

#[test]
fn an_empty_nested_tree_finds_no_duplicates() {
    let tree = TempTree::with_entries(&["a/b/c/", "d/"]);
    let root = tree.path().to_string_lossy().into_owned();

    let output = dupefind(&[&root, "-r", "-m"]);

    let text = output_text(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("No duplicates found"), "{text}");
    assert_eq!(tree.dirs(), vec!["a", "a/b", "a/b/c", "d"]);
}
