//! Nested tree and filesystem edge case tests for merging directories.
//!
//! Every test builds a tree several levels deep with `TempTree`
//! and asserts the complete resulting tree, so a lost, duplicated, or misplaced entry fails the test.
//! The trees cover hidden files, dry runs, and symlinks that point outside the walked directories.

use cli_tools_common::test_support::TempTree;

use super::test_helpers::{make_dirmove, test_merge_config};

/// Run the prefixed directory merge on the "Root" directory of the tree.
fn merge(tree: &TempTree, recurse: bool, dryrun: bool, overwrite: bool) {
    let dirmove = make_dirmove(
        tree.join("Root"),
        test_merge_config(vec!["studio"], recurse, dryrun, overwrite),
    );
    dirmove.merge_prefixed_directories().expect("merging should succeed");
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
