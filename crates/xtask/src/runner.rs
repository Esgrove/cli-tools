//! Turn a validated task file into scheduled tasks and run them.
//!
//! Evaluates each task's `when` condition against the resolved flags,
//! expands `${NAME}` variables in command steps,
//! sets the `XTASK_*` environment for every command,
//! and collects the variables and notes tasks export through the `XTASK_ENV` and `XTASK_NOTES` files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use crate::builtin::Builtin;
use crate::cli::{EnvSnapshot, RunOptions};
use crate::condition::Condition;
use crate::config::{CommandStepConfig, EnvProfile, TaskConfig, TaskFile, TaskFileLocation};
use crate::process::{CommandSpec, command_exists, require_command};
use crate::scheduler::{
    SharedState, TaskAction, TaskActionResult, TaskContext, TaskOutcome, TaskSpec, filter_tasks, print_task_list,
    run_schedule, validate_graph,
};
use crate::ui::{Reporter, UiConfig};

/// Default log directory relative to the project root.
const DEFAULT_LOG_DIR: &str = "target/xtask-logs";

/// Everything needed to evaluate conditions for one run.
struct ConditionContext<'a> {
    flags: &'a BTreeMap<String, bool>,
    conditions: BTreeMap<String, Condition>,
    dry_run: bool,
    is_ci: bool,
}

/// Paths of the files a command task writes exported variables and notes to.
struct ExportFiles {
    env: PathBuf,
    notes: PathBuf,
}

impl ConditionContext<'_> {
    /// Whether a task with the given `when` expression is enabled.
    fn is_enabled(&self, when: Option<&str>) -> Result<bool> {
        let Some(expression) = when else {
            return Ok(true);
        };
        let condition = Condition::parse(expression)?;
        self.evaluate(&condition, 0)
    }

    fn evaluate(&self, condition: &Condition, depth: usize) -> Result<bool> {
        if depth > self.conditions.len() {
            bail!("Conditions refer to each other in a cycle");
        }
        condition.evaluate(&mut |name| self.resolve(name, depth))
    }

    fn resolve(&self, name: &str, depth: usize) -> Result<bool> {
        if let Some(enabled) = self.flags.get(name) {
            return Ok(*enabled);
        }
        if let Some(condition) = self.conditions.get(name) {
            return self.evaluate(condition, depth.saturating_add(1));
        }
        match name {
            "macos" => Ok(cfg!(target_os = "macos")),
            "linux" => Ok(cfg!(target_os = "linux")),
            "windows" => Ok(cfg!(windows)),
            "unix" => Ok(cfg!(unix)),
            "ci" => Ok(self.is_ci),
            "dryrun" => Ok(self.dry_run),
            other => bail!("Unknown condition '{other}'"),
        }
    }
}

impl ExportFiles {
    /// Export files stored next to the task's log file.
    fn for_log(log_path: &Path) -> Self {
        Self {
            env: log_path.with_extension("env"),
            notes: log_path.with_extension("notes"),
        }
    }

    /// Remove files left by an earlier run with the same log path.
    fn reset(&self) -> Result<()> {
        for path in [&self.env, &self.notes] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("Failed to remove {}", path.display())),
            }
        }
        Ok(())
    }

    /// Read the exported variables and notes into the shared state.
    ///
    /// Each `KEY=VALUE` line of the env file adds a value, so repeating a key makes a list variable.
    fn collect(&self, context: &TaskContext) -> Result<()> {
        if let Some(content) = read_optional(&self.env)? {
            let mut exported: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for line in content.lines().filter(|line| !line.trim().is_empty()) {
                let Some((key, value)) = line.split_once('=') else {
                    bail!(
                        "Invalid line in {}: expected KEY=VALUE, got '{line}'",
                        self.env.display()
                    );
                };
                exported
                    .entry(key.trim().to_owned())
                    .or_default()
                    .push(value.to_owned());
            }
            for (key, values) in exported {
                context.set_variable(key, values);
            }
        }
        if let Some(content) = read_optional(&self.notes)? {
            for line in content.lines().filter(|line| !line.trim().is_empty()) {
                context.add_note(line.to_owned());
            }
        }
        Ok(())
    }
}

