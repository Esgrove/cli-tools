//! The update pipeline for one Rust project.
//!
//! Bumps `rust-version` and the `rust` Docker images,
//! then runs the optional cargo and git steps in order.
//! All output is collected into a [`Report`],
//! so projects processed in parallel do not interleave their output.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result};
use colored::Colorize;

use cli_tools_common::{diff_lines, get_relative_path_or_filename};

use crate::discovery::find_member_manifests;
use crate::dockerfile::update_dockerfile;
use crate::manifest::{ManifestUpdate, RustVersion, has_workspace_table, update_rust_version};
use crate::registry::DigestResolver;

/// Final state of one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The Rust version was bumped in at least one file.
    Updated,
    /// Every file already matched the target version.
    Unchanged,
    /// The git repository has uncommitted changes.
    SkippedDirty,
    /// A step failed, with a short description of which one.
    Failed(String),
}

/// Options that control which steps run.
/// What bumping one workspace member manifest did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberUpdate {
    /// The member's own rust-version was raised.
    Updated,
    /// The member's own rust-version is already at least the target.
    Current,
    /// The member has no rust-version of its own, for example because it inherits the workspace value.
    NoField,
}

#[derive(Debug, Clone, Copy, Default)]
#[expect(clippy::struct_excessive_bools, reason = "independent CLI flags")]
pub struct Options {
    /// Run `cargo upgrade` and `cargo update`.
    pub update: bool,
    /// Run `cargo clean`.
    pub clean: bool,
    /// Run clippy fixes and tests.
    pub check: bool,
    /// Commit the changes.
    pub commit: bool,
    /// Process projects with uncommitted changes.
    pub force: bool,
    /// Only show what would change.
    pub dryrun: bool,
    /// Show the output of successful commands.
    pub verbose: bool,
}

/// Shared state for processing all projects.
pub struct RunContext<'a> {
    /// Rust version to bump to.
    pub target: RustVersion,
    /// Step options.
    pub options: Options,
    /// Docker Hub digest lookup.
    pub resolver: &'a DigestResolver,
    /// Every discovered project root, used to keep nested projects apart.
    pub roots: &'a [PathBuf],
    /// Serializes git commits, since projects can share a repository.
    pub commit_lock: &'a Mutex<()>,
}

/// Git state of a project, read before any project is changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitState {
    /// The project is inside a git work tree.
    pub is_repository: bool,
    /// The project has uncommitted changes.
    pub is_dirty: bool,
}

/// Everything that happened to one project.
#[derive(Debug, Clone)]
pub struct Report {
    /// Project root directory.
    pub root: PathBuf,
    /// Final state.
    pub outcome: Outcome,
    /// The manifest has no `rust-version` field of its own.
    pub missing_rust_version: bool,
    /// The changes were committed.
    pub committed: bool,
    /// Output lines to print under the project heading.
    pub lines: Vec<String>,
}

/// Output of one external command.
struct CommandResult {
    /// The command exited successfully.
    success: bool,
    /// Combined stdout and stderr.
    output: String,
}

impl GitState {
    /// Read the git state of a project.
    ///
    /// Nested project roots are excluded, so their changes do not count for this one.
    #[must_use]
    pub fn read(root: &Path, roots: &[PathBuf]) -> Self {
        let is_repository = run_command(root, "git", &["rev-parse", "--is-inside-work-tree"])
            .is_ok_and(|result| result.success && result.output.trim() == "true");
        if !is_repository {
            return Self::default();
        }
        let is_dirty = has_changes(root, roots).unwrap_or(true);
        Self {
            is_repository,
            is_dirty,
        }
    }
}

impl Report {
    /// Create an empty report for a project.
    const fn new(root: PathBuf) -> Self {
        Self {
            root,
            outcome: Outcome::Unchanged,
            missing_rust_version: false,
            committed: false,
            lines: Vec::new(),
        }
    }

    /// Add an indented output line.
    fn push(&mut self, line: impl Into<String>) {
        self.lines.push(format!("  {}", line.into()));
    }

    /// Add command output, indented one level deeper.
    fn push_output(&mut self, output: &str) {
        self.lines.extend(
            output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| format!("    {line}")),
        );
    }

    /// Mark the project failed with a message.
    fn fail(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.push(format!("{} {message}", "✗".red()));
        self.outcome = Outcome::Failed(message);
    }

    /// Run a command as a step, recording its result.
    ///
    /// Output is shown for failures, for dry runs, and in verbose mode.
    /// Returns whether the step succeeded.
    fn run_step(&mut self, program: &str, args: &[&str], show_output: bool) -> bool {
        let label = format!("{program} {}", args.join(" "));
        match run_command(&self.root.clone(), program, args) {
            Ok(result) if result.success => {
                self.push(format!("{} {label}", "✓".green()));
                if show_output {
                    self.push_output(&result.output);
                }
                true
            }
            Ok(result) => {
                self.fail(format!("{label} failed"));
                self.push_output(&result.output);
                false
            }
            Err(error) => {
                self.fail(format!("{error:#}"));
                false
            }
        }
    }
}

