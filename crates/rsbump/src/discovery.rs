//! Rust project discovery.
//!
//! Walks the input paths for `Cargo.toml` files and maps each one to its project root:
//! the closest ancestor workspace, or the package itself when no workspace contains it.
//! Also finds the Dockerfiles that belong to a project.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

use cli_tools_common::should_skip_entry;

use crate::dockerfile::is_dockerfile;
use crate::manifest::has_workspace_table;

/// Directory names never searched, since they hold build output or third party code.
const EXCLUDED_DIRECTORIES: [&str; 2] = ["target", "node_modules"];

/// Directory names skipped when searching for Dockerfiles,
/// since test data there is meant to stay as written.
const EXCLUDED_DOCKERFILE_DIRECTORIES: [&str; 5] = ["target", "node_modules", "fixtures", "testdata", "test-data"];

/// Search depth for Dockerfiles inside one project.
const DOCKERFILE_MAX_DEPTH: usize = 6;

/// Find the Rust project roots under the given paths.
///
/// A path inside a workspace member resolves to the workspace root.
/// The result is sorted and has no duplicates.
#[must_use]
pub fn find_projects(paths: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for path in paths {
        if path.is_file() {
            if path.file_name().is_some_and(|name| name == "Cargo.toml")
                && let Some(directory) = path.parent()
            {
                roots.insert(project_root(directory));
            }
            continue;
        }
        let manifests = WalkDir::new(path)
            .max_depth(max_depth)
            .into_iter()
            .filter_entry(|entry| !should_skip(entry, &EXCLUDED_DIRECTORIES))
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file() && entry.file_name() == "Cargo.toml");
        for manifest in manifests {
            if let Some(directory) = manifest.path().parent() {
                roots.insert(project_root(directory));
            }
        }
    }
    roots.into_iter().collect()
}

/// Find the Dockerfiles inside a project, skipping nested projects in `other_roots`.
#[must_use]
pub fn find_dockerfiles(root: &Path, other_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dockerfiles: Vec<PathBuf> = WalkDir::new(root)
        .max_depth(DOCKERFILE_MAX_DEPTH)
        .into_iter()
        .filter_entry(|entry| {
            !should_skip(entry, &EXCLUDED_DOCKERFILE_DIRECTORIES)
                && (entry.depth() == 0 || !other_roots.iter().any(|other| other.as_path() == entry.path()))
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && is_dockerfile(entry.path()))
        .map(DirEntry::into_path)
        .collect();
    dockerfiles.sort();
    dockerfiles
}

/// Find the member manifests of a workspace, without the root manifest and without nested project roots.
#[must_use]
pub fn find_member_manifests(root: &Path, other_roots: &[PathBuf]) -> Vec<PathBuf> {
    let root_manifest = root.join("Cargo.toml");
    let mut manifests: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !should_skip(entry, &EXCLUDED_DIRECTORIES)
                && (entry.depth() == 0 || !other_roots.iter().any(|other| other.as_path() == entry.path()))
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "Cargo.toml")
        .map(DirEntry::into_path)
        .filter(|path| *path != root_manifest)
        .collect();
    manifests.sort();
    manifests
}

/// Map a directory holding a `Cargo.toml` to its project root.
///
/// The directory is its own root when its manifest defines a workspace.
/// Otherwise the closest ancestor with a workspace manifest is the root,
/// and with no such ancestor the directory is a standalone package.
fn project_root(directory: &Path) -> PathBuf {
    if is_workspace_manifest(&directory.join("Cargo.toml")) {
        return directory.to_path_buf();
    }
    directory
        .ancestors()
        .skip(1)
        .find(|ancestor| is_workspace_manifest(&ancestor.join("Cargo.toml")))
        .unwrap_or(directory)
        .to_path_buf()
}

/// Check if the manifest at the given path exists and defines a workspace.
fn is_workspace_manifest(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|content| has_workspace_table(&content))
}

/// Check if a walk entry is hidden, a system directory, or one of the excluded directory names.
fn should_skip(entry: &DirEntry, excluded: &[&str]) -> bool {
    should_skip_entry(entry)
        || (entry.depth() > 0
            && entry.file_type().is_dir()
            && entry.file_name().to_str().is_some_and(|name| excluded.contains(&name)))
}