/// Run the task file with the resolved options.
///
/// Returns whether no task failed and no explicitly requested task was blocked.
///
/// # Errors
///
/// Returns an error if conditions cannot be evaluated,
/// a task ID or scope is unknown, or the graph is invalid.
pub fn run(
    task_file: &TaskFile,
    location: &TaskFileLocation,
    options: &RunOptions,
    builtins: &BTreeMap<String, Arc<dyn Builtin>>,
    variables: BTreeMap<String, Vec<String>>,
) -> Result<bool> {
    let env = EnvSnapshot::from_process();
    let root = &location.root;
    let tasks = plan(task_file, location, options, builtins, env.is_ci())?;

    if options.list_tasks {
        print_task_list(&tasks);
        return Ok(true);
    }

    let run_id = next_run_id();
    let log_dir = root
        .join(task_file.settings.log_dir.as_deref().unwrap_or(DEFAULT_LOG_DIR))
        .join(run_id);
    let reporter = Arc::new(Reporter::from_config(UiConfig::from_options(options, &env)));
    let context = TaskContext {
        repo_root: root.clone(),
        log_dir,
        dry_run: options.dry_run,
        verbose: options.verbose,
        fail_fast: options.fail_fast,
        flags: Arc::new(options.flags.clone()),
        task_id: None,
        reporter: Arc::clone(&reporter),
        shared: Arc::new(Mutex::new(SharedState {
            variables,
            notes: Vec::new(),
        })),
    };

    let scope_label = options.scope.as_deref().unwrap_or("all");
    let summary = run_schedule(tasks, &context, options.jobs, options.fail_fast, scope_label)?;
    reporter.finish_and_print_summary(&summary, root, options.dry_run);
    let requested_task_blocked = summary
        .reports
        .iter()
        .any(|report| matches!(report.outcome, TaskOutcome::Blocked(_)) && options.task_ids.contains(&report.id));
    Ok(summary.is_success() && !requested_task_blocked)
}

/// The tasks a run would execute, in task file order, after applying conditions, the scope, and task IDs.
///
/// # Errors
///
/// Returns an error if conditions cannot be evaluated, a task ID or scope is unknown,
/// or the selected dependency graph is invalid.
pub fn plan(
    task_file: &TaskFile,
    location: &TaskFileLocation,
    options: &RunOptions,
    builtins: &BTreeMap<String, Arc<dyn Builtin>>,
    is_ci: bool,
) -> Result<Vec<TaskSpec>> {
    let condition_context = ConditionContext {
        flags: &options.flags,
        conditions: task_file.parsed_conditions()?,
        dry_run: options.dry_run,
        is_ci,
    };
    let enabled_tasks = build_tasks(task_file, &condition_context, &location.root, builtins)?;
    let selected_scopes = resolve_scope(task_file, options.scope.as_deref())?;
    let mut selected = filter_tasks(enabled_tasks, selected_scopes.as_ref(), &options.task_ids)?;
    resolve_selected_dependencies(task_file, &mut selected)?;
    validate_graph(&selected)?;
    Ok(selected)
}

/// Generate a unique log directory name across rapid or concurrent runs.
fn next_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
    format!("{stamp}-{}-{sequence}", std::process::id())
}

/// Name of the operating system as used by flag `platforms` and the built-in conditions.
#[must_use]
pub const fn current_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// Environment variable name for a project flag, for example `XTASK_FLAG_SIGN_IN` for `sign-in`.
#[must_use]
pub fn flag_env_name(flag: &str) -> String {
    format!("XTASK_FLAG_{}", flag.to_ascii_uppercase().replace('-', "_"))
}

