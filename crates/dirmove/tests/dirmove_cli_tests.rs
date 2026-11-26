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
fn preview_offers_a_file_to_the_next_match_after_a_collision() {
    let tree = TempTree::with_entries(&["Library/Show/"]);
    tree.file_with("Library/Show.Alpha.S01E01.mkv", "incoming");
    tree.file_with("Library/Show Alpha/Show.Alpha.S01E01.mkv", "existing");
    let home = TempTree::new();
    let before = tree.snapshot();
    let root = tree.join("Library");

    let preview = run_dirmove(home.path(), &["--print", "--files-only"], &root);

    assert_success(&preview);
    let printed = stdout(&preview);
    assert!(printed.lines().any(|line| line.ends_with("Move to: Show")), "{printed}");
    assert!(String::from_utf8_lossy(&preview.stderr).contains("Skipping existing file:"));
    assert_eq!(tree.snapshot(), before);

    let run = run_dirmove(home.path(), &["--auto", "--files-only"], &root);

    assert_success(&run);
    assert_eq!(tree.read("Library/Show/Show.Alpha.S01E01.mkv"), "incoming");
    assert_eq!(tree.read("Library/Show Alpha/Show.Alpha.S01E01.mkv"), "existing");
}

#[test]
fn unpack_preview_excludes_an_occupied_destination() {
    let tree = TempTree::new();
    tree.file_with("Library/Film/english.srt", "existing");
    tree.file_with("Library/Film/subs/english.srt", "blocked");
    tree.file_with("Library/Film/subs/other.srt", "incoming");
    let home = TempTree::new();
    let before = tree.snapshot();
    let root = tree.join("Library");

    let preview = run_dirmove(
        home.path(),
        &[&ARGUMENTS[..], &["--print", "--verbose"]].concat(),
        &root,
    );

    assert_success(&preview);
    let printed = stdout(&preview).replace('\\', "/");
    assert!(printed.contains("subs/other.srt -> other.srt"), "{printed}");
    assert!(!printed.contains("subs/english.srt -> english.srt"), "{printed}");
    assert!(String::from_utf8_lossy(&preview.stderr).contains("Skipping existing file:"));
    assert_eq!(tree.snapshot(), before);

    let run = run_dirmove(home.path(), &ARGUMENTS, &root);

    assert_success(&run);
    assert_eq!(tree.read("Library/Film/other.srt"), "incoming");
    assert_eq!(tree.read("Library/Film/english.srt"), "existing");
    assert_eq!(tree.read("Library/Film/subs/english.srt"), "blocked");
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

#[test]
fn create_mode_groups_loose_files_into_a_new_directory() {
    let tree = TempTree::new();
    tree.file_with("Inbox/Show.Name.S01E01.mkv", "one");
    tree.file_with("Inbox/Show.Name.S01E02.mkv", "two");
    tree.file_with("Inbox/Show.Name.S01E03.mkv", "three");
    tree.file_with("Inbox/Unrelated.mkv", "other");
    let home = TempTree::new();

    let output = run_dirmove(home.path(), &["--auto", "--create"], &tree.join("Inbox"));

    assert_success(&output);
    assert_eq!(
        tree.snapshot(),
        vec![
            "Inbox/",
            "Inbox/Show Name/",
            "Inbox/Show Name/Show.Name.S01E01.mkv: one",
            "Inbox/Show Name/Show.Name.S01E02.mkv: two",
            "Inbox/Show Name/Show.Name.S01E03.mkv: three",
            "Inbox/Unrelated.mkv: other",
        ]
    );
}

#[test]
fn files_move_to_the_output_root_that_has_the_matching_directory() {
    let tree = TempTree::new();
    tree.file_with("Inbox/Show.Alpha.S01E01.mkv", "one");
    tree.dir("First Library/Other Show");
    tree.dir("Second Library/Show Alpha");
    let home = TempTree::new();

    let output = run_dirmove(
        home.path(),
        &[
            "--auto",
            "--output",
            &tree.join("First Library").to_string_lossy(),
            "--output",
            &tree.join("Second Library").to_string_lossy(),
        ],
        &tree.join("Inbox"),
    );

    assert_success(&output);
    assert_eq!(tree.read("Second Library/Show Alpha/Show.Alpha.S01E01.mkv"), "one");
    assert!(!tree.exists("Inbox/Show.Alpha.S01E01.mkv"));
}

#[test]
fn force_merge_leaves_later_episode_in_its_source() {
    let tree = TempTree::new();
    tree.file_with("First/Show/episode.mkv", "first episode");
    tree.file_with("Second/Show/episode.mkv", "second episode");
    tree.file_with("First/Show/replace.mkv", "replacement");
    tree.file_with("First/Show/Season/episode.mkv", "first nested episode");
    tree.file_with("Second/Show/Season/episode.mkv", "second nested episode");
    tree.file_with("Second/Show/Season/bonus.mkv", "nested bonus");
    tree.file_with("Library/Show/replace.mkv", "pre-existing destination");
    let home = TempTree::new();

    let output = dirmove_command(home.path())
        .args(["--auto", "--force", "--output"])
        .arg(tree.join("Library"))
        .args([tree.join("First"), tree.join("Second")])
        .output()
        .unwrap();

    assert_success(&output);
    assert!(stdout(&output).contains("2 directories"), "{}", stdout(&output));
    assert_eq!(tree.read("Library/Show/episode.mkv"), "first episode");
    assert_eq!(tree.read("Second/Show/episode.mkv"), "second episode");
    assert_eq!(tree.read("Library/Show/replace.mkv"), "replacement");
    assert_eq!(tree.read("Library/Show/Season/episode.mkv"), "first nested episode");
    assert_eq!(tree.read("Second/Show/Season/episode.mkv"), "second nested episode");
    assert_eq!(tree.read("Library/Show/Season/bonus.mkv"), "nested bonus");
    assert!(!tree.exists("Second/Show/Season/bonus.mkv"));
    assert!(!tree.exists("First/Show"));
    let warnings = String::from_utf8_lossy(&output.stderr);
    assert!(
        warnings.contains("another entry in this merge has the same destination"),
        "{warnings}"
    );
}

#[test]
fn files_only_leaves_matching_input_directories_in_place() {
    let tree = TempTree::new();
    tree.file_with("Inbox/Show Alpha/Show.Alpha.S01E01.mkv", "nested");
    tree.file_with("Inbox/Show.Alpha.S01E02.mkv", "loose");
    tree.dir("Library/Show Alpha");
    let home = TempTree::new();
    let output_argument = tree.join("Library").to_string_lossy().into_owned();

    let output = run_dirmove(
        home.path(),
        &["--auto", "--files-only", "--output", &output_argument],
        &tree.join("Inbox"),
    );

    assert_success(&output);
    assert_eq!(tree.read("Inbox/Show Alpha/Show.Alpha.S01E01.mkv"), "nested");
    assert_eq!(tree.read("Library/Show Alpha/Show.Alpha.S01E02.mkv"), "loose");

    let without_files_only = run_dirmove(
        home.path(),
        &["--auto", "--output", &output_argument],
        &tree.join("Inbox"),
    );

    assert_success(&without_files_only);
    assert_eq!(tree.read("Library/Show Alpha/Show.Alpha.S01E01.mkv"), "nested");
    assert!(!tree.exists("Inbox/Show Alpha"));
}
