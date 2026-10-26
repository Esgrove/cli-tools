//! `Git` version tag creation for `CMake`, Python, and Rust projects.
//!
//! Detects project manifests, parses versions and names, and creates or pushes `Git` tags.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use colored::Colorize;
use git2::{Oid, Repository};
use regex::Regex;

/// Matches VERSION number inside a `CMake` `project()` declaration.
/// Supports 2-4 component versions: `1.0`, `1.0.0`, and `1.0.0.0`.
static CMAKE_VERSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)project\s*\([^)]*VERSION\s+([0-9]+\.[0-9]+(?:\.[0-9]+){0,2})").expect("Invalid regex")
});

/// Matches the project name (e.g. `project("AudioBatch" ...)` or `project(AudioBatch ...)`).
static CMAKE_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)project\s*\(\s*["']?([^"'\s)]+)["']?"#).expect("Invalid regex"));

#[derive(Parser, Debug)]
#[command(author, version, name = env!("CARGO_BIN_NAME"), about = "Create git version tags for a project (CMake, Python, Rust)")]
struct Args {
    #[command(subcommand)]
    command: Option<VersionTagCommand>,

    /// Optional project directory in a git repository, such as a subdirectory. Defaults to current directory.
    #[arg(value_hint = clap::ValueHint::AnyPath)]
    path: Option<PathBuf>,

    /// Only print information without creating or pushing tags
    #[arg(short, long)]
    dryrun: bool,

    /// Push tags to remote
    #[arg(short, long)]
    push: bool,

    /// Only push new tags that did not exist locally
    #[arg(short, long)]
    new: bool,

    /// Use a single push to push all tags
    #[arg(short, long)]
    single: bool,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Prefix for the tag names, for example "rust-" for tags like rust-v1.2.3
    #[arg(short = 't', long, value_name = "PREFIX", default_value = "")]
    tag_prefix: String,
}

/// Subcommands for vtag.
#[derive(Subcommand, Debug)]
enum VersionTagCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion {
        /// Shell to generate completion for
        #[arg(value_enum)]
        shell: Shell,

        /// Install completion script to the shell's completion directory
        #[arg(short = 'I', long)]
        install: bool,
    },
}

/// Supported project types for version tagging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectType {
    CMake,
    Python,
    Rust,
}

impl ProjectType {
    /// The manifest filename for this project type.
    const fn manifest_filename(self) -> &'static str {
        match self {
            Self::CMake => "CMakeLists.txt",
            Self::Python => "pyproject.toml",
            Self::Rust => "Cargo.toml",
        }
    }
}

impl fmt::Display for ProjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CMake => write!(f, "CMake"),
            Self::Python => write!(f, "Python"),
            Self::Rust => write!(f, "Rust"),
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(VersionTagCommand::Completion { shell, install }) = &args.command {
        return cli_tools::generate_shell_completion(
            *shell,
            Args::command(),
            *install,
            args.verbose,
            env!("CARGO_BIN_NAME"),
        );
    }
    let repo_path = cli_tools::resolve_input_path(args.path.as_deref())?;
    if !repo_path.is_dir() {
        anyhow::bail!("Input path needs to be a directory in a git repository")
    }
    version_tag(
        &repo_path,
        args.push,
        args.dryrun,
        args.verbose,
        args.single,
        args.new,
        &args.tag_prefix,
    )
}