/// Build the enabled tasks from the task file.
fn build_tasks(
    task_file: &TaskFile,
    conditions: &ConditionContext<'_>,
    root: &Path,
    builtins: &BTreeMap<String, Arc<dyn Builtin>>,
) -> Result<Vec<TaskSpec>> {
    let mut specs = Vec::new();
    for task in &task_file.tasks {
        let is_enabled = conditions
            .is_enabled(task.when.as_deref())
            .with_context(|| format!("Failed to evaluate 'when' of task '{}'", task.id))?;
        if !is_enabled {
            continue;
        }
        let steps = task.resolved_steps()?;
        let action = build_action(task, steps.clone(), task_file, conditions.flags, root, builtins)?;
        let dry_run_summary = task.dry_run_summary.clone().unwrap_or_else(|| {
            task.builtin
                .as_ref()
                .map_or_else(|| summarize_steps(&steps), |name| format!("builtin {name}"))
        });

        specs.push(TaskSpec {
            id: task.id.clone(),
            label: task.label.clone(),
            priority: task.priority,
            deps: task.deps.clone(),
            after: task.after.clone(),
            resources: task.locks.clone(),
            scopes: task.scopes.clone(),
            dry_run_summary,
            action,
        });
    }
    Ok(specs)
}

/// Reject invalid dependencies in the selected closure and drop ordering outside it.
fn resolve_selected_dependencies(task_file: &TaskFile, selected: &mut [TaskSpec]) -> Result<()> {
    let declared_ids: BTreeSet<&str> = task_file.tasks.iter().map(|task| task.id.as_str()).collect();
    let selected_ids: BTreeSet<String> = selected.iter().map(|task| task.id.clone()).collect();
    for task in selected {
        for dependency in &task.deps {
            if !selected_ids.contains(dependency) && declared_ids.contains(dependency.as_str()) {
                bail!(
                    "Task '{}' depends on '{dependency}', which is not enabled for the current flags. \
                     Use 'after' for an ordering that only applies when both tasks run.",
                    task.id
                );
            }
        }
        for predecessor in &task.after {
            if !declared_ids.contains(predecessor.as_str()) {
                bail!("Task '{}' orders after unknown task '{predecessor}'.", task.id);
            }
        }
        task.after
            .retain(|id| selected_ids.contains(id) && !task.deps.contains(id));
    }
    Ok(())
}

/// Build the action closure for a builtin or command task.
fn build_action(
    task: &TaskConfig,
    steps: Vec<CommandStepConfig>,
    task_file: &TaskFile,
    flags: &BTreeMap<String, bool>,
    root: &Path,
    builtins: &BTreeMap<String, Arc<dyn Builtin>>,
) -> Result<TaskAction> {
    let action: TaskAction = if let Some(name) = &task.builtin {
        let Some(builtin) = builtins.get(name) else {
            bail!("Task '{}' uses unknown builtin '{name}'", task.id);
        };
        let builtin = Arc::clone(builtin);
        Box::new(move |context, log_path| builtin.run(context, log_path))
    } else {
        build_command_action(task, steps, task_file, flags, root)?
    };
    let require_commands = task.require_commands.clone();
    let skip_if_missing = task.skip_if_missing.clone();
    Ok(Box::new(move |context, log_path| {
        if !context.dry_run {
            if let Some(optional_command) = &skip_if_missing
                && !command_exists(optional_command)
            {
                return Ok(TaskActionResult::Skipped(format!("{optional_command} not found")));
            }
            for requirement in &require_commands {
                let (name, hint) = requirement.as_parts();
                require_command(name, hint)?;
            }
        }
        action(context, log_path)
    }))
}

