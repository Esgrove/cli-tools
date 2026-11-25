//! The `xtask.toml` task file: discovery, schema, and validation.
//!
//! The task file is searched for in the current directory and its parents,
//! stopping at the root of the git repository the current directory is in.
//! It declares project flags, named conditions, scopes, environment profiles, and the task graph.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::condition::{BUILTIN_CONDITIONS, Condition};

/// Default task file name searched for in the current directory and its parents.
pub const TASK_FILE_NAME: &str = "xtask.toml";

/// Short options used by the generic command line arguments, which task file flags cannot use.
pub const RESERVED_SHORT_FLAGS: &[char] = &['C', 'd', 'f', 'h', 'j', 'K', 'O', 'S', 'T', 'v', 'V'];

/// Long options used by the generic command line arguments, which task file flags cannot use.
pub const RESERVED_LONG_FLAGS: &[&str] = &[
    "color",
    "config",
    "dryrun",
    "fail-fast",
    "help",
    "jobs",
    "output",
    "scope",
    "tail-lines",
    "verbose",
    "version",
];

/// Platforms a flag can be limited to with `platforms`.
const PLATFORMS: &[&str] = &["macos", "linux", "windows"];

/// Names of the subcommands, which cannot be used as task IDs.
const SUBCOMMANDS: &[&str] = &["completion", "list"];

/// Required command entry supporting either `"tool"` or `{ name = "tool", hint = "..." }`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum RequiredCommandConfig {
    /// Bare command name using the default error message.
    Name(String),
    /// Command name paired with a custom installation hint.
    Detailed {
        /// Executable name on `PATH`.
        name: String,
        /// Custom message shown when the executable is absent.
        hint: String,
    },
}

/// Root structure deserialized from `xtask.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFile {
    /// Runner settings.
    #[serde(default)]
    pub settings: Settings,
    /// Project specific boolean command line flags keyed by long name.
    #[serde(default)]
    pub flags: BTreeMap<String, FlagConfig>,
    /// Named boolean expressions usable in task `when` filters.
    #[serde(default)]
    pub conditions: BTreeMap<String, String>,
    /// Scopes that include other scopes for `--scope`.
    #[serde(default)]
    pub scopes: BTreeMap<String, ScopeConfig>,
    /// Named sets of environment changes applied to tasks with `env_profile`.
    #[serde(default)]
    pub env_profiles: BTreeMap<String, EnvProfile>,
    /// Ordered list of task definitions.
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
}

/// The `[settings]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Log directory relative to the project root (default: `target/xtask-logs`).
    pub log_dir: Option<String>,
    /// Default maximum number of concurrent tasks.
    pub jobs: Option<usize>,
    /// Command shown in the re-run hint of a failed task (default: `xtask`).
    pub rerun_prefix: Option<String>,
    /// Project specific runner the generic `xtask` binary hands the run over to,
    /// for example `["cargo", "run", "--quiet", "--package", "xtask", "--"]`.
    #[serde(default)]
    pub runner: Vec<String>,
}

/// One `[flags.<name>]` entry.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlagConfig {
    /// Optional single character short option.
    pub short: Option<char>,
    /// Help text shown in `--help`.
    #[serde(default)]
    pub help: String,
    /// Other flags turned on together with this one.
    #[serde(default)]
    pub implies: Vec<String>,
    /// Flags that must also be on when this one is.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Platforms the flag is supported on (`macos`, `linux`, `windows`), empty for all.
    #[serde(default)]
    pub platforms: Vec<String>,
}

/// One `[scopes.<name>]` entry.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeConfig {
    /// Other scopes whose tasks also belong to this scope.
    #[serde(default)]
    pub includes: Vec<String>,
}