#[cfg(test)]
mod test_find_projects {
    use super::*;

    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("has parent")).expect("create directories");
        fs::write(path, content).expect("write file");
    }

    fn canonical_temp_dir() -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path().canonicalize().expect("canonical temp dir");
        (temp, root)
    }

    #[test]
    fn member_manifests_skip_the_root_build_output_and_nested_projects() {
        let (_temp, root) = canonical_temp_dir();
        let workspace = root.join("ws");
        write(&root, "ws/Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(&root, "ws/crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(&root, "ws/crates/b/Cargo.toml", "[package]\nname = \"b\"\n");
        write(&root, "ws/target/package/c/Cargo.toml", "[package]\nname = \"c\"\n");
        write(&root, "ws/nested/Cargo.toml", "[workspace]\n");
        write(&root, "ws/nested/d/Cargo.toml", "[package]\nname = \"d\"\n");

        let members = find_member_manifests(&workspace, &[workspace.clone(), workspace.join("nested")]);

        assert_eq!(
            members,
            vec![
                workspace.join("crates/a/Cargo.toml"),
                workspace.join("crates/b/Cargo.toml")
            ]
        );
    }

    #[test]
    fn workspace_members_map_to_root() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "ws/Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(&root, "ws/crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(&root, "ws/crates/b/Cargo.toml", "[package]\nname = \"b\"\n");

        assert_eq!(find_projects(std::slice::from_ref(&root), 5), vec![root.join("ws")]);
    }

    #[test]
    fn nested_workspace_is_separate_root() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "ws/Cargo.toml", "[workspace]\nmembers = [\"rust/crates/*\"]\n");
        write(&root, "ws/rust/crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(
            &root,
            "ws/rust/fuzz/Cargo.toml",
            "[package]\nname = \"fuzz\"\n\n[workspace]\nmembers = [\".\"]\n",
        );

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 5),
            vec![root.join("ws"), root.join("ws/rust/fuzz")]
        );
    }

    #[test]
    fn package_under_package_is_separate_root() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "app/Cargo.toml", "[package]\nname = \"app\"\n");
        write(&root, "app/office/docker/Cargo.toml", "[package]\nname = \"docker\"\n");

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 5),
            vec![root.join("app"), root.join("app/office/docker")]
        );
    }

    #[test]
    fn skips_target_node_modules_and_hidden() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "app/Cargo.toml", "[package]\nname = \"app\"\n");
        write(
            &root,
            "app/target/package/copy/Cargo.toml",
            "[package]\nname = \"copy\"\n",
        );
        write(&root, "web/node_modules/dep/Cargo.toml", "[package]\nname = \"dep\"\n");
        write(&root, ".hidden/Cargo.toml", "[package]\nname = \"hidden\"\n");

        assert_eq!(find_projects(std::slice::from_ref(&root), 5), vec![root.join("app")]);
    }

    #[test]
    fn member_path_resolves_to_workspace_root() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "ws/Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(&root, "ws/crates/a/Cargo.toml", "[package]\nname = \"a\"\n");

        assert_eq!(find_projects(&[root.join("ws/crates/a")], 5), vec![root.join("ws")]);
    }

    #[test]
    fn respects_max_depth() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "a/b/c/Cargo.toml", "[package]\nname = \"deep\"\n");

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 3),
            [] as [std::path::PathBuf; 0]
        );
        assert_eq!(find_projects(std::slice::from_ref(&root), 4), vec![root.join("a/b/c")]);
    }

    #[test]
    fn manifest_file_path_is_accepted() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "app/Cargo.toml", "[package]\nname = \"app\"\n");

        assert_eq!(find_projects(&[root.join("app/Cargo.toml")], 5), vec![root.join("app")]);
    }
}

#[cfg(test)]
mod test_find_dockerfiles {
    use super::*;

    use tempfile::TempDir;

    fn write(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("has parent")).expect("create directories");
        fs::write(path, "FROM rust:1\n").expect("write file");
    }

    #[test]
    fn finds_dockerfiles_and_skips_nested_projects() {
        let temp = TempDir::new().expect("temp dir");
        let root = temp.path().to_path_buf();
        write(&root, "Dockerfile");
        write(&root, "infra/backend.Dockerfile");
        write(&root, "target/Dockerfile");
        write(&root, "tests/fixtures/app.dockerfile");
        write(&root, "nested/Dockerfile");

        let dockerfiles = find_dockerfiles(&root, &[root.join("nested")]);
        assert_eq!(
            dockerfiles,
            vec![root.join("Dockerfile"), root.join("infra/backend.Dockerfile")]
        );
    }
}
