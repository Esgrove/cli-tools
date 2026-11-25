//! The update pipeline for one Rust project, and running it for many projects in parallel.
//!
//! Bumps `rust-version` and the `rust` Docker images,
//! then runs the optional cargo and git steps in order.
//! Each step is recorded as a [`ReportEntry`] in the project's [`Report`],
//! so projects processed in parallel do not interleave and callers decide how to show them.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;

use cli_tools_common::{command_exists, count_label, get_relative_path_or_filename};

use crate::discovery::{find_dockerfiles, find_member_manifests};
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

/// One thing that happened while processing a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportEntry {
    /// The project was skipped because its git repository has uncommitted changes.
    SkippedDirty,
    /// `rust-version` was raised in a manifest.
    RustVersionBumped {
        /// Manifest path relative to the project root.
        manifest: String,
        /// Version before the bump.
        old: String,
        /// Version after the bump.
        new: String,
    },
    /// The project manifest already has the target `rust-version` or a newer one.
    RustVersionCurrent(String),
    /// Neither the project manifest nor any workspace member sets `rust-version`.
    NoRustVersion,
    /// A file would change like this in a dry run.
    Diff {
        /// File path relative to the project root.
        path: String,
        /// Current contents.
        original: String,
        /// Contents after the change.
        updated: String,
    },
    /// A Dockerfile has something that needs attention.
    DockerfileWarning {
        /// Dockerfile path relative to the project root.
        path: String,
        /// What needs attention.
        warning: String,
    },
    /// A `rust` image in a Dockerfile was pinned to the target version.
    ImagePinned {
        /// Dockerfile path relative to the project root.
        path: String,
        /// Image reference before the change.
        old: String,
        /// Image reference after the change.
        new: String,
    },
    /// A command succeeded.
    StepSucceeded {
        /// The command line.
        label: String,
        /// Combined stdout and stderr.
        output: String,
        /// The output is the point of the step, as for a dry run dependency upgrade.
        output_is_result: bool,
    },
    /// A command ran and failed.
    StepFailed {
        /// The command line.
        label: String,
        /// Combined stdout and stderr.
        output: String,
    },
    /// A step could not be done.
    Failed(String),
    /// A step that a dry run does not run.
    WouldRun(String),
    /// Nothing is committed because the project is not in a git repository.
    NotARepository,
    /// Nothing is committed because the repository had uncommitted changes before the update.
    NotCommittingDirty,
    /// The update changed nothing, so there is nothing to commit.
    NothingToCommit,
}

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

/// Options that control which steps run.
#[derive(Debug, Clone, Copy, Default)]
#[expect(clippy::struct_excessive_bools, reason = "each step is switched on independently")]
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
    /// Only record what would change, without changing anything.
    pub dry_run: bool,
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
    /// The steps in the order they happened.
    pub entries: Vec<ReportEntry>,
}

/// Output of one external command.
struct CommandResult {
    /// The command exited successfully.
    success: bool,
    /// Combined stdout and stderr.
    output: String,
}

/// Receives each project report as soon as the project is done.
///
/// Projects are processed in parallel, so the method can be called from several threads at once.
/// The method does nothing by default, and `()` reports nothing.
pub trait BumpObserver: Sync {
    /// One project finished.
    fn on_project_finished(&self, _report: &Report) {}
}

impl BumpObserver for () {}

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
            entries: Vec::new(),
        }
    }

    /// Return whether the project failed.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        matches!(self.outcome, Outcome::Failed(_))
    }

    /// Record an entry.
    fn push(&mut self, entry: ReportEntry) {
        self.entries.push(entry);
    }

    /// Mark the project failed with a message.
    fn fail(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.push(ReportEntry::Failed(message.clone()));
        self.outcome = Outcome::Failed(message);
    }

    /// Run a command as a step, recording its result.
    ///
    /// Returns whether the step succeeded.
    fn run_step(&mut self, program: &str, args: &[&str], output_is_result: bool) -> bool {
        let label = format!("{program} {}", args.join(" "));
        match run_command(&self.root.clone(), program, args) {
            Ok(result) if result.success => {
                self.push(ReportEntry::StepSucceeded {
                    label,
                    output: result.output,
                    output_is_result,
                });
                true
            }
            Ok(result) => {
                self.outcome = Outcome::Failed(format!("{label} failed"));
                self.push(ReportEntry::StepFailed {
                    label,
                    output: result.output,
                });
                false
            }
            Err(error) => {
                self.fail(format!("{error:#}"));
                false
            }
        }
    }
}

/// Check that the external tools the options need are installed.
///
/// # Errors
/// Returns an error naming the missing tool and how to install it.
pub fn check_tools(options: Options) -> Result<()> {
    if options.update && !command_exists("cargo-upgrade") {
        anyhow::bail!("cargo upgrade not found, install it with: cargo install cargo-edit");
    }
    Ok(())
}

