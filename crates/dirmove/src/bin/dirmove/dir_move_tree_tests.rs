//! Nested tree and filesystem edge case tests for unpacking and merging directories.
//!
//! Every test builds a tree several levels deep with `TempTree`
//! and asserts the complete resulting tree, so a lost, duplicated, or misplaced entry fails the test.
//! The trees cover hidden files, dry runs, and symlinks that point outside the walked directories.

use cli_tools_common::test_support::TempTree;

use super::test_helpers::make_dirmove;
use super::*;

/// Tree with unpack directories at several depths, hidden files inside them, and an unrelated empty directory.
fn unpack_tree() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("Library/Show A/Season 1/episode.mkv", "episode");
    tree.file_with("Library/Show A/Season 1/subs/english.srt", "english");
    tree.file_with("Library/Show A/Season 1/subs/.hidden.nfo", "hidden");
    tree.file_with("Library/Show A/Season 1/subs/languages/.deep hidden.srt", "deep hidden");
    tree.file_with("Library/Show A/Season 1/subs/languages/finnish.srt", "finnish");
    tree.file_with("Library/Show B/extras/sample/clip.mkv", "clip");
    tree.file_with("Library/Show B/extras/sample/sample/inner clip.mkv", "inner clip");
    tree.dir("Library/Show B/unrelated empty");
    tree
}

/// Run the unpack pass on the "Library" directory of the tree.
fn unpack(tree: &TempTree, recurse: bool, dryrun: bool) {
    let dirmove = make_dirmove(
        tree.join("Library"),
        Config::test_unpack(vec!["subs", "sample"], recurse, dryrun, false),
    );
    dirmove.unpack_directories().expect("unpacking should succeed");
}

/// Run the prefixed directory merge on the "Root" directory of the tree.
fn merge(tree: &TempTree, recurse: bool, dryrun: bool, overwrite: bool) {
    let dirmove = make_dirmove(
        tree.join("Root"),
        Config::test_merge(vec!["studio"], recurse, dryrun, overwrite),
    );
    dirmove.merge_prefixed_directories().expect("merging should succeed");
}

#[test]
fn recursive_unpack_moves_every_file_including_hidden_ones() {
    let tree = unpack_tree();

    unpack(&tree, true, false);

    assert_eq!(
        tree.snapshot(),
        vec![
            "Library/",
            "Library/Show A/",
            "Library/Show A/Season 1/",
            "Library/Show A/Season 1/.hidden.nfo: hidden",
            "Library/Show A/Season 1/english.srt: english",
            "Library/Show A/Season 1/episode.mkv: episode",
            "Library/Show A/Season 1/languages/",
            "Library/Show A/Season 1/languages/.deep hidden.srt: deep hidden",
            "Library/Show A/Season 1/languages/finnish.srt: finnish",
            "Library/Show B/",
            "Library/Show B/extras/",
            "Library/Show B/extras/clip.mkv: clip",
            "Library/Show B/extras/inner clip.mkv: inner clip",
            "Library/Show B/unrelated empty/",
        ]
    );
}

#[test]
fn unpack_is_idempotent() {
    let tree = unpack_tree();
    unpack(&tree, true, false);
    let after_first_run = tree.snapshot();

    unpack(&tree, true, false);

    assert_eq!(tree.snapshot(), after_first_run);
}

#[test]
fn non_recursive_unpack_leaves_nested_unpack_directories_alone() {
    let tree = unpack_tree();
    let before = tree.snapshot();

    unpack(&tree, false, false);

    assert_eq!(tree.snapshot(), before);
}

#[test]
fn dryrun_unpack_on_a_deep_tree_changes_nothing() {
    let tree = unpack_tree();
    let before = tree.snapshot();

    unpack(&tree, true, true);

    assert_eq!(tree.snapshot(), before);
}

