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

    /// Optional git repository path. Defaults to current directory.
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
        anyhow::bail!("Input path needs to be a git repository directory")
    }
    version_tag(&repo_path, args.push, args.dryrun, args.verbose, args.single, args.new)
}

/// Create version tags for each unique package version from the project's git history.
fn version_tag(
    repo_path: &PathBuf,
    push: bool,
    dryrun: bool,
    verbose: bool,
    combined_push: bool,
    new_tags_only: bool,
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
    let mut reverse_walk = repo.revwalk()?;
    reverse_walk.push_head()?;
    reverse_walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut current_tag = String::new();
    let mut tags_to_push = Vec::new();

    // Walk through each commit that modified the manifest file
    for oid in reverse_walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        if let Some(entry) = tree.get_name(manifest_filename)
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

            let version_tag = format!("v{version_number}");
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

/// Parse the version string from the given file content based on the project type.
fn parse_version(content: &str, project_type: ProjectType) -> Option<String> {
    match project_type {
        ProjectType::CMake => parse_cmake_version(content),
        ProjectType::Python => parse_pyproject_toml_version(content),
        ProjectType::Rust => parse_cargo_toml_version(content),
    }
}

/// Parse version from Cargo.toml content.
fn parse_cargo_toml_version(content: &str) -> Option<String> {
    let toml_value: toml::Value = content.parse().ok()?;
    toml_value
        .get("package")?
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
    let toml_value: toml::Value = content.parse().ok()?;
    // Standard pyproject.toml uses [project].version
    toml_value
        .get("project")?
        .get("version")?
        .as_str()
        .map(ToString::to_string)
}

/// Check if the tag already exists locally.
fn tag_name_exists(repo: &Repository, tag_name: &str) -> Result<bool> {
    for tag in repo.tag_names(Some("v*"))?.iter().flatten() {
        if tag == Some(tag_name) {
            return Ok(true);
        }
    }
    Ok(false)
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
            let toml_value: toml::Value = content.parse().ok()?;
            toml_value
                .get("project")?
                .get("name")?
                .as_str()
                .map(ToString::to_string)
        }
        ProjectType::Rust => {
            let toml_value: toml::Value = content.parse().ok()?;
            toml_value
                .get("package")?
                .get("name")?
                .as_str()
                .map(ToString::to_string)
        }
    }
}