/// Build a command task action that runs its steps and collects their exports.
fn build_command_action(
    task: &TaskConfig,
    steps: Vec<CommandStepConfig>,
    task_file: &TaskFile,
    flags: &BTreeMap<String, bool>,
    root: &Path,
) -> Result<TaskAction> {
    let profile = match &task.env_profile {
        Some(name) => task_file
            .env_profiles
            .get(name)
            .cloned()
            .with_context(|| format!("Task '{}' uses unknown env_profile '{name}'", task.id))?,
        None => EnvProfile::default(),
    };
    let task_env = task.env.clone();
    let notes = task.notes.clone();
    let flag_env: Vec<(String, String)> = flags
        .iter()
        .map(|(name, enabled)| (flag_env_name(name), if *enabled { "1" } else { "0" }.to_owned()))
        .collect();
    let root = root.to_path_buf();

    Ok(Box::new(move |context, log_path| {
        if context.dry_run {
            for step in &steps {
                let _ = materialize_step(step, context, &root, &profile, &task_env)?;
            }
            return Ok(TaskActionResult::DryRun);
        }

        let exports = ExportFiles::for_log(log_path);
        exports.reset()?;
        for step in &steps {
            let mut spec = materialize_step(step, context, &root, &profile, &task_env)?;
            spec = spec
                .env("XTASK_ENV", exports.env.to_string_lossy())
                .env("XTASK_NOTES", exports.notes.to_string_lossy());
            for (key, value) in &flag_env {
                spec = spec.env(key, value);
            }
            context.run_logged(&spec, log_path)?;
            exports.collect(context)?;
            exports.reset()?;
        }

        for note in &notes {
            context.add_note(note.clone());
        }
        Ok(TaskActionResult::Passed)
    }))
}