/// Create version tags for each unique package version from the project's git history.
///
/// The project may live in a subdirectory of the repository, such as `rust/` in a repository with several projects.
/// Its manifest is then read from that subdirectory in each commit,
/// and commits made before the manifest first appeared there use the manifest at the repository root,
/// so the history of a project that was moved into a subdirectory is still tagged.
/// Tag names are the version with a `v` and the given prefix in front, for example `rust-v1.2.3`.
fn version_tag(
    repo_path: &PathBuf,
    push: bool,
    dryrun: bool,
    verbose: bool,
    combined_push: bool,
    new_tags_only: bool,
    tag_prefix: &str,
) -> Result<()> {
    let project_type = detect_project_type(repo_path)?;
    let manifest_filename = project_type.manifest_filename();

    if verbose {
        let name =
            get_project_name(repo_path, project_type).unwrap_or_else(|| cli_tools::path_to_string_relative(repo_path));
        println!(
            "{}",
            format!("Creating version tags for {name} ({project_type})")
                .magenta()
                .bold()
        );
    }

    let repo = Repository::discover(repo_path)?;
    let manifest_path = manifest_path_in_repository(&repo, repo_path, manifest_filename)?;
    let mut reverse_walk = repo.revwalk()?;
    reverse_walk.push_head()?;
    reverse_walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut current_tag = String::new();
    let mut tags_to_push = Vec::new();
    let mut seen_project_manifest = false;

    // Walk through each commit that modified the manifest file
    for oid in reverse_walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        let entry = find_manifest_entry(&tree, &manifest_path, !seen_project_manifest);
        if let Some((_, from_project_path)) = &entry {
            seen_project_manifest |= *from_project_path;
        }
        if let Some((entry, _)) = entry
            && let Ok(blob) = entry
                .to_object(&repo)
                .and_then(|obj| obj.into_blob().map_err(|_| git2::Error::from_str("Not a blob")))
        {
            let content = std::str::from_utf8(blob.content()).unwrap_or_default();
            let Some(version_number) = parse_version(content, project_type) else {
                if verbose {
                    println!(
                        "{}",
                        format!("Failed to parse version from {manifest_filename} at {}", commit.id()).yellow()
                    );
                }
                continue;
            };
            if current_tag == version_number {
                if verbose {
                    println!("{}", format!("Skip {}: {}", version_number, commit.id()).yellow());
                }
                continue;
            }

            current_tag.clone_from(&version_number);
            println!("{}", version_number.bold());

            let version_tag = format!("{tag_prefix}v{version_number}");
            let tag_exists = tag_name_exists(&repo, &version_tag)?;
            if tag_exists {
                println!("{}", format!("Tag {version_tag} already exists, skipping...").yellow());
            } else {
                create_version_tag(&repo, &version_tag, &version_number, commit.id(), dryrun)?;
            }
            if push && !(new_tags_only && tag_exists) {
                if combined_push {
                    tags_to_push.push(version_tag);
                } else {
                    push_tag(&repo, &version_tag, dryrun)?;
                }
            }
        }
    }

    // Push all the collected tags at once
    if push && !tags_to_push.is_empty() {
        if verbose {
            println!("Pushing {} tags to remote", tags_to_push.len());
        }
        push_all_tags(&repo, &tags_to_push, dryrun)?;
    }

    Ok(())
}

/// Create version tag with the given version for the given object identifier (commit).
fn create_version_tag(repo: &Repository, tag_name: &str, version_number: &str, oid: Oid, dryrun: bool) -> Result<()> {
    let message = format!("Version {version_number}");
    if dryrun {
        println!("Dry-run: Tag {tag_name} with message '{message}'");
        return Ok(());
    }

    let commit = repo.find_commit(oid)?;
    repo.tag(tag_name, commit.as_object(), &repo.signature()?, &message, false)?;
    println!("Created tag: {tag_name}");
    Ok(())
}

/// Push a single tag to remote.
fn push_tag(repo: &Repository, tag_name: &str, dryrun: bool) -> Result<()> {
    if dryrun {
        println!("Dry-run: Push tag {tag_name}");
        return Ok(());
    }

    let mut remote = repo.find_remote("origin")?;

    // Set up callbacks for authentication
    let mut callbacks = git2::RemoteCallbacks::new();

    // Use Git's credential helper or SSH key from agent
    callbacks.credentials(|_url, username_from_url, allowed_types| {
        if allowed_types.contains(git2::CredentialType::SSH_KEY) {
            return git2::Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"));
        }
        git2::Cred::default()
    });

    // Set up a sideband progress callback to see what is happening
    callbacks.sideband_progress(|data| {
        if let Ok(text) = std::str::from_utf8(data) {
            print!("remote: {text}");
        }
        true
    });

    let mut push_options = git2::PushOptions::new();
    push_options.remote_callbacks(callbacks);

    let refspec = format!("refs/tags/{tag_name}");
    match remote.push(&[&refspec], Some(&mut push_options)) {
        Ok(()) => {
            println!("Pushed tag: {tag_name}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to push tag {tag_name}:\n{e}");
            Err(e.into())
        }
    }
}