/// Process every project root in parallel and return the reports in the order of `roots`.
///
/// Git states are read for every project before any project changes,
/// so one project's update cannot make a project in the same repository look dirty.
/// The observer gets each report as soon as its project is done.
///
/// # Errors
/// Returns an error if a needed tool is missing,
/// or the thread pool or the Docker Hub client cannot be created.
pub fn bump_projects(
    roots: &[PathBuf],
    target: RustVersion,
    options: Options,
    jobs: usize,
    observer: &impl BumpObserver,
) -> Result<Vec<Report>> {
    check_tools(options)?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .context("Failed to build thread pool")?;

    let git_states: Vec<GitState> = pool.install(|| roots.par_iter().map(|root| GitState::read(root, roots)).collect());

    let resolver = DigestResolver::new()?;
    let commit_lock = Mutex::new(());
    let context = RunContext {
        target,
        options,
        resolver: &resolver,
        roots,
        commit_lock: &commit_lock,
    };

    Ok(pool.install(|| {
        roots
            .par_iter()
            .zip(git_states.par_iter())
            .map(|(root, git)| {
                let report = process_project(root, *git, &context);
                observer.on_project_finished(&report);
                report
            })
            .collect()
    }))
}

/// Return an error naming how many projects failed, if any did.
///
/// # Errors
/// Returns an error when at least one report failed.
pub fn ensure_none_failed(reports: &[Report]) -> Result<()> {
    let failed = reports.iter().filter(|report| report.is_failed()).count();
    if failed > 0 {
        anyhow::bail!("{} failed", count_label(failed, "project", "projects"));
    }
    Ok(())
}

/// Run every step for one project.
#[must_use]
pub fn process_project(root: &Path, git: GitState, context: &RunContext) -> Report {
    let options = context.options;
    let mut report = Report::new(root.to_path_buf());

    if git.is_dirty && !options.force {
        report.push(ReportEntry::SkippedDirty);
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

    if options.commit && !options.dry_run {
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
            report.push(ReportEntry::RustVersionBumped {
                manifest: "Cargo.toml".to_string(),
                old,
                new,
            });
            write_or_record_diff(report, context.options, path, "Cargo.toml", original, updated)?;
            Ok(true)
        }
        ManifestUpdate::Current(version) => {
            report.push(ReportEntry::RustVersionCurrent(version));
            Ok(false)
        }
        ManifestUpdate::NoField => {
            if !member_has_field {
                report.missing_rust_version = true;
                report.push(ReportEntry::NoRustVersion);
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
            report.push(ReportEntry::RustVersionBumped {
                manifest: relative.clone(),
                old,
                new,
            });
            write_or_record_diff(report, context.options, path, &relative, &original, updated)?;
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
    for path in find_dockerfiles(&root, &nested_roots) {
        let original = fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
        let update = update_dockerfile(&original, context.target, &resolve);
        let display = get_relative_path_or_filename(&path, &root);
        for warning in &update.warnings {
            report.push(ReportEntry::DockerfileWarning {
                path: display.clone(),
                warning: warning.clone(),
            });
        }
        if !update.is_changed() {
            continue;
        }
        changed = true;
        for change in &update.changes {
            report.push(ReportEntry::ImagePinned {
                path: display.clone(),
                old: change.old.clone(),
                new: change.new.clone(),
            });
        }
        write_or_record_diff(report, context.options, &path, &display, &original, update.content)?;
    }
    Ok(changed)
}

/// Write the updated contents, or record them as a diff in a dry run.
fn write_or_record_diff(
    report: &mut Report,
    options: Options,
    path: &Path,
    display: &str,
    original: &str,
    updated: String,
) -> Result<()> {
    if options.dry_run {
        report.push(ReportEntry::Diff {
            path: display.to_string(),
            original: original.to_string(),
            updated,
        });
    } else {
        fs::write(path, updated).with_context(|| format!("Failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Run the optional cargo steps in order, stopping at the first failure.
///
/// In a dry run, the update step only reports what would change, and the other steps are listed.
/// Returns whether all steps succeeded.
fn run_cargo_steps(report: &mut Report, options: Options) -> bool {
    if options.update {
        let (upgrade_args, update_args): (&[&str], &[&str]) = if options.dry_run {
            (
                &["upgrade", "--incompatible", "allow", "--dry-run"],
                &["update", "--dry-run"],
            )
        } else {
            (&["upgrade", "--incompatible", "allow"], &["update"])
        };
        if !report.run_step("cargo", upgrade_args, options.dry_run)
            || !report.run_step("cargo", update_args, options.dry_run)
        {
            return false;
        }
    }

    if options.dry_run {
        if options.clean {
            report.push(ReportEntry::WouldRun("cargo clean".to_string()));
        }
        if options.check {
            report.push(ReportEntry::WouldRun("cargo clippy --fix and cargo test".to_string()));
        }
        return true;
    }

    if options.clean && !report.run_step("cargo", &["clean"], false) {
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
        if !report.run_step("cargo", &clippy_args, false) || !report.run_step("cargo", &["test", "--workspace"], false)
        {
            return false;
        }
    }
    true
}

/// Commit the project changes when there are any.
fn commit_changes(report: &mut Report, git: GitState, rust_changed: bool, context: &RunContext) {
    if !git.is_repository {
        report.push(ReportEntry::NotARepository);
        return;
    }
    if git.is_dirty {
        report.push(ReportEntry::NotCommittingDirty);
        return;
    }

    let Ok(_guard) = context.commit_lock.lock() else {
        report.fail("Commit lock poisoned");
        return;
    };
    let root = report.root.clone();
    match has_changes(&root, context.roots) {
        Ok(false) => {
            report.push(ReportEntry::NothingToCommit);
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

    if report.run_step("git", &add_args, false) && report.run_step("git", &commit_args, false) {
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
        assert!(report.entries.contains(&ReportEntry::RustVersionBumped {
            manifest: "crates/own/Cargo.toml".to_string(),
            old: "1.90".to_string(),
            new: "1.100".to_string(),
        }));
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
