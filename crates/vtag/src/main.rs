//! `Git` version tag creation for `CMake`, Python, and Rust projects.
//!
//! Detects project manifests, parses versions and names, and creates or pushes `Git` tags.

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cli_tools_common::CompletionArgs;
use colored::Colorize;
use git2::{Oid, Repository};
use regex::Regex;

/// Captures the arguments of a `CMake` `project()` call, including parentheses inside quoted strings.
static CMAKE_PROJECT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)\bproject\s*\(((?:[^"()]|"(?:[^"\\]|\\.)*")*)\)"#).expect("Invalid regex"));

/// Matches one quoted or unquoted `CMake` project argument.
static CMAKE_ARGUMENT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)"((?:[^"\\]|\\.)*)"|([^"\s]+)"#).expect("Invalid regex"));

/// Matches a complete `CMake` version with one to four numeric components.
static CMAKE_VERSION_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[0-9]+(?:\.[0-9]+){0,3}\z").expect("Invalid regex"));

/// Matches the project name (e.g. `project("AudioBatch" ...)` or `project(AudioBatch ...)`).
static CMAKE_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bproject\s*\(\s*["']?([^"'\s)]+)["']?"#).expect("Invalid regex"));

/// Subcommands for vtag.
#[derive(Subcommand, Debug)]
enum VersionTagCommand {
    /// Generate shell completion script
    #[command(name = "completion")]
    Completion(CompletionArgs),
}

/// Supported project types for version tagging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProjectType {
    #[value(name = "cmake")]
    CMake,
    Python,
    Rust,
}

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

    /// Walk merged side branches as well instead of only first-parent history
    #[arg(short = 'm', long)]
    all_branches: bool,

    /// Git remote name to push tags to
    #[arg(short = 'R', long, value_name = "REMOTE", default_value = "origin")]
    remote: String,

    /// Override project type detection
    #[arg(short = 'T', long = "type", value_enum, value_name = "TYPE")]
    project_type: Option<ProjectType>,

    /// Print verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Prefix for the tag names, for example "rust-" for tags like rust-v1.2.3
    #[arg(short = 't', long, value_name = "PREFIX", default_value = "")]
    tag_prefix: String,
}

/// Decides which root manifests in the history of a subdirectory project belong to that project.
///
/// A historical root manifest belongs to the subdirectory project only if their names match.
struct RootFallback {
    /// Package or project name of the subdirectory manifest at `HEAD`.
    project_name: Option<String>,
    /// Project type used to read names from manifests.
    project_type: ProjectType,
}

/// Planned version tag discovered during history traversal.
struct PlannedTag {
    version_number: String,
    tag_name: String,
    commit_id: Oid,
}

/// Options controlling `version_tag`.
struct TagOptions<'a> {
    push: bool,
    dryrun: bool,
    verbose: bool,
    combined_push: bool,
    new_tags_only: bool,
    all_branches: bool,
    remote: &'a str,
    project_type: Option<ProjectType>,
    tag_prefix: &'a str,
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

impl RootFallback {
    /// Read the project name from the committed manifest at `HEAD`.
    fn at_head(repo: &Repository, manifest_path: &Path, project_type: ProjectType) -> Self {
        let project_name = (|| {
            let head_tree = repo.head().ok()?.peel_to_tree().ok()?;
            let entry = head_tree.get_path(manifest_path).ok()?;
            let blob = entry.to_object(repo).ok()?.into_blob().ok()?;
            let content = std::str::from_utf8(blob.content()).ok()?;
            parse_project_name(content, project_type).filter(|name| !name.trim().is_empty())
        })();
        Self {
            project_name,
            project_type,
        }
    }