/// One `[env_profiles.<name>]` entry.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvProfile {
    /// Environment variables to set.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Environment variables to remove.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Declarative definition of one task in `xtask.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    /// Unique identifier for this task.
    pub id: String,
    /// Human readable description shown in status lines.
    pub label: String,
    /// Scheduling priority (1 = highest, runs first; larger values run later; default: 5).
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Hard prerequisite task IDs (must be active and pass before this task starts).
    #[serde(default)]
    pub deps: Vec<String>,
    /// Soft ordering dependencies (waited on only when the referenced task is active).
    #[serde(default)]
    pub after: Vec<String>,
    /// Arbitrary exclusive resource lock names held while this task runs.
    #[serde(default)]
    pub locks: Vec<String>,
    /// Scope tags for `--scope`.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Condition expression that must hold for this task to be included (default: always).
    pub when: Option<String>,
    /// Commands that must exist on `PATH` before executing.
    #[serde(default)]
    pub require_commands: Vec<RequiredCommandConfig>,
    /// Optional command name.
    /// If missing from `PATH`, the task is skipped instead of failing.
    pub skip_if_missing: Option<String>,
    /// Name of an `[env_profiles]` entry applied to every command step.
    pub env_profile: Option<String>,
    /// Extra environment variables set for every command step.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Extra summary lines printed when the task passes (for example, coverage report paths).
    #[serde(default)]
    pub notes: Vec<String>,
    /// Name of a builtin registered by a project specific runner (mutually exclusive with `command`/`commands`).
    pub builtin: Option<String>,
    /// Single command argv shorthand.
    pub command: Option<Vec<String>>,
    /// Ordered list of command steps executed sequentially inside this task.
    #[serde(default)]
    pub commands: Vec<CommandStepConfig>,
    /// Optional summary override for `--dryrun`.
    pub dry_run_summary: Option<String>,
}

/// One subprocess step inside a declarative task.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandStepConfig {
    /// Executable and arguments (`argv[0]` is the program).
    pub argv: Vec<String>,
    /// Optional working directory relative to the project root.
    pub cwd: Option<String>,
    /// Extra environment variables set for this step.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Command that combined stdout and stderr are piped through while keeping the step's exit status,
    /// for example `["xcbeautify"]`. Output is not piped when the command is missing.
    #[serde(default)]
    pub pipe_through: Vec<String>,
}

/// Task file location and the project root its relative paths resolve against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskFileLocation {
    /// Absolute path to the task file.
    pub path: PathBuf,
    /// Directory containing the task file.
    pub root: PathBuf,
}