/// Tree with a prefixed directory holding hidden files and nested directories to merge into its unprefixed match.
fn merge_tree() -> TempTree {
    let tree = TempTree::new();
    tree.file_with("Root/studio alpha beta/file one.mkv", "one");
    tree.file_with("Root/studio alpha beta/.hidden", "hidden");
    tree.file_with("Root/studio alpha beta/extras/deep/.deep hidden", "deep hidden");
    tree.file_with("Root/studio alpha beta/extras/deep/clip.mkv", "clip");
    tree.file_with("Root/alpha beta/existing.mkv", "existing");
    tree.file_with("Root/alpha beta/extras/existing extra.mkv", "existing extra");
    tree
}

#[test]
fn merge_moves_hidden_files_and_keeps_an_existing_subdirectory_without_overwrite() {
    let tree = merge_tree();

    merge(&tree, false, false, false);

    assert_eq!(
        tree.snapshot(),
        vec![
            "Root/",
            "Root/alpha beta/",
            "Root/alpha beta/.hidden: hidden",
            "Root/alpha beta/existing.mkv: existing",
            "Root/alpha beta/extras/",
            "Root/alpha beta/extras/existing extra.mkv: existing extra",
            "Root/alpha beta/file one.mkv: one",
            "Root/studio alpha beta/",
            "Root/studio alpha beta/extras/",
            "Root/studio alpha beta/extras/deep/",
            "Root/studio alpha beta/extras/deep/.deep hidden: deep hidden",
            "Root/studio alpha beta/extras/deep/clip.mkv: clip",
        ]
    );
}

#[test]
fn overwrite_merges_nested_directories_and_hidden_files_at_every_depth() {
    let tree = merge_tree();

    merge(&tree, false, false, true);

    assert_eq!(
        tree.snapshot(),
        vec![
            "Root/",
            "Root/alpha beta/",
            "Root/alpha beta/.hidden: hidden",
            "Root/alpha beta/existing.mkv: existing",
            "Root/alpha beta/extras/",
            "Root/alpha beta/extras/deep/",
            "Root/alpha beta/extras/deep/.deep hidden: deep hidden",
            "Root/alpha beta/extras/deep/clip.mkv: clip",
            "Root/alpha beta/extras/existing extra.mkv: existing extra",
            "Root/alpha beta/file one.mkv: one",
        ]
    );
}

#[test]
fn dryrun_merge_on_a_deep_tree_changes_nothing() {
    let tree = merge_tree();
    let before = tree.snapshot();

    merge(&tree, true, true, false);

    assert_eq!(tree.snapshot(), before);
}

#[test]
fn merge_conflicts_keep_both_versions() {
    let tree = merge_tree();
    tree.file_with("Root/studio alpha beta/existing.mkv", "incoming");

    merge(&tree, false, false, false);

    assert_eq!(tree.read("Root/alpha beta/existing.mkv"), "existing");
    assert_eq!(tree.read("Root/studio alpha beta/existing.mkv"), "incoming");
}

#[cfg(unix)]
#[test]
fn unpack_does_not_pull_files_out_of_a_linked_unpack_directory() {
    let tree = TempTree::new();
    tree.file_with("outside/real/data.txt", "outside data");
    tree.file_with("Library/Movie/subs/english.srt", "english");
    tree.symlink("outside/real", "Library/Movie/subs/sample");

    unpack(&tree, true, false);

    assert_eq!(tree.read("outside/real/data.txt"), "outside data");
    assert!(!tree.exists("Library/Movie/data.txt"));
    assert_eq!(tree.read("Library/Movie/english.srt"), "english");
}

#[cfg(unix)]
#[test]
fn unpack_moves_a_linked_directory_as_a_link() {
    let tree = TempTree::new();
    tree.file_with("outside/real/data.txt", "outside data");
    tree.file_with("Library/Movie/subs/english.srt", "english");
    tree.symlink("outside/real", "Library/Movie/subs/linked");

    unpack(&tree, true, false);

    assert_eq!(tree.read("outside/real/data.txt"), "outside data");
    let link = tree
        .join("Library/Movie/linked")
        .symlink_metadata()
        .expect("the link should be moved next to the unpacked files");
    assert!(link.file_type().is_symlink());
    assert!(!tree.exists("Library/Movie/subs"));
}