    /// Whether a root manifest from the history belongs to the subdirectory project.
    fn accepts(&self, root_manifest: &str) -> bool {
        self.project_name.is_some() && parse_project_name(root_manifest, self.project_type) == self.project_name
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(VersionTagCommand::Completion(completion)) = &args.command {
        return completion.generate::<Args>(args.verbose, env!("CARGO_BIN_NAME"));
    }
    let repo_path = cli_tools_common::resolve_input_path(args.path.as_deref())?;
    if !repo_path.is_dir() {
        anyhow::bail!("Input path needs to be a directory in a git repository")
    }
    let options = TagOptions {
        push: args.push,
        dryrun: args.dryrun,
        verbose: args.verbose,
        combined_push: args.single,
        new_tags_only: args.new,
        all_branches: args.all_branches,
        remote: &args.remote,
        project_type: args.project_type,
        tag_prefix: &args.tag_prefix,
    };
    version_tag_with_options(&repo_path, &options)
}

/// Compatibility helper used by tests to invoke `version_tag` with default remote and first-parent history.
#[cfg(test)]
fn version_tag(
    repo_path: &Path,
    push: bool,
    dryrun: bool,
    verbose: bool,
    combined_push: bool,
    new_tags_only: bool,
    tag_prefix: &str,
) -> Result<()> {
    let options = TagOptions {
        push,
        dryrun,
        verbose,
        combined_push,
        new_tags_only,
        all_branches: false,
        remote: "origin",
        project_type: None,
        tag_prefix,
    };
    version_tag_with_options(repo_path, &options)
}

/// Create version tags for each unique package version from the project's git history.
///
/// The project may live in a subdirectory of the repository, such as `rust/` in a repository with several projects.
/// Its manifest is then read from that subdirectory in each commit,
/// and commits before the manifest first appeared there use the repository root manifest only if the project names match.
/// This preserves the history of a named project moved into a subdirectory.
/// Tag names are the version with a `v` and the given prefix in front, for example `rust-v1.2.3`.
fn version_tag_with_options(repo_path: &Path, options: &TagOptions<'_>) -> Result<()> {
    let project_type = match options.project_type {
        Some(project_type) => project_type,
        None => detect_project_type(repo_path)?,
    };
    let manifest_filename = project_type.manifest_filename();

    if options.verbose {
        let name = get_project_name(repo_path, project_type)
            .unwrap_or_else(|| cli_tools_common::path_to_string_relative(repo_path));
        println!(
            "{}",
            format!("Creating version tags for {name} ({project_type})")
                .magenta()
                .bold()
        );
    }

    let repo = Repository::discover(repo_path)?;
    let manifest_path = manifest_path_in_repository(&repo, repo_path, manifest_filename)?;
    let planned_tags = collect_planned_tags(&repo, &manifest_path, project_type, options)?;

    let mut tags_to_push = Vec::new();
    for planned in planned_tags {
        println!("{}", planned.version_number.bold());
        let tag_exists = tag_name_exists(&repo, &planned.tag_name)?;
        if tag_exists {
            println!(
                "{}",
                format!("Tag {} already exists, skipping...", planned.tag_name).yellow()
            );
        } else {
            create_version_tag(
                &repo,
                &planned.tag_name,
                &planned.version_number,
                planned.commit_id,
                options.dryrun,
            )?;
        }
        if options.push && !(options.new_tags_only && tag_exists) {
            if options.combined_push {
                tags_to_push.push(planned.tag_name);
            } else {
                push_tag(&repo, options.remote, &planned.tag_name, options.dryrun)?;
            }
        }
    }

    // Push all the collected tags at once
    if options.push && !tags_to_push.is_empty() {
        if options.verbose {
            println!("Pushing {} tags to remote {}", tags_to_push.len(), options.remote);
        }
        push_all_tags(&repo, options.remote, &tags_to_push, options.dryrun)?;
    }

    Ok(())
}

/// Walk git history and collect validated version tags before creating or pushing anything.
fn collect_planned_tags(
    repo: &Repository,
    manifest_path: &Path,
    project_type: ProjectType,
    options: &TagOptions<'_>,
) -> Result<Vec<PlannedTag>> {
    let manifest_filename = project_type.manifest_filename();
    let root_fallback = RootFallback::at_head(repo, manifest_path, project_type);
    let mut reverse_walk = repo.revwalk()?;
    reverse_walk.push_head()?;
    if !options.all_branches {
        reverse_walk.simplify_first_parent()?;
    }
    reverse_walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut seen_versions = std::collections::HashSet::new();
    let mut planned_tags = Vec::new();
    let mut seen_project_manifest = false;

    for oid in reverse_walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        let entry = find_manifest_entry(&tree, manifest_path, !seen_project_manifest);
        if let Some((_, from_project_path)) = &entry {
            seen_project_manifest |= *from_project_path;
        }
        if let Some((entry, from_project_path)) = entry
            && let Ok(blob) = entry
                .to_object(repo)
                .and_then(|obj| obj.into_blob().map_err(|_| git2::Error::from_str("Not a blob")))
        {
            let content = std::str::from_utf8(blob.content()).unwrap_or_default();
            if !from_project_path && !root_fallback.accepts(content) {
                continue;
            }
            let source_path = if from_project_path {
                manifest_path
            } else {
                Path::new(manifest_filename)
            };
            let Some(version_number) = parse_version(content, project_type, repo, &tree, source_path) else {
                if options.verbose {
                    println!(
                        "{}",
                        format!("Failed to parse version from {manifest_filename} at {}", commit.id()).yellow()
                    );
                }
                continue;
            };
            if !seen_versions.insert(version_number.clone()) {
                if options.verbose {
                    println!("{}", format!("Skip {}: {}", version_number, commit.id()).yellow());
                }
                continue;
            }

            let tag_name = format!("{}v{version_number}", options.tag_prefix);
            let full_ref = format!("refs/tags/{tag_name}");
            if !git2::Reference::is_valid_name(&full_ref) {
                anyhow::bail!("Invalid git tag name: '{tag_name}'");
            }
            planned_tags.push(PlannedTag {
                version_number,
                tag_name,
                commit_id: commit.id(),
            });
        }
    }