impl TaskFile {
    /// Load and parse a task file from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or contains invalid TOML or unknown fields.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
        Self::from_toml_str(&raw).with_context(|| format!("Invalid task file {}", path.display()))
    }

    /// Parse a task file from a TOML string.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid TOML or unknown fields.
    pub fn from_toml_str(raw: &str) -> Result<Self> {
        Ok(toml::from_str(raw)?)
    }

    /// Check flags, conditions, scopes, environment profiles, and tasks for consistency.
    ///
    /// `builtins` lists the builtin names the running binary provides.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first problem found.
    pub fn validate(&self, builtins: &BTreeSet<String>) -> Result<()> {
        self.validate_flags()?;
        let conditions = self.parsed_conditions()?;
        Self::validate_condition_cycles(&conditions)?;
        for task in &self.tasks {
            self.validate_task(task, builtins)?;
        }
        Ok(())
    }

    /// Parse every named condition, checking that it only refers to known identifiers.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid expression, an unknown identifier, or a name clashing with a flag.
    pub fn parsed_conditions(&self) -> Result<BTreeMap<String, Condition>> {
        let mut parsed = BTreeMap::new();
        for (name, expression) in &self.conditions {
            if self.flags.contains_key(name) || BUILTIN_CONDITIONS.contains(&name.as_str()) {
                bail!("Condition '{name}' has the same name as a flag or a built-in condition");
            }
            let condition =
                Condition::parse(expression).with_context(|| format!("Invalid expression for condition '{name}'"))?;
            self.check_identifiers(&condition, &format!("condition '{name}'"))?;
            parsed.insert(name.clone(), condition);
        }
        Ok(parsed)
    }

    /// Every scope name that is declared in `[scopes]` or used by a task.
    #[must_use]
    pub fn known_scopes(&self) -> BTreeSet<&str> {
        self.scopes
            .iter()
            .flat_map(|(name, scope)| std::iter::once(name.as_str()).chain(scope.includes.iter().map(String::as_str)))
            .chain(
                self.tasks
                    .iter()
                    .flat_map(|task| task.scopes.iter().map(String::as_str)),
            )
            .collect()
    }

    /// The scope itself plus every scope it includes, directly or through other scopes.
    #[must_use]
    pub fn expand_scope(&self, scope: &str) -> BTreeSet<String> {
        let mut expanded = BTreeSet::new();
        let mut pending = vec![scope.to_owned()];
        while let Some(name) = pending.pop() {
            if expanded.insert(name.clone())
                && let Some(config) = self.scopes.get(&name)
            {
                pending.extend(config.includes.iter().cloned());
            }
        }
        expanded
    }

    fn validate_flags(&self) -> Result<()> {
        let mut shorts = BTreeMap::new();
        for (name, flag) in &self.flags {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || name.starts_with('-')
            {
                bail!("Flag name '{name}' must contain only letters, digits, and dashes");
            }
            if RESERVED_LONG_FLAGS.contains(&name.as_str()) {
                bail!("Flag '--{name}' is reserved by xtask");
            }
            if BUILTIN_CONDITIONS.contains(&name.as_str()) {
                bail!("Flag '{name}' has the same name as a built-in condition");
            }
            if let Some(short) = flag.short {
                if !short.is_ascii_alphanumeric() {
                    bail!("Short option '{short}' of flag '{name}' must be a letter or digit");
                }
                if RESERVED_SHORT_FLAGS.contains(&short) {
                    bail!("Short option '-{short}' of flag '{name}' is reserved by xtask");
                }
                if let Some(other) = shorts.insert(short, name) {
                    bail!("Flags '{other}' and '{name}' both use the short option '-{short}'");
                }
            }
            for referenced in flag.implies.iter().chain(&flag.requires) {
                if !self.flags.contains_key(referenced) {
                    bail!("Flag '{name}' refers to unknown flag '{referenced}'");
                }
            }
            for platform in &flag.platforms {
                if !PLATFORMS.contains(&platform.as_str()) {
                    bail!(
                        "Flag '{name}' lists unknown platform '{platform}'. Supported: {}",
                        PLATFORMS.join(", ")
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_condition_cycles(conditions: &BTreeMap<String, Condition>) -> Result<()> {
        fn visit<'a>(
            name: &'a str,
            conditions: &'a BTreeMap<String, Condition>,
            visiting: &mut Vec<&'a str>,
            done: &mut BTreeSet<&'a str>,
        ) -> Result<()> {
            if done.contains(name) {
                return Ok(());
            }
            if visiting.contains(&name) {
                bail!(
                    "Conditions refer to each other in a cycle: {} -> {name}",
                    visiting.join(" -> ")
                );
            }
            if let Some(condition) = conditions.get(name) {
                visiting.push(name);
                for identifier in condition.identifiers() {
                    visit(identifier, conditions, visiting, done)?;
                }
                visiting.pop();
            }
            done.insert(name);
            Ok(())
        }

        let mut done = BTreeSet::new();
        for name in conditions.keys() {
            visit(name, conditions, &mut Vec::new(), &mut done)?;
        }
        Ok(())
    }

    fn validate_task(&self, task: &TaskConfig, builtins: &BTreeSet<String>) -> Result<()> {
        if SUBCOMMANDS.contains(&task.id.as_str()) {
            bail!(
                "Task ID '{}' is reserved for the xtask subcommand of the same name",
                task.id
            );
        }
        if task.id.is_empty() || task.id.starts_with('-') {
            bail!("Task ID '{}' must not be empty or start with a dash", task.id);
        }
        // The ID names the task's log file, so it has to be a plain file name.
        if task.id.contains(['/', '\\']) || task.id == "." || task.id == ".." {
            bail!(
                "Task ID '{}' must not contain a path separator or be '.' or '..'",
                task.id
            );
        }
        if let Some(expression) = &task.when {
            let condition =
                Condition::parse(expression).with_context(|| format!("Invalid 'when' for task '{}'", task.id))?;
            self.check_identifiers(&condition, &format!("task '{}'", task.id))?;
        }
        if let Some(profile) = &task.env_profile
            && !self.env_profiles.contains_key(profile)
        {
            bail!("Task '{}' uses unknown env_profile '{profile}'", task.id);
        }
        if let Some(builtin) = &task.builtin
            && !builtins.contains(builtin)
        {
            if builtins.is_empty() {
                bail!(
                    "Task '{}' uses builtin '{builtin}', which needs the project's own runner. \
                     Set 'runner' in [settings] or run the project's xtask binary.",
                    task.id
                );
            }
            bail!(
                "Task '{}' uses unknown builtin '{builtin}'. Available: {}",
                task.id,
                builtins.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        task.resolved_steps()?;
        Ok(())
    }

    fn check_identifiers(&self, condition: &Condition, owner: &str) -> Result<()> {
        for identifier in condition.identifiers() {
            let known = self.flags.contains_key(identifier)
                || self.conditions.contains_key(identifier)
                || BUILTIN_CONDITIONS.contains(&identifier);
            if !known {
                bail!(
                    "Unknown identifier '{identifier}' in {owner}. \
                     Use a flag, a named condition, or one of: {}",
                    BUILTIN_CONDITIONS.join(", ")
                );
            }
        }
        Ok(())
    }
}

impl TaskConfig {
    /// Return all command steps for this task (normalizing `command` vs `commands`).
    ///
    /// # Errors
    ///
    /// Returns an error if both `builtin` and commands are set,
    /// or neither is set, or any step has empty `argv`.
    pub fn resolved_steps(&self) -> Result<Vec<CommandStepConfig>> {
        let mut steps = self.commands.clone();
        if let Some(argv) = &self.command {
            steps.insert(
                0,
                CommandStepConfig {
                    argv: argv.clone(),
                    ..CommandStepConfig::default()
                },
            );
        }

        if self.builtin.is_some() && !steps.is_empty() {
            bail!("Task '{}' cannot set both 'builtin' and 'command'/'commands'", self.id);
        }
        if self.builtin.is_none() && steps.is_empty() {
            bail!("Task '{}' must set either 'builtin', 'command', or 'commands'", self.id);
        }
        for step in &steps {
            if step.argv.is_empty() {
                bail!("Task '{}' has a command step with empty argv", self.id);
            }
        }
        Ok(steps)
    }
}

impl RequiredCommandConfig {
    /// Return the executable name and optional custom error hint.
    #[must_use]
    pub const fn as_parts(&self) -> (&str, Option<&str>) {
        match self {
            Self::Name(name) => (name.as_str(), None),
            Self::Detailed { name, hint } => (name.as_str(), Some(hint.as_str())),
        }
    }
}

impl TaskFileLocation {
    /// Location for an explicit task file path, relative to `current_dir` when not absolute.
    ///
    /// # Errors
    ///
    /// Returns an error if the file does not exist.
    pub fn from_path(path: &Path, current_dir: &Path) -> Result<Self> {
        let joined = current_dir.join(path);
        if !joined.is_file() {
            bail!("Task file {} does not exist", joined.display());
        }
        let absolute = dunce::canonicalize(&joined).unwrap_or(joined);
        let root = absolute
            .parent()
            .map_or_else(|| current_dir.to_path_buf(), Path::to_path_buf);
        Ok(Self { path: absolute, root })
    }
}

/// Find `xtask.toml` in `start` or its parents, stopping at the git repository root.
///
/// Outside a git repository only `start` itself is checked.
/// A `.git` directory or file (as in worktrees and submodules) marks the repository root.
#[must_use]
pub fn find_task_file(start: &Path) -> Option<TaskFileLocation> {
    let inside_repository = start.ancestors().any(|directory| directory.join(".git").exists());
    for directory in start.ancestors() {
        let candidate = directory.join(TASK_FILE_NAME);
        if candidate.is_file() {
            return Some(TaskFileLocation {
                path: candidate,
                root: directory.to_path_buf(),
            });
        }
        if !inside_repository || directory.join(".git").exists() {
            break;
        }
    }
    None
}

const fn default_priority() -> u32 {
    5
}

#[cfg(test)]
mod test_task_file_validation {
    use super::*;

    fn validate(raw: &str) -> Result<()> {
        TaskFile::from_toml_str(raw)?.validate(&BTreeSet::new())
    }

    #[test]
    fn accepts_complete_example() {
        let raw = r#"
            [settings]
            log_dir = "target/test-logs"
            rerun_prefix = "./test.sh"

            [flags.all]
            short = "a"
            help = "Run everything"
            implies = ["web"]

            [flags.web]
            short = "w"

            [flags.show-ui]
            requires = ["all"]
            platforms = ["macos"]

            [conditions]
            full-web = "web && !dryrun"

            [scopes.mobile]
            includes = ["ios", "android"]

            [env_profiles.isolated]
            set = { SQLX_OFFLINE = "true" }
            remove = ["DATABASE_URL"]

            [[tasks]]
            id = "fmt"
            label = "Formatting"
            when = "all || full-web"
            scopes = ["rust"]
            env_profile = "isolated"
            command = ["cargo", "fmt", "--check"]
        "#;
        validate(raw).expect("example should validate");
    }

    #[test]
    fn rejects_unknown_fields_and_old_schema_keys() {
        let raw = r#"
            [[tasks]]
            id = "a"
            label = "A"
            when_any = ["checks"]
            command = ["true"]
        "#;
        assert!(TaskFile::from_toml_str(raw).is_err());
    }

    #[test]
    fn rejects_reserved_and_duplicate_short_options() {
        assert!(validate("[flags.dry]\nshort = \"d\"").is_err());
        assert!(validate("[flags.a]\nshort = \"x\"\n[flags.b]\nshort = \"x\"").is_err());
        assert!(validate("[flags.jobs]").is_err());
        assert!(validate("[flags.macos]").is_err());
    }

    #[test]
    fn rejects_unknown_references() {
        assert!(validate("[flags.a]\nimplies = [\"b\"]").is_err());
        assert!(validate("[flags.a]\nplatforms = [\"beos\"]").is_err());
        assert!(validate("[conditions]\nx = \"missing\"").is_err());
        assert!(validate("[conditions]\nx = \"y\"\ny = \"x\"").is_err());
        let unknown_when = "[[tasks]]\nid = \"a\"\nlabel = \"A\"\nwhen = \"nope\"\ncommand = [\"true\"]";
        assert!(validate(unknown_when).is_err());
        let unknown_profile = "[[tasks]]\nid = \"a\"\nlabel = \"A\"\nenv_profile = \"x\"\ncommand = [\"true\"]";
        assert!(validate(unknown_profile).is_err());
    }

    #[test]
    fn rejects_builtins_without_a_project_runner() {
        let raw = "[[tasks]]\nid = \"a\"\nlabel = \"A\"\nbuiltin = \"prepare\"";
        let error = validate(raw).expect_err("builtin should need a runner");
        assert!(error.to_string().contains("project's own runner"));
        let builtins = BTreeSet::from(["prepare".to_owned()]);
        TaskFile::from_toml_str(raw)
            .expect("should parse")
            .validate(&builtins)
            .expect("registered builtin should validate");
    }

    #[test]
    fn rejects_reserved_task_id_and_invalid_steps() {
        assert!(validate("[[tasks]]\nid = \"completion\"\nlabel = \"C\"\ncommand = [\"true\"]").is_err());
        assert!(validate("[[tasks]]\nid = \"list\"\nlabel = \"L\"\ncommand = [\"true\"]").is_err());
        assert!(validate("[[tasks]]\nid = \"a\"\nlabel = \"A\"").is_err());
        assert!(validate("[[tasks]]\nid = \"a\"\nlabel = \"A\"\ncommand = []").is_err());
    }

    #[test]
    fn rejects_task_ids_that_are_not_plain_file_names() {
        for id in ["ns/sub", "ns\\\\sub", ".", ".."] {
            let raw = format!("[[tasks]]\nid = \"{id}\"\nlabel = \"L\"\ncommand = [\"true\"]");
            assert!(validate(&raw).is_err(), "{id}");
        }
        assert!(validate("[[tasks]]\nid = \"a.b-c_d\"\nlabel = \"L\"\ncommand = [\"true\"]").is_ok());
    }

    #[test]
    fn expands_nested_scopes() {
        let file = TaskFile::from_toml_str(
            "[scopes.all-mobile]\nincludes = [\"mobile\"]\n[scopes.mobile]\nincludes = [\"ios\", \"android\"]",
        )
        .expect("should parse");
        let expanded = file.expand_scope("all-mobile");
        assert_eq!(
            expanded.iter().map(String::as_str).collect::<Vec<_>>(),
            ["all-mobile", "android", "ios", "mobile"]
        );
    }
}

#[cfg(test)]
mod test_task_file_discovery {
    use super::*;

    fn write_task_file(directory: &Path) {
        std::fs::write(directory.join(TASK_FILE_NAME), "").expect("should write task file");
    }

    #[test]
    fn finds_file_in_parent_directory_inside_repository() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let root = temp.path();
        std::fs::create_dir(root.join(".git")).expect("should create .git");
        write_task_file(root);
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("should create nested dirs");

        let location = find_task_file(&nested).expect("should find task file");
        assert_eq!(location.root, root);
        assert_eq!(location.path, root.join(TASK_FILE_NAME));
    }

    #[test]
    fn prefers_nearest_file() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let root = temp.path();
        std::fs::create_dir(root.join(".git")).expect("should create .git");
        write_task_file(root);
        let nested = root.join("sub");
        std::fs::create_dir(&nested).expect("should create sub");
        write_task_file(&nested);

        assert_eq!(find_task_file(&nested).expect("should find").root, nested);
    }

    #[test]
    fn stops_at_repository_root() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        write_task_file(temp.path());
        let repository = temp.path().join("repo");
        let nested = repository.join("src");
        std::fs::create_dir_all(&nested).expect("should create dirs");
        std::fs::write(repository.join(".git"), "gitdir: elsewhere").expect("should write .git file");

        assert!(find_task_file(&nested).is_none());
    }

    #[test]
    fn checks_only_start_outside_repository() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        write_task_file(temp.path());
        let nested = temp.path().join("child");
        std::fs::create_dir(&nested).expect("should create child");

        assert!(find_task_file(&nested).is_none());
        assert_eq!(find_task_file(temp.path()).expect("should find").root, temp.path());
    }

    #[test]
    fn explicit_path_resolves_relative_to_current_dir() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let directory = dunce::canonicalize(temp.path()).expect("should canonicalize");
        let file = directory.join("custom.toml");
        std::fs::write(&file, "").expect("should write");

        let location = TaskFileLocation::from_path(Path::new("custom.toml"), &directory).expect("should resolve");
        assert_eq!(location.path, file);
        assert_eq!(location.root, directory);
        assert!(TaskFileLocation::from_path(Path::new("missing.toml"), &directory).is_err());
    }
}