/// Validate the requested scope and return it together with every scope it includes.
fn resolve_scope(task_file: &TaskFile, scope: Option<&str>) -> Result<Option<BTreeSet<String>>> {
    let Some(scope) = scope.filter(|scope| *scope != "all") else {
        return Ok(None);
    };
    let known = task_file.known_scopes();
    if !known.contains(scope) {
        bail!(
            "Unknown scope '{scope}'. Available: all, {}",
            known.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(Some(task_file.expand_scope(scope)))
}

/// Build the command for one step with expanded variables and the task environment.
fn materialize_step(
    step: &CommandStepConfig,
    context: &TaskContext,
    root: &Path,
    profile: &EnvProfile,
    task_env: &BTreeMap<String, String>,
) -> Result<CommandSpec> {
    let mut argv = Vec::with_capacity(step.argv.len());
    for argument in &step.argv {
        if let Some(name) = whole_variable_reference(argument) {
            argv.extend(lookup_variable(name, context)?);
        } else {
            argv.push(expand_variables(argument, context)?);
        }
    }
    let Some((program, rest)) = argv.split_first() else {
        bail!("Command step has empty argv");
    };
    let cwd = match step.cwd.as_deref() {
        Some(relative) => root.join(expand_variables(relative, context)?),
        None => root.to_path_buf(),
    };

    let mut spec = CommandSpec::new(program, cwd).args(rest.iter().cloned());
    for key in &profile.remove {
        spec = spec.env_remove(key);
    }
    for (key, value) in profile.set.iter().chain(task_env).chain(&step.env) {
        spec = spec.env(key, expand_variables(value, context)?);
    }
    spec = spec
        .env("XTASK_ROOT", root.to_string_lossy())
        .env("XTASK_LOG_DIR", context.log_dir.to_string_lossy())
        .env("XTASK_TASK_ID", context.task_id.clone().unwrap_or_default())
        .env("XTASK_DRYRUN", if context.dry_run { "1" } else { "0" });
    if !step.pipe_through.is_empty() {
        let mut expanded_pipe = Vec::with_capacity(step.pipe_through.len());
        for argument in &step.pipe_through {
            if let Some(name) = whole_variable_reference(argument) {
                expanded_pipe.extend(lookup_variable(name, context)?);
            } else {
                expanded_pipe.push(expand_variables(argument, context)?);
            }
        }
        spec = spec.with_pipe_through(expanded_pipe);
    }
    Ok(spec)
}

/// The variable name when the whole argument is a single `${NAME}` reference.
fn whole_variable_reference(argument: &str) -> Option<&str> {
    argument
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .filter(|name| !name.is_empty() && name.chars().all(is_variable_character))
}

/// Replace every `${NAME}` in `raw`, joining list values with spaces.
fn expand_variables(raw: &str, context: &TaskContext) -> Result<String> {
    let mut output = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        let (before, after) = rest.split_at(start);
        output.push_str(before);
        let reference = after.get(2..).unwrap_or_default();
        let Some(end) = reference.find('}') else {
            bail!("Unclosed variable reference in '{raw}'");
        };
        let name = reference.get(..end).unwrap_or_default();
        if name.is_empty() || !name.chars().all(is_variable_character) {
            bail!("Invalid variable name '{name}' in '{raw}'");
        }
        output.push_str(&lookup_variable(name, context)?.join(" "));
        rest = reference.get(end.saturating_add(1)..).unwrap_or_default();
    }
    output.push_str(rest);
    Ok(output)
}

/// Values of a variable from the shared state, falling back to the process environment.
fn lookup_variable(name: &str, context: &TaskContext) -> Result<Vec<String>> {
    if let Some(values) = context.variable(name) {
        return Ok(values);
    }
    if let Ok(value) = std::env::var(name) {
        return Ok(vec![value]);
    }
    bail!(
        "Variable ${{{name}}} is not set. Add the task that exports it as a dependency, \
         or set it in the environment."
    )
}

/// Return whether `character` is allowed in a `${NAME}` variable reference.
const fn is_variable_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// Read a file that may not exist.
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

/// One line summary of the command steps for `--dryrun` and `list`.
fn summarize_steps(steps: &[CommandStepConfig]) -> String {
    steps
        .iter()
        .map(|step| {
            let line = step.argv.join(" ");
            if step.pipe_through.is_empty() {
                line
            } else {
                format!("{line} 2>&1 | {}", step.pipe_through.join(" "))
            }
        })
        .collect::<Vec<_>>()
        .join(" && ")
}

#[cfg(test)]
mod test_helpers {
    use super::*;

    pub fn context_with_variables(variables: &[(&str, &[&str])]) -> TaskContext {
        let shared = SharedState {
            variables: variables
                .iter()
                .map(|(name, values)| {
                    (
                        (*name).to_owned(),
                        values.iter().map(|value| (*value).to_owned()).collect(),
                    )
                })
                .collect(),
            notes: Vec::new(),
        };
        TaskContext {
            repo_root: PathBuf::from("/project"),
            log_dir: PathBuf::from("/project/target/xtask-logs/run"),
            dry_run: false,
            verbose: false,
            fail_fast: false,
            flags: Arc::new(BTreeMap::new()),
            task_id: Some("task".to_owned()),
            reporter: Arc::new(Reporter::silent()),
            shared: Arc::new(Mutex::new(shared)),
        }
    }
}

#[cfg(test)]
mod test_variable_expansion {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn expands_inline_and_list_variables() {
        let context = context_with_variables(&[("SERIAL", &["emulator-5554"]), ("TARGETS", &["a.js", "b.js"])]);
        let step = CommandStepConfig {
            argv: vec![
                "tool".to_owned(),
                "--serial=${SERIAL}".to_owned(),
                "${TARGETS}".to_owned(),
            ],
            ..CommandStepConfig::default()
        };
        let spec = materialize_step(
            &step,
            &context,
            Path::new("/project"),
            &EnvProfile::default(),
            &BTreeMap::new(),
        )
        .expect("should materialize");
        assert_eq!(spec.program, "tool");
        assert_eq!(spec.args, ["--serial=emulator-5554", "a.js", "b.js"]);
    }

    #[test]
    fn falls_back_to_environment_and_reports_missing_variables() {
        let context = context_with_variables(&[]);
        let path = std::env::var("PATH").expect("PATH should be set");
        assert_eq!(expand_variables("${PATH}", &context).expect("should expand"), path);
        let error = expand_variables("x ${CLI_TOOLS_XTASK_MISSING}", &context).expect_err("should fail");
        assert!(error.to_string().contains("CLI_TOOLS_XTASK_MISSING"));
        assert!(expand_variables("${unclosed", &context).is_err());
        assert!(expand_variables("${bad-name}", &context).is_err());
        assert_eq!(
            expand_variables("no variables", &context).expect("should expand"),
            "no variables"
        );
    }

    #[test]
    fn applies_profile_task_and_step_environment_in_order() {
        let context = context_with_variables(&[("VALUE", &["from-variable"])]);
        let profile = EnvProfile {
            set: BTreeMap::from([
                ("A".to_owned(), "profile".to_owned()),
                ("B".to_owned(), "profile".to_owned()),
            ]),
            remove: vec!["DATABASE_URL".to_owned()],
        };
        let task_env = BTreeMap::from([("B".to_owned(), "task".to_owned())]);
        let step = CommandStepConfig {
            argv: vec!["tool".to_owned()],
            cwd: Some("web".to_owned()),
            env: BTreeMap::from([("C".to_owned(), "${VALUE}".to_owned())]),
            pipe_through: vec!["xcbeautify".to_owned()],
        };
        let spec =
            materialize_step(&step, &context, Path::new("/project"), &profile, &task_env).expect("should materialize");
        assert_eq!(spec.cwd, Path::new("/project/web"));
        assert_eq!(spec.env_set.get("A").map(String::as_str), Some("profile"));
        assert_eq!(spec.env_set.get("B").map(String::as_str), Some("task"));
        assert_eq!(spec.env_set.get("C").map(String::as_str), Some("from-variable"));
        assert_eq!(spec.env_set.get("XTASK_TASK_ID").map(String::as_str), Some("task"));
        assert_eq!(spec.env_remove, ["DATABASE_URL"]);
        assert_eq!(spec.pipe_through, ["xcbeautify"]);
    }

    #[test]
    fn summarizes_steps_with_pipes() {
        let steps = vec![
            CommandStepConfig {
                argv: vec!["make".to_owned(), "build".to_owned()],
                ..CommandStepConfig::default()
            },
            CommandStepConfig {
                argv: vec!["xcodebuild".to_owned(), "test".to_owned()],
                pipe_through: vec!["xcbeautify".to_owned()],
                ..CommandStepConfig::default()
            },
        ];
        assert_eq!(
            summarize_steps(&steps),
            "make build && xcodebuild test 2>&1 | xcbeautify"
        );
    }

    #[test]
    fn names_flag_environment_variables() {
        assert_eq!(flag_env_name("sign-in"), "XTASK_FLAG_SIGN_IN");
        assert_eq!(flag_env_name("all"), "XTASK_FLAG_ALL");
    }

    #[test]
    fn rapid_run_ids_are_distinct() {
        assert_ne!(next_run_id(), next_run_id());
    }
}

#[cfg(test)]
mod test_exports {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn collects_variables_and_notes_from_export_files() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let exports = ExportFiles::for_log(&temp.path().join("prepare.log"));
        std::fs::write(&exports.env, "SERIAL=emulator-5554\nTARGET=a\nTARGET=b\n\n").expect("should write");
        std::fs::write(&exports.notes, "Coverage: target/cov/index.html\n").expect("should write");

        let context = context_with_variables(&[]);
        exports.collect(&context).expect("should collect");
        assert_eq!(context.variable("SERIAL"), Some(vec!["emulator-5554".to_owned()]));
        assert_eq!(context.variable("TARGET"), Some(vec!["a".to_owned(), "b".to_owned()]));
        let notes = context.shared.lock().expect("lock").notes.clone();
        assert_eq!(notes, ["Coverage: target/cov/index.html"]);

        exports.reset().expect("should reset");
        assert!(!exports.env.exists() && !exports.notes.exists());
        exports.reset().expect("reset of missing files should succeed");
    }

    #[test]
    fn rejects_malformed_export_lines() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let exports = ExportFiles::for_log(&temp.path().join("task.log"));
        std::fs::write(&exports.env, "not a pair\n").expect("should write");
        assert!(exports.collect(&context_with_variables(&[])).is_err());
    }
}

