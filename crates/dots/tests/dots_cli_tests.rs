//! End to end tests for the `dots` binary on nested directory trees.
//!
//! Each test runs the compiled binary against a temporary tree and asserts the whole tree afterwards.
//! The binary runs with `HOME` pointing at an empty directory and without `CARGO_MANIFEST_DIR`,
//! so it reads no user config and uses the built-in defaults.

use std::path::Path;
use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

/// Run the binary with the given arguments and an empty home directory.
fn run_dots(home: &Path, arguments: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dots"))
        .args(arguments)
        .arg(root)
        .env_remove("CARGO_MANIFEST_DIR")
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .expect("the dots binary should run")
}

/// Standard output as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Assert that the process exited successfully, showing its output otherwise.
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "dots should succeed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Library tree with unformatted names at several depths, a hidden directory, and formatted names.
fn library_tree() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("library/top level file.txt", "top");
    tree.file_with("library/show name/some episode one.mkv", "one");
    tree.file_with("library/show name/season one/another_episode two.mkv", "two");
    tree.file_with("library/show name/season one/extras/behind the scenes.mp4", "extras");
    tree.file_with("library/show name/season one/Already.Formatted.mkv", "formatted");
    tree.file_with("library/.hidden dir/secret file.mkv", "secret");
    tree.dir("library/empty dir");
    tree
}

#[test]
fn dryrun_reports_the_renames_and_changes_nothing() {
    let tree = library_tree();
    let home = TempTree::new();
    let before = tree.snapshot();

    let output = run_dots(home.path(), &["--recurse", "--print"], &tree.join("library"));

    assert_success(&output);
    assert_eq!(tree.snapshot(), before);
    let printed = stdout(&output);
    assert!(printed.contains("Dryrun: would have renamed 4 files"), "{printed}");
    assert!(
        !printed.contains("secret"),
        "hidden files should not be listed:\n{printed}"
    );
}

#[test]
fn recursive_run_renames_files_at_every_depth_and_is_idempotent() {
    let tree = library_tree();
    let home = TempTree::new();
    let root = tree.join("library");

    let first = run_dots(home.path(), &["--recurse"], &root);
    assert_success(&first);
    assert!(stdout(&first).contains("Renamed 4 files"), "{}", stdout(&first));

    let expected = vec![
        "library/",
        "library/.hidden dir/",
        "library/.hidden dir/secret file.mkv: secret",
        "library/Top.Level.File.txt: top",
        "library/empty dir/",
        "library/show name/",
        "library/show name/Some.Episode.One.mkv: one",
        "library/show name/season one/",
        "library/show name/season one/Already.Formatted.mkv: formatted",
        "library/show name/season one/Another.Episode.Two.mkv: two",
        "library/show name/season one/extras/",
        "library/show name/season one/extras/Behind.the.Scenes.mp4: extras",
    ];
    assert_eq!(tree.snapshot(), expected);

    let second = run_dots(home.path(), &["--recurse"], &root);
    assert_success(&second);
    assert_eq!(tree.snapshot(), expected);
}

#[test]
fn recursive_directory_run_renames_nested_directories_deepest_first() {
    let tree = library_tree();
    let home = TempTree::new();
    let root = tree.join("library");

    let output = run_dots(home.path(), &["--recurse", "--directory"], &root);
    assert_success(&output);

    assert_eq!(
        tree.dirs(),
        vec![
            "library",
            "library/.hidden dir",
            "library/Empty Dir",
            "library/Show Name",
            "library/Show Name/Season One",
            "library/Show Name/Season One/Extras",
        ]
    );
    assert_eq!(tree.files().len(), 6, "renaming directories should keep every file");

    let again = run_dots(home.path(), &["--recurse", "--directory"], &root);
    assert_success(&again);
    assert_eq!(tree.dirs().len(), 6);
}

#[test]
fn conflicting_targets_keep_every_file() {
    let tree = TempTree::new();
    tree.file_with("library/nested/some file.mkv", "first");
    tree.file_with("library/nested/some_file.mkv", "second");
    let home = TempTree::new();

    let output = run_dots(home.path(), &["--recurse"], &tree.join("library"));
    assert_success(&output);

    let files = tree.files();
    assert_eq!(files.len(), 2, "no file should be lost: {files:?}");
    assert!(tree.exists("library/nested/Some.File.mkv"));
}

#[test]
fn a_missing_path_is_an_error() {
    let tree = TempTree::new();
    let home = TempTree::new();

    let output = run_dots(home.path(), &[], &tree.join("does not exist"));

    assert!(!output.status.success(), "a missing path should fail");
    assert_eq!(tree.files(), Vec::<String>::new());
}