/// Push multiple tags to remote.
fn push_all_tags(repo: &Repository, tags: &[String], dryrun: bool) -> Result<()> {
    if dryrun {
        println!("Dry-run: Push tags {tags:?}");
        return Ok(());
    }

    let mut remote = repo.find_remote("origin")?;

    // Set up callbacks for authentication
    let mut callbacks = git2::RemoteCallbacks::new();

    // Use Git's credential helper or SSH key from agent
    callbacks.credentials(|_url, username_from_url, allowed_types| {
        if allowed_types.contains(git2::CredentialType::SSH_KEY) {
            return git2::Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"));
        }
        git2::Cred::default()
    });

    callbacks.sideband_progress(|data| {
        if let Ok(text) = std::str::from_utf8(data) {
            print!("remote: {text}");
        }
        true
    });

    let refspecs: Vec<String> = tags.iter().map(|tag| format!("refs/tags/{tag}")).collect();
    let refspec_refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();

    let mut push_options = git2::PushOptions::new();
    push_options.remote_callbacks(callbacks);

    // Push the tags with the configured options
    match remote.push(&refspec_refs, Some(&mut push_options)) {
        Ok(()) => {
            println!("Pushed tags successfully: {tags:?}");
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to push tags:\n{e}");
            Err(e.into())
        }
    }
}

/// Detect the project type based on which manifest file exists in the directory.
fn detect_project_type(path: &Path) -> Result<ProjectType> {
    if path.join("Cargo.toml").exists() {
        Ok(ProjectType::Rust)
    } else if path.join("CMakeLists.txt").exists() {
        Ok(ProjectType::CMake)
    } else if path.join("pyproject.toml").exists() {
        Ok(ProjectType::Python)
    } else {
        anyhow::bail!("No supported project manifest found (Cargo.toml, CMakeLists.txt, or pyproject.toml)")
    }
}

/// Return the manifest path relative to the repository root, the form used to look it up in commit trees.
///
/// Both paths are canonicalized first, so symlinked temporary directories and relative input paths resolve the same.
fn manifest_path_in_repository(repo: &Repository, project_path: &Path, manifest_filename: &str) -> Result<PathBuf> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Repository has no working directory"))?;
    let workdir = dunce::canonicalize(workdir)?;
    let project = dunce::canonicalize(project_path)?;
    let relative = project.strip_prefix(&workdir).map_err(|_| {
        anyhow::anyhow!(
            "Project path {} is outside the repository {}",
            project.display(),
            workdir.display()
        )
    })?;
    Ok(relative.join(manifest_filename))
}

/// Find the manifest in a commit tree.
///
/// Returns the entry and whether it was found at the project path.
/// When the project lives in a subdirectory and its manifest is missing from the commit,
/// the manifest at the repository root is used instead if `allow_root_fallback` is set.
fn find_manifest_entry(
    tree: &git2::Tree<'_>,
    manifest_path: &Path,
    allow_root_fallback: bool,
) -> Option<(git2::TreeEntry<'static>, bool)> {
    if let Ok(entry) = tree.get_path(manifest_path) {
        return Some((entry, true));
    }
    let file_name = manifest_path.file_name()?;
    let in_subdirectory = manifest_path
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());
    if !allow_root_fallback || !in_subdirectory {
        return None;
    }
    tree.get_path(Path::new(file_name)).ok().map(|entry| (entry, false))
}

/// Parse the version string from the given file content based on the project type.
fn parse_version(content: &str, project_type: ProjectType) -> Option<String> {
    match project_type {
        ProjectType::CMake => parse_cmake_version(content),
        ProjectType::Python => parse_pyproject_toml_version(content),
        ProjectType::Rust => parse_cargo_toml_version(content),
    }
}

/// Parse version from Cargo.toml content.
///
/// Reads `[package].version`, or `[workspace.package].version` for a virtual workspace manifest.
fn parse_cargo_toml_version(content: &str) -> Option<String> {
    let toml_value: toml::Value = toml::from_str(content).ok()?;
    toml_value
        .get("package")
        .or_else(|| toml_value.get("workspace")?.get("package"))?
        .get("version")?
        .as_str()
        .map(ToString::to_string)
}

/// Parse version from CMakeLists.txt `project()` declaration.
fn parse_cmake_version(content: &str) -> Option<String> {
    // Match VERSION followed by a version number inside a project() call.
    // The regex handles both single-line and multi-line project() declarations.
    CMAKE_VERSION_REGEX
        .captures(content)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().to_string())
}