    Ok(planned_tags)
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

/// Build push callbacks supporting multi-key SSH agent, username queries, on-disk SSH keys, and rejection capture.
fn build_push_callbacks(
    git_config: Option<git2::Config>,
    rejections: &std::sync::Mutex<Vec<String>>,
) -> git2::RemoteCallbacks<'_> {
    const MAX_AGENT_ATTEMPTS: usize = 5;
    let on_disk_keys: Vec<PathBuf> = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map_or_default(|home| {
            ["id_ed25519", "id_ecdsa", "id_rsa"]
                .iter()
                .map(|name| home.join(".ssh").join(name))
                .filter(|path| path.is_file())
                .collect()
        });

    let mut callbacks = git2::RemoteCallbacks::new();
    let mut agent_attempts = 0usize;
    let mut disk_key_index = 0usize;
    let mut tried_helper = false;
    let mut tried_default = false;
    callbacks.credentials(move |url, username_from_url, allowed_types| {
        let username = username_from_url.unwrap_or("git");
        if allowed_types.contains(git2::CredentialType::USERNAME) {
            return git2::Cred::username(username);
        }
        if allowed_types.contains(git2::CredentialType::SSH_KEY) {
            if agent_attempts < MAX_AGENT_ATTEMPTS {
                agent_attempts += 1;
                if let Ok(cred) = git2::Cred::ssh_key_from_agent(username) {
                    return Ok(cred);
                }
            }
            if let Some(key_path) = on_disk_keys.get(disk_key_index) {
                disk_key_index += 1;
                let pub_path = key_path.with_extension("pub");
                let pub_ref = pub_path.is_file().then_some(pub_path.as_path());
                return git2::Cred::ssh_key(username, pub_ref, key_path, None);
            }
            return Err(git2::Error::from_str("SSH authentication failed"));
        }
        if allowed_types.contains(git2::CredentialType::USER_PASS_PLAINTEXT)
            && !tried_helper
            && let Some(ref config) = git_config
        {
            tried_helper = true;
            if let Ok(cred) = git2::Cred::credential_helper(config, url, username_from_url) {
                return Ok(cred);
            }
        }
        if !tried_default {
            tried_default = true;
            return git2::Cred::default();
        }
        Err(git2::Error::from_str("Authentication failed"))
    });
    callbacks.sideband_progress(|data| {
        if let Ok(text) = std::str::from_utf8(data) {
            print!("remote: {text}");
        }
        true
    });
    callbacks.push_update_reference(move |refname, status| {
        if let Some(message) = status
            && let Ok(mut list) = rejections.lock()
        {
            list.push(format!("{refname}: {message}"));
        }
        Ok(())
    });
    callbacks
}

/// Push a single tag to remote.
fn push_tag(repo: &Repository, remote_name: &str, tag_name: &str, dryrun: bool) -> Result<()> {
    if dryrun {
        println!("Dry-run: Push tag {tag_name} to {remote_name}");
        return Ok(());
    }

    let mut remote = repo
        .find_remote(remote_name)
        .with_context(|| format!("Git remote '{remote_name}' not found"))?;
    let rejections = std::sync::Mutex::new(Vec::new());
    let callbacks = build_push_callbacks(repo.config().ok(), &rejections);

    let mut push_options = git2::PushOptions::new();
    push_options.remote_callbacks(callbacks);

    let refspec = format!("refs/tags/{tag_name}");
    remote
        .push(&[&refspec], Some(&mut push_options))
        .with_context(|| format!("Failed to push tag {tag_name} to {remote_name}"))?;
    if let Ok(list) = rejections.lock()
        && !list.is_empty()
    {
        anyhow::bail!("Remote rejected tag {tag_name}: {}", list.join("; "));
    }
    println!("Pushed tag: {tag_name}");
    Ok(())
}