/// Run every step for one project.
#[must_use]
pub fn process_project(root: &Path, git: GitState, context: &RunContext) -> Report {
    let options = context.options;
    let mut report = Report::new(root.to_path_buf());

    if git.is_dirty && !options.force {
        report.push("Skipped, git repository has uncommitted changes".yellow().to_string());
        report.outcome = Outcome::SkippedDirty;
        return report;
    }

    let mut rust_changed = match bump_manifest(&mut report, context) {
        Ok(changed) => changed,
        Err(error) => {
            report.fail(format!("{error:#}"));
            return report;
        }
    };
    match bump_dockerfiles(&mut report, context) {
        Ok(changed) => rust_changed |= changed,
        Err(error) => {
            report.fail(format!("{error:#}"));
            return report;
        }
    }
    if rust_changed {
        report.outcome = Outcome::Updated;
    }

    if !run_cargo_steps(&mut report, options) {
        return report;
    }

    if options.commit && !options.dryrun {
        commit_changes(&mut report, git, rust_changed, context);
    }
    report
}

/// Bump `rust-version` in the project manifest.
///
/// Returns whether the manifest changed.
fn bump_manifest(report: &mut Report, context: &RunContext) -> Result<bool> {
    let path = report.root.join("Cargo.toml");
    let original = fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    // Members that set their own rust-version instead of inheriting it are bumped too.
    let mut members_changed = false;
    let mut member_has_field = false;
    if has_workspace_table(&original) {
        for member in find_member_manifests(&report.root, context.roots) {
            match bump_member_manifest(report, context, &member)? {
                MemberUpdate::Updated => {
                    members_changed = true;
                    member_has_field = true;
                }
                MemberUpdate::Current => member_has_field = true,
                MemberUpdate::NoField => {}
            }
        }
    }
    let root_changed = bump_root_manifest(report, context, &path, &original, member_has_field)?;
    Ok(root_changed || members_changed)
}

/// Bump `rust-version` in the project root manifest.
///
/// A missing field is only reported when no member sets one either.
fn bump_root_manifest(
    report: &mut Report,
    context: &RunContext,
    path: &Path,
    original: &str,
    member_has_field: bool,
) -> Result<bool> {
    match update_rust_version(original, context.target)? {
        ManifestUpdate::Updated {
            old,
            new,
            content: updated,
        } => {
            report.push(format!("rust-version {} -> {}", old.yellow(), new.green()));
            if context.options.dryrun {
                for line in diff_lines("Cargo.toml", original, &updated) {
                    report.push(line);
                }
            } else {
                fs::write(path, updated).with_context(|| format!("Failed to write {}", path.display()))?;
            }
            Ok(true)
        }
        ManifestUpdate::Current(version) => {
            if context.options.verbose {
                report.push(format!("rust-version {version} is current").dimmed().to_string());
            }
            Ok(false)
        }
        ManifestUpdate::NoField => {
            if !member_has_field {
                report.missing_rust_version = true;
                report.push("No rust-version field".yellow().to_string());
            }
            Ok(false)
        }
    }
}

/// Bump `rust-version` in one workspace member manifest that sets its own value.
fn bump_member_manifest(report: &mut Report, context: &RunContext, path: &Path) -> Result<MemberUpdate> {
    let original = fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let relative = path.strip_prefix(&report.root).unwrap_or(path).display().to_string();
    match update_rust_version(&original, context.target)? {
        ManifestUpdate::Updated {
            old,
            new,
            content: updated,
        } => {
            report.push(format!("{relative}: rust-version {} -> {}", old.yellow(), new.green()));
            if context.options.dryrun {
                for line in diff_lines(&relative, &original, &updated) {
                    report.push(line);
                }
            } else {
                fs::write(path, updated).with_context(|| format!("Failed to write {}", path.display()))?;
            }
            Ok(MemberUpdate::Updated)
        }
        ManifestUpdate::Current(_) => Ok(MemberUpdate::Current),
        ManifestUpdate::NoField => Ok(MemberUpdate::NoField),
    }
}