/// Parse version from pyproject.toml content.
fn parse_pyproject_toml_version(content: &str) -> Option<String> {
    let toml_value: toml::Value = toml::from_str(content).ok()?;
    // Standard pyproject.toml uses [project].version
    toml_value
        .get("project")?
        .get("version")?
        .as_str()
        .map(ToString::to_string)
}

/// Check if the tag already exists locally.
fn tag_name_exists(repo: &Repository, tag_name: &str) -> Result<bool> {
    match repo.find_reference(&format!("refs/tags/{tag_name}")) {
        Ok(_) => Ok(true),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Get the project name from the manifest file.
fn get_project_name(path: &Path, project_type: ProjectType) -> Option<String> {
    let manifest_path = path.join(project_type.manifest_filename());
    let content = std::fs::read_to_string(manifest_path).ok()?;
    match project_type {
        ProjectType::CMake => CMAKE_NAME_REGEX
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .map(|m| m.as_str().to_string()),
        ProjectType::Python => {
            let toml_value: toml::Value = toml::from_str(&content).ok()?;
            toml_value
                .get("project")?
                .get("name")?
                .as_str()
                .map(ToString::to_string)
        }
        ProjectType::Rust => {
            let toml_value: toml::Value = toml::from_str(&content).ok()?;
            toml_value
                .get("package")?
                .get("name")?
                .as_str()
                .map(ToString::to_string)
        }
    }
}

#[cfg(test)]
mod test_args {
    use super::*;

    #[test]
    fn parses_defaults_and_all_operational_flags() {
        let defaults = Args::try_parse_from(["vtag"]).expect("default arguments should parse");
        assert!(defaults.command.is_none());
        assert!(defaults.path.is_none());
        assert!(!defaults.dryrun);
        assert!(!defaults.push);
        assert!(!defaults.new);
        assert!(!defaults.single);
        assert!(!defaults.verbose);

        let combined = Args::try_parse_from([
            "vtag",
            "project",
            "--dryrun",
            "--push",
            "--new",
            "--single",
            "--verbose",
        ])
        .expect("combined arguments should parse");
        assert_eq!(combined.path, Some(PathBuf::from("project")));
        assert!(combined.dryrun);
        assert!(combined.push);
        assert!(combined.new);
        assert!(combined.single);
        assert!(combined.verbose);
    }

    #[test]
    fn parses_completion_and_has_valid_command_definition() {
        let args =
            Args::try_parse_from(["vtag", "completion", "bash", "--install"]).expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(VersionTagCommand::Completion {
                shell: Shell::Bash,
                install: true
            })
        ));
        Args::command().debug_assert();
    }
}

#[cfg(test)]
mod test_project_type {
    use super::*;

    #[test]
    fn exposes_manifest_filenames_and_display_names() {
        assert_eq!(ProjectType::Rust.manifest_filename(), "Cargo.toml");
        assert_eq!(ProjectType::CMake.manifest_filename(), "CMakeLists.txt");
        assert_eq!(ProjectType::Python.manifest_filename(), "pyproject.toml");
        assert_eq!(ProjectType::Rust.to_string(), "Rust");
        assert_eq!(ProjectType::CMake.to_string(), "CMake");
        assert_eq!(ProjectType::Python.to_string(), "Python");
    }
}

#[cfg(test)]
mod test_version_parsing {
    use super::*;

