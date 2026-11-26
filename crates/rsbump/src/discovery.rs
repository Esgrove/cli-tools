//! Rust project discovery.
//!
//! Walks the input paths for `Cargo.toml` files and uses Cargo's workspace membership to group them.
//! Also finds the Dockerfiles that belong to a project.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item};
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

/// The parts of Cargo metadata that identify the workspace and its members.
#[derive(Deserialize)]
struct CargoWorkspace {
    workspace_root: PathBuf,
    workspace_members: BTreeSet<String>,
    packages: Vec<CargoPackage>,
}

/// A package listed by Cargo metadata.
#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    manifest_path: PathBuf,
}

impl CargoWorkspace {
    /// Run Cargo offline without resolving dependencies to get the actual members.
    fn read(root: &Path) -> Result<Self> {
        let output = Command::new("cargo")
            .args(["metadata", "--offline", "--no-deps", "--format-version", "1"])
            .current_dir(root)
            .output()
            .with_context(|| format!("Failed to run Cargo metadata in {}", root.display()))?;
        if !output.status.success() {
            anyhow::bail!(
                "Cargo metadata failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout).with_context(|| format!("Invalid Cargo metadata in {}", root.display()))
    }

    /// Return the member manifests, without the root manifest.
    fn member_manifests(self) -> BTreeSet<PathBuf> {
        let root_manifest = self.workspace_root.join("Cargo.toml");
        self.packages
            .into_iter()
            .filter(|package| package.manifest_path != root_manifest && self.workspace_members.contains(&package.id))
            .map(|package| package.manifest_path)
            .collect()
    }
}

/// Find the Rust project roots under the given paths.
/// A workspace member resolves to its Cargo workspace root.
/// The result is sorted and has no duplicates.
///
/// # Errors
/// Returns an error if a manifest cannot be read or workspace membership cannot be determined.
pub fn find_projects(paths: &[PathBuf], max_depth: usize) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    let mut workspaces = BTreeMap::new();
    for path in paths {
        if path.is_file() {
            if path.file_name().is_some_and(|name| name == "Cargo.toml") {
                roots.insert(project_root(path, &mut workspaces)?);
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
            match project_root(manifest.path(), &mut workspaces) {
                Ok(root) => {
                    roots.insert(root);
                }
                Err(error) => {
                    eprintln!("Warning: skipping {}: {error:#}", manifest.path().display());
                }
            }
        }
    }
    Ok(roots.into_iter().collect())
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

/// Find the member manifests Cargo reports for a project, without the root manifest.
///
/// # Errors
/// Returns an error when Cargo cannot determine membership.
pub fn find_member_manifests(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(CargoWorkspace::read(root)?.member_manifests().into_iter().collect())
}

/// Find the nearest independent project roots beneath this project.
/// A scope boundary has a manifest of its own but is not a Cargo member.
pub(super) fn find_nested_projects(root: &Path, members: &[PathBuf]) -> Vec<PathBuf> {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut nested = Vec::new();
    let mut entries = WalkDir::new(&canonical_root).into_iter();
    while let Some(entry) = entries.next() {
        let Ok(entry) = entry else { continue };
        if should_skip(&entry, &EXCLUDED_DIRECTORIES) {
            if entry.file_type().is_dir() {
                entries.skip_current_dir();
            }
            continue;
        }
        if entry.depth() > 0 && entry.file_type().is_dir() {
            let manifest = entry.path().join("Cargo.toml");
            if manifest.is_file() && members.binary_search(&manifest).is_err() {
                nested.push(entry.into_path());
                entries.skip_current_dir();
            }
        }
    }
    nested
}

/// Map a manifest to its workspace only if Cargo reports it as a member.
fn project_root(manifest: &Path, workspaces: &mut BTreeMap<PathBuf, BTreeSet<PathBuf>>) -> Result<PathBuf> {
    let manifest = cli_tools_common::resolve_input_path(Some(manifest))?;
    let directory = manifest.parent().context("Manifest has no parent directory")?;
    let content = fs::read_to_string(&manifest).with_context(|| format!("Failed to read {}", manifest.display()))?;
    let document: DocumentMut = content
        .parse()
        .with_context(|| format!("Failed to parse {}", manifest.display()))?;
    if document.contains_key("workspace") {
        return Ok(directory.to_path_buf());
    }
    let explicit = document
        .get("package")
        .and_then(|package| package.get("workspace"))
        .and_then(Item::as_str);
    let candidate = if let Some(path) = explicit {
        Some(cli_tools_common::resolve_input_path(Some(&directory.join(path)))?)
    } else {
        directory
            .ancestors()
            .skip(1)
            .find(|ancestor| is_workspace_manifest(&ancestor.join("Cargo.toml")))
            .map(Path::to_path_buf)
    };
    let Some(candidate) = candidate else {
        return Ok(directory.to_path_buf());
    };
    if !workspaces.contains_key(&candidate) {
        workspaces.insert(candidate.clone(), CargoWorkspace::read(&candidate)?.member_manifests());
    }
    if workspaces
        .get(&candidate)
        .is_some_and(|members| members.contains(&manifest))
    {
        Ok(candidate)
    } else {
        Ok(directory.to_path_buf())
    }
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
        fs::write(&path, content).expect("write file");
        if content.starts_with("[package]") {
            let source = path.parent().unwrap().join("src");
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("lib.rs"), "").unwrap();
        }
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

        let members = find_member_manifests(&workspace).unwrap();

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

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 5).unwrap(),
            vec![root.join("ws")]
        );
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
            find_projects(std::slice::from_ref(&root), 5).unwrap(),
            vec![root.join("ws"), root.join("ws/rust/fuzz")]
        );
    }

    #[test]
    fn package_under_package_is_separate_root() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "app/Cargo.toml", "[package]\nname = \"app\"\n");
        write(&root, "app/office/docker/Cargo.toml", "[package]\nname = \"docker\"\n");

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 5).unwrap(),
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

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 5).unwrap(),
            vec![root.join("app")]
        );
    }

    #[test]
    fn member_path_resolves_to_workspace_root() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "ws/Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(&root, "ws/crates/a/Cargo.toml", "[package]\nname = \"a\"\n");

        assert_eq!(
            find_projects(&[root.join("ws/crates/a")], 5).unwrap(),
            vec![root.join("ws")]
        );
    }

    #[test]
    fn cargo_membership_keeps_excluded_packages_separate() {
        let tree = cli_tools_common::test_support::TempTree::new();
        let root = tree.path().canonicalize().unwrap();
        tree.file_with(
            "ws/Cargo.toml",
            "[workspace]\nmembers = ['app']\nexclude = ['legacy']\n",
        );
        for name in ["app", "legacy", "unlisted"] {
            tree.file_with(
                &format!("ws/{name}/Cargo.toml"),
                format!("[package]\nname = '{name}'\nversion = '0.1.0'\n"),
            );
            tree.file(&format!("ws/{name}/src/lib.rs"));
        }
        let workspace = root.join("ws");
        let legacy = workspace.join("legacy");
        assert_eq!(find_projects(&[legacy.join("Cargo.toml")], 5).unwrap(), [legacy]);
        assert_eq!(
            find_member_manifests(&workspace).unwrap(),
            [workspace.join("app/Cargo.toml")]
        );
    }

    #[test]
    fn cargo_membership_honors_an_explicit_workspace_over_the_nearest_one() {
        let tree = cli_tools_common::test_support::TempTree::new();
        let workspace = tree.path().canonicalize().unwrap();
        tree.file_with("Cargo.toml", "[workspace]\nmembers = ['nested/app']\n");
        tree.file_with("nested/Cargo.toml", "[workspace]\nmembers = []\n");
        tree.file_with(
            "nested/app/Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.1.0'\nworkspace = '../..'\n",
        );
        tree.file("nested/app/src/lib.rs");
        assert_eq!(
            find_projects(&[workspace.join("nested/app/Cargo.toml")], 5).unwrap(),
            std::slice::from_ref(&workspace)
        );
        assert_eq!(
            find_member_manifests(&workspace).unwrap(),
            [workspace.join("nested/app/Cargo.toml")]
        );
    }

    #[test]
    fn respects_max_depth() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "a/b/c/Cargo.toml", "[package]\nname = \"deep\"\n");

        assert_eq!(
            find_projects(std::slice::from_ref(&root), 3).unwrap(),
            [] as [std::path::PathBuf; 0]
        );
        assert_eq!(
            find_projects(std::slice::from_ref(&root), 4).unwrap(),
            vec![root.join("a/b/c")]
        );
    }

    #[test]
    fn manifest_file_path_is_accepted() {
        let (_temp, root) = canonical_temp_dir();
        write(&root, "app/Cargo.toml", "[package]\nname = \"app\"\n");

        assert_eq!(
            find_projects(&[root.join("app/Cargo.toml")], 5).unwrap(),
            vec![root.join("app")]
        );
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