/// Pin the `rust` images in the project Dockerfiles.
///
/// Returns whether any Dockerfile changed.
fn bump_dockerfiles(report: &mut Report, context: &RunContext) -> Result<bool> {
    let root = report.root.clone();
    let nested_roots: Vec<PathBuf> = context
        .roots
        .iter()
        .filter(|other| other.as_path() != root && other.starts_with(&root))
        .cloned()
        .collect();

    let resolve = |tag: &str| context.resolver.resolve(tag);
    let mut changed = false;
    for path in crate::discovery::find_dockerfiles(&root, &nested_roots) {
        let original = fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        let update = update_dockerfile(&original, context.target, &resolve);
        let display = get_relative_path_or_filename(&path, &root);
        for warning in &update.warnings {
            report.push(format!("{display}: {warning}").yellow().to_string());
        }
        if !update.is_changed() {
            continue;
        }
        changed = true;
        for change in &update.changes {
            report.push(format!(
                "{display}: {} -> {}",
                shorten_digest(&change.old).yellow(),
                shorten_digest(&change.new).green()
            ));
        }
        if context.options.dryrun {
            for line in diff_lines(&display, &original, &update.content) {
                report.push(line);
            }
        } else {
            fs::write(&path, &update.content).with_context(|| format!("Failed to write {}", path.display()))?;
        }
    }
    Ok(changed)
}

/// Run the optional cargo steps in order, stopping at the first failure.
///
/// In a dry run, the update step only reports what would change, and the other steps are listed.
/// Returns whether all steps succeeded.
fn run_cargo_steps(report: &mut Report, options: Options) -> bool {
    if options.update {
        let (upgrade_args, update_args): (&[&str], &[&str]) = if options.dryrun {
            (
                &["upgrade", "--incompatible", "allow", "--dry-run"],
                &["update", "--dry-run"],
            )
        } else {
            (&["upgrade", "--incompatible", "allow"], &["update"])
        };
        let show_output = options.verbose || options.dryrun;
        if !report.run_step("cargo", upgrade_args, show_output) || !report.run_step("cargo", update_args, show_output) {
            return false;
        }
    }

    if options.dryrun {
        if options.clean {
            report.push("Would run cargo clean".dimmed().to_string());
        }
        if options.check {
            report.push("Would run cargo clippy --fix and cargo test".dimmed().to_string());
        }
        return true;
    }

    if options.clean && !report.run_step("cargo", &["clean"], options.verbose) {
        return false;
    }
    if options.check {
        let clippy_args = [
            "clippy",
            "--fix",
            "--allow-dirty",
            "--allow-staged",
            "--workspace",
            "--all-targets",
        ];
        if !report.run_step("cargo", &clippy_args, options.verbose)
            || !report.run_step("cargo", &["test", "--workspace"], options.verbose)
        {
            return false;
        }
    }
    true
}

/// Commit the project changes when there are any.
fn commit_changes(report: &mut Report, git: GitState, rust_changed: bool, context: &RunContext) {
    if !git.is_repository {
        report.push("Not a git repository, nothing committed".dimmed().to_string());
        return;
    }
    if git.is_dirty {
        report.push(
            "Not committing, the repository had uncommitted changes before the update"
                .yellow()
                .to_string(),
        );
        return;
    }

    let Ok(_guard) = context.commit_lock.lock() else {
        report.fail("Commit lock poisoned");
        return;
    };
    let root = report.root.clone();
    match has_changes(&root, context.roots) {
        Ok(false) => {
            if context.options.verbose {
                report.push("Nothing to commit".dimmed().to_string());
            }
            return;
        }
        Ok(true) => {}
        Err(error) => {
            report.fail(format!("{error:#}"));
            return;
        }
    }

    let message = commit_message(context.target, rust_changed, context.options.update);
    let pathspecs = pathspecs(&root, context.roots);
    let mut add_args = vec!["add", "--all", "--"];
    add_args.extend(pathspecs.iter().map(String::as_str));
    let mut commit_args = vec!["commit", "--message", message.as_str(), "--"];
    commit_args.extend(pathspecs.iter().map(String::as_str));

    if report.run_step("git", &add_args, context.options.verbose)
        && report.run_step("git", &commit_args, context.options.verbose)
    {
        report.committed = true;
    }
}

/// Commit message describing what changed.
fn commit_message(target: RustVersion, rust_changed: bool, updated_dependencies: bool) -> String {
    let version = target.major_minor();
    match (rust_changed, updated_dependencies) {
        (true, true) => format!("Update Rust to {version} and dependencies"),
        (true, false) => format!("Update Rust to {version}"),
        (false, true) => "Update dependencies".to_string(),
        (false, false) => "Apply clippy fixes".to_string(),
    }
}

/// Check if the project has uncommitted changes, ignoring nested projects.
fn has_changes(root: &Path, roots: &[PathBuf]) -> Result<bool> {
    let pathspecs = pathspecs(root, roots);
    let mut args = vec!["status", "--porcelain", "--"];
    args.extend(pathspecs.iter().map(String::as_str));
    let result = run_command(root, "git", &args)?;
    if !result.success {
        anyhow::bail!("git status failed: {}", result.output.trim());
    }
    Ok(!result.output.trim().is_empty())
}