    #[test]
    fn parses_cargo_package_version() {
        let content = "[package]\nname = \"demo\"\nversion = \"1.2.3\"";
        let parsed: toml::Value = toml::from_str(content).expect("valid Cargo TOML should parse");
        assert!(parsed.get("package").is_some());
        assert_eq!(parse_cargo_toml_version(content), Some("1.2.3".to_string()));
        assert_eq!(
            parse_version("[package]\nversion = \"1.2.3\"", ProjectType::Rust),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn parses_cargo_workspace_package_version() {
        let content = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.3.0\"";
        assert_eq!(parse_cargo_toml_version(content), Some("0.3.0".to_string()));
    }

    #[test]
    fn cargo_parser_rejects_invalid_or_missing_versions() {
        assert_eq!(parse_cargo_toml_version("invalid = ["), None);
        assert_eq!(parse_cargo_toml_version("[workspace]"), None);
        assert_eq!(parse_cargo_toml_version("[package]\nversion = 123"), None);
    }

    #[test]
    fn parses_python_project_version() {
        let content = "[project]\nname = \"demo\"\nversion = \"2.0.1\"";
        assert_eq!(parse_pyproject_toml_version(content), Some("2.0.1".to_string()));
        assert_eq!(parse_version(content, ProjectType::Python), Some("2.0.1".to_string()));
    }

    #[test]
    fn python_parser_rejects_invalid_or_missing_versions() {
        assert_eq!(parse_pyproject_toml_version("invalid = ["), None);
        assert_eq!(parse_pyproject_toml_version("[project]\nname = \"demo\""), None);
        assert_eq!(parse_pyproject_toml_version("[project]\nversion = 123"), None);
    }

    #[test]
    fn parses_cmake_versions_case_insensitively_across_lines() {
        assert_eq!(
            parse_cmake_version("project(Demo VERSION 3.4)"),
            Some("3.4".to_string())
        );
        assert_eq!(
            parse_cmake_version("PROJECT(\n  Demo\n  VERSION 3.4.5.6\n)"),
            Some("3.4.5.6".to_string())
        );
        assert_eq!(
            parse_version("project(Demo VERSION 3.4.5)", ProjectType::CMake),
            Some("3.4.5".to_string())
        );
    }

    #[test]
    fn cmake_parser_requires_project_version() {
        assert_eq!(parse_cmake_version("project(Demo)"), None);
        assert_eq!(parse_cmake_version("set(VERSION 1.2.3)"), None);
    }
}

#[cfg(test)]
mod test_project_detection {
    use super::*;

    #[test]
    fn detects_each_supported_manifest() -> anyhow::Result<()> {
        let rust_directory = tempfile::TempDir::new()?;
        std::fs::write(rust_directory.path().join("Cargo.toml"), "[package]")?;
        assert_eq!(detect_project_type(rust_directory.path())?, ProjectType::Rust);

        let cmake_directory = tempfile::TempDir::new()?;
        std::fs::write(cmake_directory.path().join("CMakeLists.txt"), "project(Demo)")?;
        assert_eq!(detect_project_type(cmake_directory.path())?, ProjectType::CMake);

        let python_directory = tempfile::TempDir::new()?;
        std::fs::write(python_directory.path().join("pyproject.toml"), "[project]")?;
        assert_eq!(detect_project_type(python_directory.path())?, ProjectType::Python);
        Ok(())
    }

    #[test]
    fn manifest_precedence_is_rust_then_cmake_then_python() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        std::fs::write(temp_directory.path().join("pyproject.toml"), "[project]")?;
        std::fs::write(temp_directory.path().join("CMakeLists.txt"), "project(Demo)")?;
        assert_eq!(detect_project_type(temp_directory.path())?, ProjectType::CMake);

        std::fs::write(temp_directory.path().join("Cargo.toml"), "[package]")?;
        assert_eq!(detect_project_type(temp_directory.path())?, ProjectType::Rust);
        Ok(())
    }

    #[test]
    fn unsupported_directory_returns_contextual_error() {
        let temp_directory = tempfile::TempDir::new().expect("should create temp directory");
        let error = detect_project_type(temp_directory.path()).expect_err("missing manifest should fail");
        assert!(error.to_string().contains("No supported project manifest found"));
    }
}

#[cfg(test)]
mod test_project_names {
    use super::*;

    #[test]
    fn reads_names_from_all_supported_manifests() -> anyhow::Result<()> {
        let rust_directory = tempfile::TempDir::new()?;
        std::fs::write(
            rust_directory.path().join("Cargo.toml"),
            "[package]\nname = \"rust-demo\"\nversion = \"1.0.0\"",
        )?;
        assert_eq!(
            get_project_name(rust_directory.path(), ProjectType::Rust),
            Some("rust-demo".to_string())
        );

        let python_directory = tempfile::TempDir::new()?;
        std::fs::write(
            python_directory.path().join("pyproject.toml"),
            "[project]\nname = \"python-demo\"\nversion = \"1.0.0\"",
        )?;
        assert_eq!(
            get_project_name(python_directory.path(), ProjectType::Python),
            Some("python-demo".to_string())
        );

        let cmake_directory = tempfile::TempDir::new()?;
        std::fs::write(
            cmake_directory.path().join("CMakeLists.txt"),
            "project(\"CMakeDemo\" VERSION 1.0)",
        )?;
        assert_eq!(
            get_project_name(cmake_directory.path(), ProjectType::CMake),
            Some("CMakeDemo".to_string())
        );
        Ok(())
    }