#[cfg(test)]
mod test_builtin_command_checks {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cli_tools_common::test_support::TempTree;

    use super::test_helpers::*;
    use super::*;

    /// Run one builtin and return its outcome and whether its action was invoked.
    fn run_builtin(command_checks: &str, dry_run: bool) -> (TaskOutcome, bool) {
        let tree = TempTree::new();
        let raw = format!("[[tasks]]\nid = 'setup'\nlabel = 'Setup'\nbuiltin = 'record-call'\n{command_checks}");
        let location = TaskFileLocation {
            path: tree.file_with("xtask.toml", &raw),
            root: tree.path().to_path_buf(),
        };
        let task_file = TaskFile::load(&location.path).unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let action_called = Arc::clone(&called);
        let builtin: Arc<dyn Builtin> = Arc::new(move |context: &TaskContext, _: &Path| {
            action_called.store(true, Ordering::SeqCst);
            Ok(if context.dry_run {
                TaskActionResult::DryRun
            } else {
                TaskActionResult::Passed
            })
        });
        let options = RunOptions {
            dry_run,
            ..RunOptions::default()
        };
        let tasks = plan(
            &task_file,
            &location,
            &options,
            &BTreeMap::from([("record-call".to_owned(), builtin)]),
            false,
        )
        .unwrap();
        let mut context = context_with_variables(&[]);
        context.repo_root = location.root;
        context.log_dir = tree.join("target/logs");
        context.dry_run = dry_run;
        let summary = run_schedule(tasks, &context, 1, false, "all").unwrap();
        (summary.reports[0].outcome.clone(), called.load(Ordering::SeqCst))
    }