/// Git pathspecs covering the project directory without the nested project roots.
fn pathspecs(root: &Path, roots: &[PathBuf]) -> Vec<String> {
    let mut specs = vec![".".to_string()];
    specs.extend(
        roots
            .iter()
            .filter(|other| other.as_path() != root)
            .filter_map(|other| other.strip_prefix(root).ok())
            .map(|relative| format!(":(exclude){}", relative.display())),
    );
    specs
}

/// Shorten a digest in an image reference for display.
fn shorten_digest(reference: &str) -> String {
    reference.split_once("@sha256:").map_or_else(
        || reference.to_string(),
        |(image, digest)| {
            let short: String = digest.chars().take(12).collect();
            format!("{image}@sha256:{short}…")
        },
    )
}

/// Run a command in the given directory, capturing stdout and stderr together.
fn run_command(directory: &Path, program: &str, args: &[&str]) -> Result<CommandResult> {
    let mut command = Command::new(program);
    command.args(args).current_dir(directory);
    if program == "cargo" && std::io::stdout().is_terminal() {
        command.env("CARGO_TERM_COLOR", "always");
    }
    let output = command
        .output()
        .with_context(|| format!("Failed to run {program} {}", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(CommandResult {
        success: output.status.success(),
        output: text,
    })
}

#[cfg(test)]
mod test_commit_message {
    use super::*;

    const TARGET: RustVersion = RustVersion::new(1, 100, 0);

    #[test]
    fn describes_changes() {
        assert_eq!(
            commit_message(TARGET, true, true),
            "Update Rust to 1.100 and dependencies"
        );
        assert_eq!(commit_message(TARGET, true, false), "Update Rust to 1.100");
        assert_eq!(commit_message(TARGET, false, true), "Update dependencies");
        assert_eq!(commit_message(TARGET, false, false), "Apply clippy fixes");
    }
}

#[cfg(test)]
mod test_pathspecs {
    use super::*;

    #[test]
    fn excludes_nested_roots() {
        let root = PathBuf::from("/dev/app");
        let roots = vec![
            PathBuf::from("/dev/app"),
            PathBuf::from("/dev/app/office/docker"),
            PathBuf::from("/dev/other"),
        ];
        assert_eq!(pathspecs(&root, &roots), vec![".", ":(exclude)office/docker"]);
    }
}

#[cfg(test)]
mod test_shorten_digest {
    use super::*;

    #[test]
    fn shortens_digest() {
        let reference = "rust:1-slim@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe";
        assert_eq!(shorten_digest(reference), "rust:1-slim@sha256:6f14b2b4b278…");
    }

    #[test]
    fn keeps_reference_without_digest() {
        assert_eq!(shorten_digest("rust:1.100"), "rust:1.100");
    }
}

#[cfg(test)]
mod test_git_state {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn plain_directory_is_not_a_repository() {
        let temp = TempDir::new().expect("temp dir");
        let state = GitState::read(temp.path(), &[]);
        assert_eq!(state, GitState::default());
    }
}

#[cfg(test)]
mod test_bump_manifest {
    use super::*;

    #[test]
    fn members_with_their_own_rust_version_are_bumped() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().canonicalize().expect("canonical directory");
        let write = |relative: &str, content: &str| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("create directories");
            fs::write(path, content).expect("write manifest");
        };
        write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nrust-version = \"1.90\"\n",
        );
        write(
            "crates/own/Cargo.toml",
            "[package]\nname = \"own\"\nrust-version = \"1.90\"\n",
        );
        let inherited = "[package]\nname = \"inherited\"\nrust-version.workspace = true\n";
        write("crates/inherited/Cargo.toml", inherited);
        let resolver = DigestResolver::new().expect("resolver");
        let roots = vec![root.clone()];
        let commit_lock = Mutex::new(());
        let context = RunContext {
            target: RustVersion::new(1, 100, 0),
            options: Options::default(),
            resolver: &resolver,
            roots: &roots,
            commit_lock: &commit_lock,
        };

        let report = process_project(&root, GitState::default(), &context);

        assert_eq!(report.outcome, Outcome::Updated);
        let own = fs::read_to_string(root.join("crates/own/Cargo.toml")).expect("read");
        assert!(own.contains("rust-version = \"1.100\""), "{own}");
        let root_manifest = fs::read_to_string(root.join("Cargo.toml")).expect("read");
        assert!(root_manifest.contains("rust-version = \"1.100\""), "{root_manifest}");
        assert_eq!(
            fs::read_to_string(root.join("crates/inherited/Cargo.toml")).expect("read"),
            inherited
        );
    }
}