/// Push multiple tags to remote.
fn push_all_tags(repo: &Repository, remote_name: &str, tags: &[String], dryrun: bool) -> Result<()> {
    if dryrun {
        println!("Dry-run: Push tags {tags:?} to {remote_name}");
        return Ok(());
    }

    let mut remote = repo
        .find_remote(remote_name)
        .with_context(|| format!("Git remote '{remote_name}' not found"))?;
    let rejections = std::sync::Mutex::new(Vec::new());
    let callbacks = build_push_callbacks(repo.config().ok(), &rejections);

    let refspecs: Vec<String> = tags.iter().map(|tag| format!("refs/tags/{tag}")).collect();
    let refspec_refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();

    let mut push_options = git2::PushOptions::new();
    push_options.remote_callbacks(callbacks);

    remote
        .push(&refspec_refs, Some(&mut push_options))
        .with_context(|| format!("Failed to push tags to {remote_name}"))?;
    if let Ok(list) = rejections.lock()
        && !list.is_empty()
    {
        anyhow::bail!("Remote rejected tags: {}", list.join("; "));
    }
    println!("Pushed tags successfully: {tags:?}");
    Ok(())
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
fn manifest_path_in_repository(repo: &Repository, project_path: &Path, manifest_filename: &str) -> Result<PathBuf> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Repository has no working directory"))?;
    let relative = cli_tools_common::relative_path_within(project_path, workdir).ok_or_else(|| {
        anyhow::anyhow!(
            "Project path {} is outside the repository {}",
            project_path.display(),
            workdir.display()
        )
    })?;
    Ok(relative.join(manifest_filename))
}

/// Match a `CMake` bracket opener (`[[`, `[=[`, `[==[`, ...) at the start of `text`,
/// returning `(consumed_bytes, equals_count)`.
fn cmake_bracket_open(text: &str) -> Option<(usize, usize)> {
    let rest = text.strip_prefix('[')?;
    let equals = rest.bytes().take_while(|&byte| byte == b'=').count();
    if rest.as_bytes().get(equals) == Some(&b'[') {
        Some((equals + 2, equals))
    } else {
        None
    }
}

/// Strip `#` line comments and `#[[...]]` bracket comments from `CMakeLists.txt` content,
/// while preserving quoted strings and `[[...]]` bracket arguments.
fn strip_cmake_comments(content: &str) -> String {
    let mut output = String::with_capacity(content.len());
    let mut index = 0;
    let bytes = content.as_bytes();
    while index < content.len() {
        let rest = content.get(index..).unwrap_or_default();
        if let Some(ch) = rest.chars().next() {
            match ch {
                '"' => {
                    output.push('"');
                    index += 1;
                    let mut escaped = false;
                    while let Some(next_char) = content.get(index..).and_then(|s| s.chars().next()) {
                        output.push(next_char);
                        index += next_char.len_utf8();
                        if escaped {
                            escaped = false;
                        } else if next_char == '\\' {
                            escaped = true;
                        } else if next_char == '"' {
                            break;
                        }
                    }
                }
                '[' => {
                    if let Some((open_len, equals)) = cmake_bracket_open(rest) {
                        let closing = format!("]{}]", "=".repeat(equals));
                        let after_open = index + open_len;
                        if let Some(end_rel) = content.get(after_open..).and_then(|s| s.find(&closing)) {
                            let end = after_open + end_rel + closing.len();
                            if let Some(segment) = content.get(index..end) {
                                output.push_str(segment);
                            }
                            index = end;
                            continue;
                        }
                    }
                    output.push('[');
                    index += 1;
                }
                '#' => {
                    let after_hash = content.get(index + 1..).unwrap_or_default();
                    if let Some((open_len, equals)) = cmake_bracket_open(after_hash) {
                        let closing = format!("]{}]", "=".repeat(equals));
                        let after_open = index + 1 + open_len;
                        if let Some(end_rel) = content.get(after_open..).and_then(|s| s.find(&closing)) {
                            index = after_open + end_rel + closing.len();
                            continue;
                        }
                        break;
                    }
                    // Skip to end of line, keeping the newline.
                    while index < bytes.len() && bytes.get(index) != Some(&b'\n') {
                        index += 1;
                    }
                }
                other => {
                    output.push(other);
                    index += other.len_utf8();
                }
            }
        } else {
            break;
        }
    }
    output
}

