//! End to end tests for the `dirmove` binary on nested directory trees.
//!
//! Each test runs the compiled binary against a temporary tree and asserts the whole tree afterwards.
//! The binary runs with `HOME` pointing at an empty directory and without `CARGO_MANIFEST_DIR`,
//! so it reads no user config and keeps its name database inside that directory.
//! The auto flag answers every prompt, so no test waits for input.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use cli_tools_common::test_support::TempTree;

/// Run the binary with the given arguments and an empty home directory.
fn run_dirmove(home: &Path, arguments: &[&str], root: &Path) -> Output {
    dirmove_command(home)
        .args(arguments)
        .arg(root)
        .output()
        .expect("the dirmove binary should run")
}

/// Build a binary invocation that reads its config and keeps its database under `home`.
fn dirmove_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dirmove"));
    command
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    command
}

/// Return whether the name database exists anywhere under `home`.
fn database_exists(home: &TempTree) -> bool {
    home.files().iter().any(|file| file.ends_with("dirmove.db"))
}

/// Standard output as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Assert that the process exited successfully, showing its output otherwise.
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "dirmove should succeed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Library with loose episodes for an existing show directory, a nested unpack directory,
/// a hidden directory, and an unrelated file.
fn library_tree() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("Library/Show Alpha/Show.Alpha.S01E00.mkv", "zero");
    tree.file_with("Library/Show.Alpha.S01E01.mkv", "one");
    tree.file_with("Library/Show.Alpha.S01E02.mkv", "two");
    tree.file_with("Library/Movie Night/subs/english.srt", "english");
    tree.file_with("Library/Movie Night/subs/.hidden.nfo", "hidden nfo");
    tree.file_with("Library/Movie Night/subs/forced/forced.srt", "forced");
    tree.file_with("Library/.hidden/Show.Alpha.S01E03.mkv", "hidden");
    tree.file_with("Library/Unrelated.File.mkv", "other");
    tree
}

/// Arguments for an automatic recursive run that also unpacks "subs" directories.
const ARGUMENTS: [&str; 4] = ["--auto", "--recurse", "--unpack", "subs"];

#[test]
fn dryrun_reports_the_moves_and_changes_nothing() {
    let tree = library_tree();
    let home = TempTree::new();
    let before = tree.snapshot();

    let output = run_dirmove(
        home.path(),
        &[&ARGUMENTS[..], &["--print"]].concat(),
        &tree.join("Library"),
    );

    assert_success(&output);
    assert_eq!(tree.snapshot(), before);
    let printed = stdout(&output);
    assert!(printed.contains("Unpacking: "), "{printed}");
    assert!(printed.contains("Move to: Show Alpha"), "{printed}");
    assert!(
        !printed.contains("S01E03"),
        "hidden files should not be offered:\n{printed}"
    );
}

#[test]
fn run_unpacks_and_moves_files_and_is_idempotent() {
    let tree = library_tree();
    let home = TempTree::new();
    let root = tree.join("Library");

    let first = run_dirmove(home.path(), &ARGUMENTS, &root);
    assert_success(&first);

    let expected = vec![
        "Library/",
        "Library/.hidden/",
        "Library/.hidden/Show.Alpha.S01E03.mkv: hidden",
        "Library/Movie Night/",
        "Library/Movie Night/.hidden.nfo: hidden nfo",
        "Library/Movie Night/english.srt: english",
        "Library/Movie Night/forced/",
        "Library/Movie Night/forced/forced.srt: forced",
        "Library/Show Alpha/",
        "Library/Show Alpha/Show.Alpha.S01E00.mkv: zero",
        "Library/Show Alpha/Show.Alpha.S01E01.mkv: one",
        "Library/Show Alpha/Show.Alpha.S01E02.mkv: two",
        "Library/Unrelated.File.mkv: other",
    ];
    assert_eq!(tree.snapshot(), expected);

    let second = run_dirmove(home.path(), &ARGUMENTS, &root);
    assert_success(&second);
    assert_eq!(tree.snapshot(), expected);
}

#[test]
fn an_existing_file_in_the_target_directory_is_kept_without_force() {
    let tree = library_tree();
    tree.file_with("Library/Show Alpha/Show.Alpha.S01E01.mkv", "already there");
    let home = TempTree::new();

    let output = run_dirmove(home.path(), &ARGUMENTS, &tree.join("Library"));

    assert_success(&output);
    assert_eq!(tree.read("Library/Show Alpha/Show.Alpha.S01E01.mkv"), "already there");
    assert_eq!(tree.read("Library/Show.Alpha.S01E01.mkv"), "one");
    assert_eq!(tree.read("Library/Show Alpha/Show.Alpha.S01E02.mkv"), "two");
}

#[test]
fn a_missing_path_is_an_error() {
    let tree = TempTree::new();
    let home = TempTree::new();

    let output = run_dirmove(home.path(), &ARGUMENTS, &tree.join("does not exist"));

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not exist"));
    assert_eq!(tree.files(), Vec::<String>::new());
}

#[test]
fn dryrun_does_not_create_the_name_database() {
    let tree = library_tree();
    let home = TempTree::new();

    let output = run_dirmove(
        home.path(),
        &[&ARGUMENTS[..], &["--print"]].concat(),
        &tree.join("Library"),
    );

    assert_success(&output);
    assert!(!database_exists(&home), "{:?}", home.files());
}

#[test]
fn database_command_reports_a_missing_database_without_creating_it() {
    let home = TempTree::new();

    let output = dirmove_command(home.path())
        .arg("database")
        .output()
        .expect("the dirmove binary should run");

    assert_success(&output);
    assert!(stdout(&output).contains("No database at"), "{}", stdout(&output));
    assert!(!database_exists(&home), "{:?}", home.files());
}

#[cfg(not(windows))]
#[test]
fn database_command_lists_the_directory_names_a_run_stored() {
    let tree = library_tree();
    let home = TempTree::new();
    assert_success(&run_dirmove(home.path(), &ARGUMENTS, &tree.join("Library")));
    assert!(database_exists(&home), "{:?}", home.files());

    let output = dirmove_command(home.path())
        .arg("db")
        .output()
        .expect("the dirmove binary should run");

    assert_success(&output);
    let printed = stdout(&output);
    assert!(printed.contains("Database entries: "), "{printed}");
    assert!(printed.contains("  Show Alpha"), "{printed}");
    assert!(printed.contains("  Movie Night"), "{printed}");
}