    #[test]
    fn missing_or_malformed_manifest_has_no_name() -> anyhow::Result<()> {
        let temp_directory = tempfile::TempDir::new()?;
        assert_eq!(get_project_name(temp_directory.path(), ProjectType::Rust), None);
        std::fs::write(temp_directory.path().join("Cargo.toml"), "invalid = [")?;
        assert_eq!(get_project_name(temp_directory.path(), ProjectType::Rust), None);
        Ok(())
    }
}

#[cfg(test)]
mod version_tag_test_helpers {
    use super::*;
    use tempfile::TempDir;

    /// Repository with one commit per given version, each changing `Cargo.toml`.
    ///
    /// The signature is set in the repository config,
    /// so tagging works without depending on the machine's global git config.
    pub fn repository_with_versions(versions: &[&str]) -> (TempDir, Repository) {
        let directory = TempDir::new().expect("temporary directory");
        let repository = Repository::init(directory.path()).expect("the repository should initialize");
        {
            let mut config = repository.config().expect("the config should be readable");
            config.set_str("user.name", "Test").expect("name should be set");
            config
                .set_str("user.email", "test@example.com")
                .expect("email should be set");
        }

        let signature = git2::Signature::now("Test", "test@example.com").expect("signature should be created");
        let mut parent: Option<Oid> = None;
        for version in versions {
            std::fs::write(
                directory.path().join("Cargo.toml"),
                format!("[package]\nname = \"demo\"\nversion = \"{version}\"\n"),
            )
            .expect("the manifest should be written");

            let mut index = repository.index().expect("the index should be readable");
            index
                .add_path(Path::new("Cargo.toml"))
                .expect("the manifest should be staged");
            index.write().expect("the index should be written");
            let tree_id = index.write_tree().expect("the tree should be written");
            let tree = repository.find_tree(tree_id).expect("the tree should be found");

            let parent_commit = parent.map(|oid| repository.find_commit(oid).expect("the parent should be found"));
            let parents: Vec<&git2::Commit<'_>> = parent_commit.iter().collect();
            parent = Some(
                repository
                    .commit(
                        Some("HEAD"),
                        &signature,
                        &signature,
                        &format!("version {version}"),
                        &tree,
                        &parents,
                    )
                    .expect("the commit should be created"),
            );
        }
        (directory, repository)
    }

    /// Empty repository with a signature in its config.
    pub fn empty_repository() -> (TempDir, Repository) {
        let directory = TempDir::new().expect("temporary directory");
        let repository = Repository::init(directory.path()).expect("the repository should initialize");
        {
            let mut config = repository.config().expect("the config should be readable");
            config.set_str("user.name", "Test").expect("name should be set");
            config
                .set_str("user.email", "test@example.com")
                .expect("email should be set");
        }
        (directory, repository)
    }

    /// Commits file changes on top of `HEAD`: `Some(content)` writes a file and `None` removes it.
    pub fn commit_changes(repository: &Repository, root: &Path, changes: &[(&str, Option<&str>)]) {
        let mut index = repository.index().expect("the index should be readable");
        for (relative, content) in changes {
            let path = root.join(relative);
            if let Some(content) = content {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("the directory should be created");
                }
                std::fs::write(&path, content).expect("the file should be written");
                index.add_path(Path::new(relative)).expect("the file should be staged");
            } else {
                std::fs::remove_file(&path).expect("the file should be removed");
                index
                    .remove_path(Path::new(relative))
                    .expect("the removal should be staged");
            }
        }
        index.write().expect("the index should be written");
        let tree_id = index.write_tree().expect("the tree should be written");
        let tree = repository.find_tree(tree_id).expect("the tree should be found");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature should be created");
        let parent = repository.head().ok().and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        repository
            .commit(Some("HEAD"), &signature, &signature, "change", &tree, &parents)
            .expect("the commit should be created");
    }