/// Parse the package or project name from manifest content.
fn parse_project_name(content: &str, project_type: ProjectType) -> Option<String> {
    match project_type {
        ProjectType::CMake => {
            let stripped = strip_cmake_comments(content);
            CMAKE_NAME_REGEX
                .captures(&stripped)
                .and_then(|captures| captures.get(1))
                .map(|name| name.as_str().to_string())
        }
        ProjectType::Python => {
            let toml_value: toml::Value = toml::from_str(content).ok()?;
            toml_value
                .get("project")
                .or_else(|| toml_value.get("tool")?.get("poetry"))?
                .get("name")?
                .as_str()
                .map(ToString::to_string)
        }
        ProjectType::Rust => {
            let toml_value: toml::Value = toml::from_str(content).ok()?;
            toml_value
                .get("package")?
                .get("name")?
                .as_str()
                .map(ToString::to_string)
        }
    }
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

/// Parse the version from the manifest and resolve Cargo workspace inheritance from the same commit.
fn parse_version(
    content: &str,
    project_type: ProjectType,
    repo: &Repository,
    tree: &git2::Tree<'_>,
    manifest_path: &Path,
) -> Option<String> {
    match project_type {
        ProjectType::CMake => parse_cmake_version(content),
        ProjectType::Python => parse_pyproject_toml_version(content),
        ProjectType::Rust => parse_cargo_toml_version(content, |explicit_workspace| {
            workspace_manifest_in_tree(repo, tree, manifest_path, explicit_workspace)
        }),
    }
}

/// Read the Cargo package version, resolving workspace inheritance if requested.
fn parse_cargo_toml_version(
    content: &str,
    find_workspace: impl FnOnce(Option<&str>) -> Option<toml::Value>,
) -> Option<String> {
    let manifest: toml::Value = toml::from_str(content).ok()?;
    let Some(package) = manifest.get("package") else {
        return workspace_package_version(&manifest);
    };
    let version = package.get("version")?;
    if let Some(version) = version.as_str() {
        return Some(version.to_owned());
    }
    if version.get("workspace")?.as_bool() != Some(true) {
        return None;
    }
    if manifest.get("workspace").is_some() {
        return workspace_package_version(&manifest);
    }
    let explicit_workspace = match package.get("workspace") {
        Some(path) => Some(path.as_str()?),
        None => None,
    };
    workspace_package_version(&find_workspace(explicit_workspace)?)
}

/// Read `[workspace.package].version` for a virtual workspace or an inheriting package.
fn workspace_package_version(manifest: &toml::Value) -> Option<String> {
    manifest
        .get("workspace")?
        .get("package")?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

/// Find the explicitly configured workspace or the nearest ancestor workspace in the commit tree.
fn workspace_manifest_in_tree(
    repo: &Repository,
    tree: &git2::Tree<'_>,
    manifest_path: &Path,
    explicit_workspace: Option<&str>,
) -> Option<toml::Value> {
    let file_name = manifest_path.file_name()?;
    let package_directory = manifest_path.parent()?;
    let read_workspace = |directory: &Path| {
        let entry = tree.get_path(&directory.join(file_name)).ok()?;
        let blob = entry.to_object(repo).ok()?.into_blob().ok()?;
        let content = std::str::from_utf8(blob.content()).ok()?;
        let manifest: toml::Value = toml::from_str(content).ok()?;
        manifest.get("workspace")?.as_table()?;
        Some(manifest)
    };
    if let Some(explicit_workspace) = explicit_workspace {
        let directory = resolve_workspace_path(package_directory, Path::new(explicit_workspace))?;
        return read_workspace(&directory);
    }
    package_directory.ancestors().skip(1).find_map(read_workspace)
}

/// Resolve a relative Cargo workspace path without consulting the working tree.
///
/// Absolute paths and paths escaping the repository cannot be read from a commit tree.
fn resolve_workspace_path(package_directory: &Path, explicit_workspace: &Path) -> Option<PathBuf> {
    let mut directory = package_directory.to_path_buf();
    for component in explicit_workspace.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => directory.push(name),
            std::path::Component::ParentDir if directory.pop() => {}
            _ => return None,
        }
    }
    Some(directory)
}

/// Parse the literal `VERSION` argument from a `CMake` `project()` declaration.
fn parse_cmake_version(content: &str) -> Option<String> {
    let stripped = strip_cmake_comments(content);
    CMAKE_PROJECT_REGEX
        .captures_iter(&stripped)
        .filter_map(|project| project.get(1))
        .find_map(|project| cmake_project_version(project.as_str()))
}

/// Find the version among the project arguments without treating other option values as keywords.
fn cmake_project_version(project: &str) -> Option<String> {
    let mut arguments = CMAKE_ARGUMENT_REGEX
        .captures_iter(project)
        .filter_map(|argument| argument.get(1).or_else(|| argument.get(2)))
        .map(|argument| argument.as_str())
        .skip(1);
    while let Some(argument) = arguments.next() {
        if argument.eq_ignore_ascii_case("VERSION") {
            let version = arguments.next()?;
            return CMAKE_VERSION_REGEX.is_match(version).then(|| version.to_owned());
        }
        if ["COMPAT_VERSION", "DESCRIPTION", "HOMEPAGE_URL", "SPDX_LICENSE"]
            .iter()
            .any(|keyword| argument.eq_ignore_ascii_case(keyword))
        {
            arguments.next();
        }
    }
    None
}