    /// A missing required command fails the task with its configured installation hint.
    #[test]
    fn missing_required_command_prevents_builtin_execution() {
        let (outcome, called) = run_builtin(
            "require_commands = [{ name = 'cli-tools-xtask-test-missing-required-8pkw', hint = 'Install the testing tool' }]",
            false,
        );
        assert_eq!(outcome, TaskOutcome::Failed("Install the testing tool".to_owned()));
        assert!(!called);
    }

    /// A missing optional command skips the builtin before checking required commands.
    #[test]
    fn missing_optional_command_prevents_builtin_execution() {
        let (outcome, called) = run_builtin(
            "skip_if_missing = 'cli-tools-xtask-test-missing-optional-8pkw'\n\
             require_commands = ['cli-tools-xtask-test-missing-required-8pkw']",
            false,
        );
        assert_eq!(
            outcome,
            TaskOutcome::Skipped("cli-tools-xtask-test-missing-optional-8pkw not found".to_owned())
        );
        assert!(!called);
    }

    /// Dry runs still invoke the builtin so it can export placeholders without external tools.
    #[test]
    fn dry_run_ignores_builtin_command_checks() {
        let (outcome, called) = run_builtin(
            "skip_if_missing = 'cli-tools-xtask-test-missing-optional-8pkw'\n\
             require_commands = ['cli-tools-xtask-test-missing-required-8pkw']",
            true,
        );
        assert_eq!(outcome, TaskOutcome::DryRun);
        assert!(called);
    }
}

#[cfg(test)]
mod test_conditions_and_scopes {
    use super::*;

    const TASK_FILE: &str = r#"
        [flags.all]
        implies = ["web"]
        [flags.web]
        [flags.sign-in]
        [flags.ios]
        [flags.android]

        [conditions]
        sign-in-ios = "ios || (sign-in && !android && macos)"

        [scopes.mobile]
        includes = ["ios", "android"]

        [[tasks]]
        id = "always"
        label = "Always"
        scopes = ["rust"]
        command = ["true"]

        [[tasks]]
        id = "web-tests"
        label = "Web"
        when = "web"
        scopes = ["web"]
        command = ["true"]
        after = ["always"]

        [[tasks]]
        id = "ios-sign-in"
        label = "iOS sign-in"
        when = "sign-in-ios"
        scopes = ["ios"]
        command = ["true"]
        after = ["web-tests"]
    "#;