    /// Tag names in the repository, sorted.
    pub fn tag_names(repository: &Repository) -> Vec<String> {
        let mut names: Vec<String> = repository
            .tag_names(None)
            .expect("tags should be readable")
            .iter()
            .flatten()
            .flatten()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod test_version_tag {
    use super::version_tag_test_helpers::*;
    use super::*;

    #[test]
    fn a_tag_is_created_for_every_version_change() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.2.0", "1.0.0"]);

        version_tag(&directory.path().to_path_buf(), false, false, true, false, false, "")
            .expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v0.1.0", "v0.2.0", "v1.0.0"]);
    }

    #[test]
    fn a_version_that_did_not_change_is_tagged_once() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.1.0", "0.2.0"]);

        version_tag(&directory.path().to_path_buf(), false, false, true, false, false, "")
            .expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v0.1.0", "v0.2.0"]);
    }

    #[test]
    fn a_dryrun_creates_no_tags() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.2.0"]);

        version_tag(&directory.path().to_path_buf(), false, true, false, false, false, "")
            .expect("a dryrun should succeed");

        assert_eq!(tag_names(&repository), [] as [String; 0]);
    }

    #[test]
    fn running_twice_leaves_the_existing_tags_alone() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.2.0"]);
        let path = directory.path().to_path_buf();

        version_tag(&path, false, false, false, false, false, "").expect("tagging should succeed");
        version_tag(&path, false, false, true, false, false, "").expect("a second run should succeed");

        assert_eq!(tag_names(&repository), vec!["v0.1.0", "v0.2.0"]);
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_skipped() {
        let (directory, repository) = repository_with_versions(&["0.1.0"]);
        std::fs::write(directory.path().join("Cargo.toml"), "this is not toml at all\n")
            .expect("the manifest should be written");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        let mut index = repository.index().expect("index");
        index.add_path(Path::new("Cargo.toml")).expect("staged");
        index.write().expect("written");
        let tree_id = index.write_tree().expect("tree");
        let tree = repository.find_tree(tree_id).expect("tree found");
        let head = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .expect("head commit");
        repository
            .commit(Some("HEAD"), &signature, &signature, "broken", &tree, &[&head])
            .expect("commit");

        version_tag(&directory.path().to_path_buf(), false, false, true, false, false, "")
            .expect("an unparseable manifest should be skipped");

        assert_eq!(tag_names(&repository), vec!["v0.1.0"]);
    }

    #[test]
    fn a_directory_without_a_manifest_is_an_error() {
        let directory = tempfile::TempDir::new().expect("temporary directory");

        let error = version_tag(&directory.path().to_path_buf(), false, true, false, false, false, "")
            .expect_err("a directory without a manifest should fail");

        assert!(error.to_string().contains("No supported project manifest"), "{error}");
    }

    #[test]
    fn pushing_in_a_dryrun_needs_no_remote() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.2.0"]);

        version_tag(&directory.path().to_path_buf(), true, true, true, false, false, "")
            .expect("a dryrun push should succeed");
        version_tag(&directory.path().to_path_buf(), true, true, true, true, false, "")
            .expect("a combined dryrun push should succeed");

        assert_eq!(tag_names(&repository), [] as [String; 0]);
    }
}

#[cfg(test)]
mod test_subdirectory_projects {
    use super::version_tag_test_helpers::*;
    use super::*;

    /// Cargo workspace manifest with the version in `[workspace.package]`.
    fn workspace_manifest(version: &str) -> String {
        format!("[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"{version}\"\n")
    }

    #[test]
    fn tags_a_workspace_in_a_subdirectory_with_a_prefix() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        commit_changes(
            &repository,
            root,
            &[("CMakeLists.txt", Some("project(App VERSION 1.0.0)"))],
        );
        commit_changes(
            &repository,
            root,
            &[("rust/Cargo.toml", Some(&workspace_manifest("0.1.0")))],
        );
        commit_changes(
            &repository,
            root,
            &[("CMakeLists.txt", Some("project(App VERSION 1.1.0)"))],
        );
        commit_changes(
            &repository,
            root,
            &[("rust/Cargo.toml", Some(&workspace_manifest("0.2.0")))],
        );