/// Prefer a static PEP 621 version, then try the Poetry version if it is missing or not a string.
fn parse_pyproject_toml_version(content: &str) -> Option<String> {
    let toml_value: toml::Value = toml::from_str(content).ok()?;
    toml_value
        .get("project")
        .and_then(|project| project.get("version"))
        .and_then(toml::Value::as_str)
        .or_else(|| toml_value.get("tool")?.get("poetry")?.get("version")?.as_str())
        .map(str::to_owned)
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
    parse_project_name(&content, project_type)
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
        use clap::CommandFactory;
        use clap_complete::Shell;

        let args =
            Args::try_parse_from(["vtag", "completion", "bash", "--install"]).expect("completion command should parse");
        assert!(matches!(
            args.command,
            Some(VersionTagCommand::Completion(CompletionArgs {
                shell: Shell::Bash,
                install: true
            }))
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
        assert_eq!(parse_cargo_toml_version(content, |_| None).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn parses_cargo_workspace_package_version() {
        let content = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.3.0\"";
        assert_eq!(parse_cargo_toml_version(content, |_| None).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn cargo_parser_rejects_invalid_or_missing_versions() {
        assert_eq!(parse_cargo_toml_version("invalid = [", |_| None), None);
        assert_eq!(parse_cargo_toml_version("[workspace]", |_| None), None);
        assert_eq!(parse_cargo_toml_version("[package]\nversion = 123", |_| None), None);
    }

    #[test]
    fn parses_python_project_version() {
        let content = "[project]\nname = \"demo\"\nversion = \"2.0.1\"";
        assert_eq!(parse_pyproject_toml_version(content), Some("2.0.1".to_string()));
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
            parse_cmake_version("project(Demo VERSION 3.4.5)").as_deref(),
            Some("3.4.5")
        );
    }

    #[test]
    fn cmake_parser_requires_project_version() {
        assert_eq!(parse_cmake_version("project(Demo)"), None);
        assert_eq!(parse_cmake_version("set(VERSION 1.2.3)"), None);
    }

    /// A compatibility version is not a project version, regardless of argument order.
    #[test]
    fn cmake_uses_version_instead_of_compat_version() {
        assert_eq!(
            parse_cmake_version("project(Demo VERSION 2.4.0 COMPAT_VERSION 1.0)").as_deref(),
            Some("2.4.0")
        );
        assert_eq!(
            parse_cmake_version("project(Demo COMPAT_VERSION 1.0 VERSION 2.4.0)").as_deref(),
            Some("2.4.0")
        );
        assert_eq!(parse_cmake_version("project(Demo COMPAT_VERSION 1.0)"), None);
    }

    /// A project version is one numeric argument, not text inside a description or a numeric prefix.
    #[test]
    fn cmake_requires_a_complete_literal_version_argument() {
        assert_eq!(
            parse_cmake_version(r#"project(Demo DESCRIPTION "VERSION 9.0" VERSION "2")"#).as_deref(),
            Some("2")
        );
        assert_eq!(parse_cmake_version("project(Demo VERSION 1.2.3.4.5)"), None);
        assert_eq!(parse_cmake_version("project(Demo VERSION 1.2beta)"), None);
    }

    /// A static PEP 621 version takes precedence over conflicting Poetry metadata.
    #[test]
    fn python_prefers_a_static_project_version_to_poetry() {
        let content = "[project]\nname = \"demo\"\nversion = \"2.0\"\n[tool.poetry]\nversion = \"9.0\"\n";
        assert_eq!(parse_pyproject_toml_version(content).as_deref(), Some("2.0"));
    }

    /// Poetry can supply the version even if another section provides the project metadata.
    #[test]
    fn python_falls_back_to_poetry_when_project_version_is_dynamic() {
        let content = "[project]\nname = \"demo\"\ndynamic = [\"version\"]\n[tool.poetry]\nversion = \"1.0\"\n";
        assert_eq!(parse_pyproject_toml_version(content).as_deref(), Some("1.0"));
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
    use cli_tools_common::test_support::TempTree;
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
    pub fn empty_repository() -> (TempTree, Repository) {
        let directory = TempTree::new();
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

        version_tag(directory.path(), false, false, true, false, false, "").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v0.1.0", "v0.2.0", "v1.0.0"]);
    }

    #[test]
    fn a_version_that_did_not_change_is_tagged_once() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.1.0", "0.2.0"]);

        version_tag(directory.path(), false, false, true, false, false, "").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v0.1.0", "v0.2.0"]);
    }

    #[test]
    fn a_dryrun_creates_no_tags() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.2.0"]);

        version_tag(directory.path(), false, true, false, false, false, "").expect("a dryrun should succeed");

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

        version_tag(directory.path(), false, false, true, false, false, "")
            .expect("an unparseable manifest should be skipped");

        assert_eq!(tag_names(&repository), vec!["v0.1.0"]);
    }

    #[test]
    fn a_directory_without_a_manifest_is_an_error() {
        let directory = tempfile::TempDir::new().expect("temporary directory");

        let error = version_tag(directory.path(), false, true, false, false, false, "")
            .expect_err("a directory without a manifest should fail");

        assert!(error.to_string().contains("No supported project manifest"), "{error}");
    }

    #[test]
    fn pushing_in_a_dryrun_needs_no_remote() {
        let (directory, repository) = repository_with_versions(&["0.1.0", "0.2.0"]);

        version_tag(directory.path(), true, true, true, false, false, "").expect("a dryrun push should succeed");
        version_tag(directory.path(), true, true, true, true, false, "")
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

    /// Cargo workspace manifest whose root package inherits the workspace version.
    fn workspace_root_package(version: &str) -> String {
        format!(
            "[package]\nname = \"app\"\nversion.workspace = true\n\n[workspace]\n\n[workspace.package]\nversion = \"{version}\"\n"
        )
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
    fn a_different_project_at_the_root_is_never_used_for_a_subdirectory_project() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        let package = |name: &str, version: &str| format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n");
        commit_changes(&repository, root, &[("Cargo.toml", Some(&package("a", "1.0.0")))]);
        commit_changes(&repository, root, &[("Cargo.toml", Some(&package("a", "2.0.0")))]);
        commit_changes(&repository, root, &[("b/Cargo.toml", Some(&package("b", "0.1.0")))]);

        version_tag(&root.join("b"), false, false, true, false, false, "b-").expect("tagging should succeed");
        assert_eq!(tag_names(&repository), vec!["b-v0.1.0"]);

        commit_changes(&repository, root, &[("Cargo.toml", None)]);

        version_tag(&root.join("b"), false, false, true, false, false, "b-").expect("tagging should succeed");
        assert_eq!(tag_names(&repository), vec!["b-v0.1.0"]);
    }

    /// Unnamed virtual workspaces do not provide positive evidence of a move.
    #[test]
    fn unnamed_workspace_manifests_are_not_assumed_to_be_the_same_project() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        commit_changes(&repository, root, &[("Cargo.toml", Some(&workspace_manifest("9.0.0")))]);
        commit_changes(
            &repository,
            root,
            &[
                ("Cargo.toml", None),
                ("rust/Cargo.toml", Some(&workspace_manifest("1.0.0"))),
            ],
        );

        version_tag(&directory.join("rust"), false, false, true, false, false, "").unwrap();
        assert_eq!(tag_names(&repository), vec!["v1.0.0"]);
    }

    #[test]
    fn the_same_package_moved_while_the_root_manifest_stays_uses_the_root_history() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        let package = |name: &str, version: &str| format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n");
        commit_changes(&repository, root, &[("Cargo.toml", Some(&package("app", "1.0.0")))]);
        commit_changes(
            &repository,
            root,
            &[
                ("Cargo.toml", Some("[workspace]\nmembers = [\"app\"]\n")),
                ("app/Cargo.toml", Some(&package("app", "1.1.0"))),
            ],
        );

        version_tag(&root.join("app"), false, false, true, false, false, "").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v1.0.0", "v1.1.0"]);
    }

    #[test]
    fn a_member_with_an_inherited_version_uses_the_workspace_version() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        let member = "[package]\nname = \"member\"\nversion.workspace = true\n";
        commit_changes(
            &repository,
            root,
            &[
                ("Cargo.toml", Some(&workspace_manifest("1.0.0"))),
                ("crates/member/Cargo.toml", Some(member)),
            ],
        );
        commit_changes(&repository, root, &[("Cargo.toml", Some(&workspace_manifest("1.1.0")))]);

        version_tag(&root.join("crates/member"), false, false, true, false, false, "").expect("tagging should succeed");

        assert_eq!(tag_names(&repository), vec!["v1.0.0", "v1.1.0"]);
    }

    /// A root package reads its version from the workspace table in the same manifest.
    #[test]
    fn a_workspace_root_package_inherits_its_own_workspace_version() {
        let (directory, repository) = empty_repository();
        for version in ["1.0.0", "1.1.0"] {
            commit_changes(
                &repository,
                directory.path(),
                &[("Cargo.toml", Some(&workspace_root_package(version)))],
            );
        }

        version_tag(directory.path(), false, false, true, false, false, "").unwrap();
        assert_eq!(tag_names(&repository), vec!["v1.0.0", "v1.1.0"]);
    }

    /// An explicitly selected workspace can be a sibling of the member's parent directory.
    #[test]
    fn a_member_uses_the_explicit_workspace_from_each_commit() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        let member = "[package]\nname = \"member\"\nworkspace = \"../.././workspace\"\nversion.workspace = true\n";
        let workspace = |version| {
            format!("[workspace]\nmembers = [\"../crates/member\"]\n\n[workspace.package]\nversion = \"{version}\"\n")
        };
        commit_changes(
            &repository,
            root,
            &[
                ("Cargo.toml", Some(&workspace_manifest("9.0.0"))),
                ("workspace/Cargo.toml", Some(&workspace("1.0.0"))),
                ("crates/member/Cargo.toml", Some(member)),
            ],
        );
        commit_changes(&repository, root, &[("workspace/Cargo.toml", None)]);
        commit_changes(
            &repository,
            root,
            &[("workspace/Cargo.toml", Some(&workspace("1.1.0")))],
        );

        version_tag(&directory.join("crates/member"), false, false, true, false, false, "").unwrap();
        assert_eq!(tag_names(&repository), vec!["v1.0.0", "v1.1.0"]);
    }

    /// A workspace with no inherited version does not borrow one from an outer workspace.
    #[test]
    fn a_member_does_not_skip_its_nearest_workspace_when_it_has_no_version() {
        let (directory, repository) = empty_repository();
        commit_changes(
            &repository,
            directory.path(),
            &[
                ("Cargo.toml", Some(&workspace_manifest("9.0.0"))),
                ("nested/Cargo.toml", Some("[workspace]\nmembers = [\"member\"]\n")),
                (
                    "nested/member/Cargo.toml",
                    Some("[package]\nname = \"member\"\nversion.workspace = true\n"),
                ),
            ],
        );

        version_tag(&directory.join("nested/member"), false, false, true, false, false, "").unwrap();
        assert_eq!(tag_names(&repository), [] as [String; 0]);
    }

    /// Historical root manifests inherit from the old location, not the current subdirectory.
    #[test]
    fn a_moved_workspace_root_inherits_versions_from_its_historical_manifest() {
        let (directory, repository) = empty_repository();
        let root = directory.path();
        commit_changes(
            &repository,
            root,
            &[("Cargo.toml", Some(&workspace_root_package("1.0.0")))],
        );
        commit_changes(
            &repository,
            root,
            &[
                ("Cargo.toml", None),
                ("rust/Cargo.toml", Some(&workspace_root_package("1.1.0"))),
            ],
        );

        version_tag(&directory.join("rust"), false, false, true, false, false, "").unwrap();
        assert_eq!(tag_names(&repository), vec!["v1.0.0", "v1.1.0"]);
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

        push_tag(&repository, "origin", "v1.2.3", true).expect("a dryrun push should succeed");
        push_all_tags(&repository, "origin", &["v1.2.3".to_string()], true).expect("a dryrun push should succeed");
    }

    #[test]
    fn pushing_without_a_remote_is_an_error() {
        let (_directory, repository) = repository_with_versions(&["1.2.3"]);

        assert!(push_tag(&repository, "origin", "v1.2.3", false).is_err());
        assert!(push_all_tags(&repository, "origin", &["v1.2.3".to_string()], false).is_err());
    }

    #[test]
    fn cmake_ignores_commented_project_lines_and_prefixed_calls() {
        let content = "\
# project(OldApp VERSION 0.1.0)
my_project(Helper VERSION 0.2.0)
project(RealApp VERSION 1.2.3)
";
        assert_eq!(parse_cmake_version(content).as_deref(), Some("1.2.3"));
        assert_eq!(
            parse_project_name(content, ProjectType::CMake).as_deref(),
            Some("RealApp")
        );
    }

    #[test]
    fn poetry_pyproject_version_is_parsed() {
        let content = "[tool.poetry]\nname = \"demo\"\nversion = \"2.3.4\"\n";
        assert_eq!(parse_pyproject_toml_version(content).as_deref(), Some("2.3.4"));
    }

    #[test]
    fn invalid_tag_prefix_is_rejected() {
        let (directory, _repository) = repository_with_versions(&["1.2.3"]);
        let result = version_tag(directory.path(), false, true, false, false, false, "bad prefix ");
        assert!(result.is_err(), "whitespace in tag prefix must be rejected");
    }
}