#[cfg(unix)]
#[test]
fn pruning_keeps_a_directory_that_only_holds_a_symlink() {
    let tree = TempTree::new();
    tree.file("outside/target.txt");
    tree.file_with("Library/Movie/subs/english.srt", "english");
    tree.symlink("outside/target.txt", "Library/Movie/subs/keeper/link.txt");

    unpack(&tree, true, false);

    assert!(tree.exists("outside/target.txt"));
    assert!(
        tree.join("Library/Movie/keeper/link.txt")
            .symlink_metadata()
            .expect("the link should survive")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn prune_does_not_follow_a_symlink_to_an_empty_directory() {
    let tree = TempTree::new();
    tree.dir("outside/empty target");
    tree.dir("Library/Movie/subs/touched");
    tree.symlink("outside/empty target", "Library/Movie/subs/touched/link");
    let dirmove = make_dirmove(
        tree.join("Library"),
        Config::test_unpack(vec!["subs"], true, false, false),
    );
    let mut touched_dirs: HashSet<PathBuf> = HashSet::from([
        tree.join("Library/Movie/subs/touched"),
        tree.join("Library/Movie/subs/touched/link"),
        tree.join("outside/empty target"),
    ]);

    dirmove
        .prune_empty_dirs_under(&tree.join("Library/Movie/subs"), &mut touched_dirs)
        .expect("pruning should succeed");

    assert!(tree.exists("outside/empty target"));
    assert!(tree.exists("Library/Movie/subs/touched/link"));
}

#[cfg(unix)]
#[test]
fn merge_does_not_pull_files_out_of_a_linked_subdirectory() {
    let tree = TempTree::new();
    tree.file_with("outside/real/data.txt", "outside data");
    tree.file_with("Root/studio alpha beta/file one.mkv", "one");
    tree.symlink("outside/real", "Root/studio alpha beta/extras");
    tree.file_with("Root/alpha beta/extras/existing extra.mkv", "existing extra");

    merge(&tree, false, false, false);

    assert_eq!(tree.read("outside/real/data.txt"), "outside data");
    assert!(!tree.exists("Root/alpha beta/extras/data.txt"));
    assert_eq!(tree.read("Root/alpha beta/file one.mkv"), "one");
}

#[cfg(unix)]
#[test]
fn overwrite_merge_does_not_pull_files_out_of_a_linked_subdirectory() {
    let tree = TempTree::new();
    tree.file_with("outside/real/data.txt", "outside data");
    tree.file_with("Root/studio alpha beta/file one.mkv", "one");
    tree.symlink("outside/real", "Root/studio alpha beta/extras");
    tree.file_with("Root/alpha beta/extras/existing extra.mkv", "existing extra");

    merge(&tree, false, false, true);

    assert_eq!(tree.read("outside/real/data.txt"), "outside data");
    assert!(!tree.exists("Root/alpha beta/extras/data.txt"));
    assert_eq!(tree.read("Root/alpha beta/extras/existing extra.mkv"), "existing extra");
    assert_eq!(tree.read("Root/alpha beta/file one.mkv"), "one");
}

#[cfg(unix)]
#[test]
fn merge_moves_a_linked_subdirectory_as_a_link() {
    let tree = TempTree::new();
    tree.file_with("outside/real/data.txt", "outside data");
    tree.file_with("Root/studio alpha beta/file one.mkv", "one");
    tree.symlink("outside/real", "Root/studio alpha beta/extras");
    tree.dir("Root/alpha beta");

    merge(&tree, false, false, false);

    assert_eq!(tree.read("outside/real/data.txt"), "outside data");
    let link = tree
        .join("Root/alpha beta/extras")
        .symlink_metadata()
        .expect("the link should be moved into the merge target");
    assert!(link.file_type().is_symlink());
    assert!(!tree.exists("Root/studio alpha beta"));
}