        version_tag(&root.join("rust"), false, false, true, false, false, "rust-").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["rust-v0.1.0", "rust-v0.2.0"]);
    }

    #[test]
    fn falls_back_to_the_root_manifest_before_the_project_moved() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        commit_changes(
            &repository,
            root,
            &[("CMakeLists.txt", Some("project(App VERSION 1.0.0)"))],
        );
        commit_changes(
            &repository,
            root,
            &[("CMakeLists.txt", Some("project(App VERSION 1.1.0)"))],
        );
        commit_changes(
            &repository,
            root,
            &[
                ("CMakeLists.txt", None),
                ("cpp/CMakeLists.txt", Some("project(App VERSION 1.1.0)")),
            ],
        );
        commit_changes(
            &repository,
            root,
            &[("cpp/CMakeLists.txt", Some("project(App VERSION 1.2.0)"))],
        );

        version_tag(&root.join("cpp"), false, false, true, false, false, "").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v1.0.0", "v1.1.0", "v1.2.0"]);
    }

    #[test]
    fn a_root_manifest_after_the_move_is_ignored() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        commit_changes(
            &repository,
            root,
            &[("rust/Cargo.toml", Some(&workspace_manifest("0.1.0")))],
        );
        commit_changes(&repository, root, &[("Cargo.toml", Some(&workspace_manifest("9.0.0")))]);

        version_tag(&root.join("rust"), false, false, true, false, false, "").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v0.1.0"]);
    }

    #[test]
    fn prefixed_tags_do_not_collide_with_plain_tags() {
        let (directory, repository) = repository_with_versions(&["0.1.0"]);
        let path = directory.path().to_path_buf();
        version_tag(&path, false, false, false, false, false, "").expect("tagging should succeed");

        version_tag(&path, false, false, false, false, false, "rust-").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["rust-v0.1.0", "v0.1.0"]);
    }

    #[test]
    fn manifest_path_is_relative_to_the_repository_root() {
        let (directory, repository) = empty_repository();
        std::fs::create_dir_all(directory.path().join("rust")).expect("directory");
        let relative = manifest_path_in_repository(&repository, &directory.path().join("rust"), "Cargo.toml")
            .expect("path inside the repository");
        assert_eq!(relative, Path::new("rust").join("Cargo.toml"));
        let at_root =
            manifest_path_in_repository(&repository, directory.path(), "Cargo.toml").expect("repository root");
        assert_eq!(at_root, Path::new("Cargo.toml"));
    }
}

#[cfg(test)]
mod test_tag_helpers {
    use super::version_tag_test_helpers::*;
    use super::*;

    #[test]
    fn creating_a_tag_names_it_and_records_the_message() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);
        let head = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .expect("head commit");

        create_version_tag(&repository, "v1.2.3", "1.2.3", head.id(), false).expect("the tag should be created");

        assert_eq!(tag_names(&repository), vec!["v1.2.3"]);
    }

    #[test]
    fn creating_a_tag_in_a_dryrun_records_nothing() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);
        let head = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .expect("head commit");

        create_version_tag(&repository, "v1.2.3", "1.2.3", head.id(), true).expect("a dryrun should succeed");

        assert_eq!(tag_names(&repository), [] as [String; 0]);
    }

    #[test]
    fn an_existing_tag_is_recognised_and_an_unrelated_name_is_not() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);
        let head = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .expect("head commit");
        create_version_tag(&repository, "v1.2.3", "1.2.3", head.id(), false).expect("the tag should be created");

        assert!(tag_name_exists(&repository, "v1.2.3").expect("tags should be readable"));
        assert!(!tag_name_exists(&repository, "v9.9.9").expect("tags should be readable"));
    }

    #[test]
    fn a_repository_without_tags_reports_none() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);

        assert!(!tag_name_exists(&repository, "v1.2.3").expect("tags should be readable"));
    }

    #[test]
    fn pushing_in_a_dryrun_reports_without_a_remote() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);

        push_tag(&repository, "v1.2.3", true).expect("a dryrun push should succeed");
        push_all_tags(&repository, &["v1.2.3".to_string()], true).expect("a dryrun push should succeed");
    }

    #[test]
    fn pushing_without_a_remote_is_an_error() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);

        assert!(push_tag(&repository, "v1.2.3", false).is_err());
        assert!(push_all_tags(&repository, &["v1.2.3".to_string()], false).is_err());
    }
}