    fn enabled_ids(flags: &[&str], dry_run: bool) -> Vec<String> {
        let task_file = TaskFile::from_toml_str(TASK_FILE).expect("should parse");
        task_file.validate(&BTreeSet::new()).expect("should validate");
        let given = flags.iter().map(|flag| ((*flag).to_owned(), true)).collect();
        let resolved = crate::cli::resolve_flags(&task_file, &given, current_os()).expect("should resolve");
        let context = ConditionContext {
            flags: &resolved,
            conditions: task_file.parsed_conditions().expect("should parse conditions"),
            dry_run,
            is_ci: false,
        };
        build_tasks(&task_file, &context, Path::new("/project"), &BTreeMap::new())
            .expect("should build")
            .into_iter()
            .map(|task| task.id)
            .collect()
    }

    #[test]
    fn enables_tasks_from_flags_implied_flags_and_named_conditions() {
        assert_eq!(enabled_ids(&[], false), ["always"]);
        assert_eq!(enabled_ids(&["all"], false), ["always", "web-tests"]);
        assert_eq!(enabled_ids(&["ios"], false), ["always", "ios-sign-in"]);
        let sign_in_only = enabled_ids(&["sign-in"], false);
        assert_eq!(
            sign_in_only.contains(&"ios-sign-in".to_owned()),
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn soft_dependencies_only_apply_to_active_tasks() {
        let task_file = TaskFile::from_toml_str(TASK_FILE).expect("should parse");
        let given = BTreeMap::from([("ios".to_owned(), true)]);
        let flags = crate::cli::resolve_flags(&task_file, &given, current_os()).expect("should resolve");
        let context = ConditionContext {
            flags: &flags,
            conditions: task_file.parsed_conditions().expect("should parse conditions"),
            dry_run: false,
            is_ci: false,
        };
        let mut tasks =
            build_tasks(&task_file, &context, Path::new("/project"), &BTreeMap::new()).expect("should build");
        resolve_selected_dependencies(&task_file, &mut tasks).expect("should resolve");
        let ios = tasks
            .iter()
            .find(|task| task.id == "ios-sign-in")
            .expect("ios task should be enabled");
        assert!(
            ios.after.is_empty(),
            "web-tests is inactive, so the soft dependency is dropped"
        );
    }

    #[test]
    fn explains_hard_dependency_on_disabled_task() {
        let raw = format!(
            "{TASK_FILE}\n[[tasks]]\nid = \"needs-web\"\nlabel = \"Needs web\"\ndeps = [\"web-tests\"]\ncommand = [\"true\"]\n"
        );
        let task_file = TaskFile::from_toml_str(&raw).expect("should parse");
        let flags = crate::cli::resolve_flags(&task_file, &BTreeMap::new(), current_os()).expect("should resolve");
        let context = ConditionContext {
            flags: &flags,
            conditions: task_file.parsed_conditions().expect("should parse conditions"),
            dry_run: false,
            is_ci: false,
        };
        let mut tasks =
            build_tasks(&task_file, &context, Path::new("/project"), &BTreeMap::new()).expect("should build");
        let error = resolve_selected_dependencies(&task_file, &mut tasks)
            .expect_err("a hard dependency on a disabled task should fail");
        assert!(error.to_string().contains("'web-tests', which is not enabled"));
    }

    #[test]
    fn resolves_scopes_with_includes() {
        let task_file = TaskFile::from_toml_str(TASK_FILE).expect("should parse");
        assert!(resolve_scope(&task_file, None).expect("should resolve").is_none());
        assert!(
            resolve_scope(&task_file, Some("all"))
                .expect("should resolve")
                .is_none()
        );
        let mobile = resolve_scope(&task_file, Some("mobile"))
            .expect("should resolve")
            .expect("should select scopes");
        assert!(mobile.contains("ios") && mobile.contains("android"));
        let error = resolve_scope(&task_file, Some("backend")).expect_err("unknown scope");
        assert!(
            error
                .to_string()
                .contains("Available: all, android, ios, mobile, rust, web")
        );
    }
}
