//! Parallel task runner driven by an `xtask.toml` task file.
//!
//! The task file declares project flags,
//! named conditions, scopes, and a graph of tasks with dependencies, priorities, and exclusive resource locks.
//! The runner schedules the enabled tasks in parallel, streams their output live,
//! writes a log per task, and ends with a failure recap and a one line result.
//!
//! The `xtask` binary runs any task file with command tasks.
//! A project that needs Rust setup code builds its own small binary on this library
//! and registers that code as builtins:
//!
//! ```no_run
//! use std::path::Path;
//!
//! use cli_tools_xtask::{Runner, TaskActionResult, TaskContext};
//!
//! fn prepare_device(context: &TaskContext, _log_path: &Path) -> anyhow::Result<TaskActionResult> {
//!     context.set_variable("DEVICE_ID", vec!["emulator-5554".to_owned()]);
//!     Ok(if context.dry_run { TaskActionResult::DryRun } else { TaskActionResult::Passed })
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     Runner::new().builtin("prepare-device", prepare_device).run()
//! }
//! ```

#![cfg_attr(test, allow(clippy::panic_in_result_fn))]

mod builtin;
mod cli;
mod condition;
mod config;
mod process;
mod runner;
mod scheduler;
mod ui;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap_complete::Shell;

pub use crate::builtin::Builtin;
pub use crate::cli::RunOptions;
pub use crate::config::{TASK_FILE_NAME, TaskFile, TaskFileLocation, find_task_file};
pub use crate::process::{CommandOutput, CommandSpec, command_exists, find_command, require_command, run_capture};
pub use crate::scheduler::{TaskActionResult, TaskContext};

use crate::cli::{UserConfig, XtaskCommand, find_config_argument, parse_args};

/// Environment variable set when the generic binary hands a run over to a project runner.
const DELEGATED_ENV: &str = "XTASK_DELEGATED";

/// Computes the values of a variable once before the run, given the project root.
type VariableProvider = dyn Fn(&Path) -> Result<Vec<String>> + Send + Sync;

/// Configures and starts a task run.
#[derive(Default)]
pub struct Runner {
    builtins: BTreeMap<String, Arc<dyn Builtin>>,
    variables: BTreeMap<String, Arc<VariableProvider>>,
    delegate: bool,
}

impl Runner {
    /// Create a runner without builtins.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Rust implementation for tasks with `builtin = "<name>"`.
    #[must_use]
    pub fn builtin(mut self, name: impl Into<String>, builtin: impl Builtin + 'static) -> Self {
        self.builtins.insert(name.into(), Arc::new(builtin));
        self
    }

    /// Register a variable whose values are computed once before the run, for `${NAME}` expansion.
    ///
    /// An argument that is exactly `${NAME}` expands to every value as separate arguments.
    #[must_use]
    pub fn variable<F>(mut self, name: impl Into<String>, provider: F) -> Self
    where
        F: Fn(&Path) -> Result<Vec<String>> + Send + Sync + 'static,
    {
        self.variables.insert(name.into(), Arc::new(provider));
        self
    }

    /// Hand the run over to the project runner named by `runner` in the task file's `[settings]`.
    ///
    /// The generic `xtask` binary enables this, so a project with builtins can still be run with plain `xtask`.
    #[must_use]
    pub const fn with_delegation(mut self) -> Self {
        self.delegate = true;
        self
    }

    /// Run with the process arguments and print errors, returning the exit code.
    pub fn run(self) -> ExitCode {
        let argv: Vec<OsString> = std::env::args_os().collect();
        match self.run_with_args(&argv) {
            Ok(code) => code,
            Err(error) => {
                cli_tools_common::print_error!("{error:#}");
                ExitCode::FAILURE
            }
        }
    }

    /// Run with the given arguments, where the first one is the program name.
    ///
    /// # Errors
    ///
    /// Returns an error if the task file is missing or invalid, or the run cannot start.
    pub fn run_with_args(&self, argv: &[OsString]) -> Result<ExitCode> {
        let current_dir = std::env::current_dir().context("Failed to read the current directory")?;
        let location = match find_config_argument(argv) {
            Some(path) => Some(TaskFileLocation::from_path(&path, &current_dir)?),
            None => find_task_file(&current_dir),
        };
        let task_file = location
            .as_ref()
            .map(|location| TaskFile::load(&location.path))
            .transpose()?;

        if let (Some(location), Some(task_file)) = (&location, &task_file)
            && self.delegate
            && !task_file.settings.runner.is_empty()
            && std::env::var_os(DELEGATED_ENV).is_none()
            && !skips_delegation(argv)
        {
            return delegate(&task_file.settings.runner, argv, location);
        }

        let parsed = match parse_args(argv.iter().cloned(), task_file.as_ref(), location.as_ref()) {
            Ok(parsed) => parsed,
            Err(error) => {
                let _ = error.print();
                return Ok(if error.use_stderr() {
                    ExitCode::from(2)
                } else {
                    ExitCode::SUCCESS
                });
            }
        };

        if let Some(XtaskCommand::Completion(completion)) = parsed.args.command {
            generate_completion(
                completion.shell,
                completion.install,
                parsed.args.verbose,
                task_file.as_ref(),
                location.as_ref(),
            )?;
            return Ok(ExitCode::SUCCESS);
        }

        let (Some(location), Some(task_file)) = (location, task_file) else {
            anyhow::bail!(
                "No {TASK_FILE_NAME} found in {} or its parents up to the git repository root",
                current_dir.display()
            );
        };

        let builtin_names: BTreeSet<String> = self.builtins.keys().cloned().collect();
        task_file.validate(&builtin_names)?;
        let user_config = UserConfig::load()?;
        let options = parsed.into_options(&task_file, &user_config, runner::current_os())?;

        let mut variables = BTreeMap::new();
        for (name, provider) in &self.variables {
            let values = provider(&location.root).with_context(|| format!("Failed to compute variable {name}"))?;
            variables.insert(name.clone(), values);
        }

        let success = runner::run(&task_file, &location, &options, &self.builtins, variables)?;
        Ok(if success { ExitCode::SUCCESS } else { ExitCode::FAILURE })
    }

    /// IDs of the tasks a run with the given arguments would execute, in task file order, without running them.
    ///
    /// Useful in a project's own tests to check which tasks its flags enable.
    /// Variable providers are not evaluated.
    ///
    /// # Errors
    ///
    /// Returns an error if the task file is missing or invalid, the arguments do not parse,
    /// or the tasks cannot be resolved.
    pub fn resolve_task_ids(&self, argv: &[OsString]) -> Result<Vec<String>> {
        let current_dir = std::env::current_dir().context("Failed to read the current directory")?;
        let location = match find_config_argument(argv) {
            Some(path) => TaskFileLocation::from_path(&path, &current_dir)?,
            None => find_task_file(&current_dir)
                .with_context(|| format!("No {TASK_FILE_NAME} found in {}", current_dir.display()))?,
        };
        let task_file = TaskFile::load(&location.path)?;
        task_file.validate(&self.builtins.keys().cloned().collect())?;
        let parsed = parse_args(argv.iter().cloned(), Some(&task_file), Some(&location))?;
        let options = parsed.into_options(&task_file, &UserConfig::default(), runner::current_os())?;
        let tasks = runner::plan(&task_file, &location, &options, &self.builtins, false)?;
        Ok(tasks.into_iter().map(|task| task.id).collect())
    }
}

/// Print or install the shell completion script, including the project flags when a task file was found.
fn generate_completion(
    shell: Shell,
    install: bool,
    verbose: bool,
    task_file: Option<&TaskFile>,
    location: Option<&TaskFileLocation>,
) -> Result<()> {
    let command = cli::build_command(task_file, location);
    cli_tools_common::generate_shell_completion(shell, command, install, verbose, "xtask")
}

/// Return whether `argv` asks for a local meta-operation (`--help`, `-h`, `--version`, `-V`, or `completion`)
/// that must never execute an untrusted `[settings].runner` command from `xtask.toml`.
fn skips_delegation(argv: &[OsString]) -> bool {
    let mut skip_next = false;
    for arg in argv.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--" {
            return false;
        }
        if matches!(
            text,
            "-C" | "--config"
                | "-S"
                | "--scope"
                | "-j"
                | "--jobs"
                | "-O"
                | "--output"
                | "-K"
                | "--color"
                | "-T"
                | "--tail-lines"
        ) {
            skip_next = true;
            continue;
        }
        if matches!(text, "-h" | "--help" | "-V" | "--version" | "completion") {
            return true;
        }
        if let Some(short_group) = text.strip_prefix('-').filter(|rest| !rest.starts_with('-')) {
            for character in short_group.chars() {
                if matches!(character, 'C' | 'S' | 'j' | 'O' | 'K' | 'T') {
                    // The rest of this short flag group is the option's value.
                    break;
                }
                if matches!(character, 'h' | 'V') {
                    return true;
                }
            }
        }
    }
    false
}

/// Run the project runner from `[settings]` with the same arguments and return its exit code.
fn delegate(runner: &[String], argv: &[OsString], location: &TaskFileLocation) -> Result<ExitCode> {
    let Some((program, runner_args)) = runner.split_first() else {
        anyhow::bail!("The runner setting is empty");
    };
    let program_path = Path::new(program);
    let resolved_program = if program_path.is_relative() && program_path.components().count() > 1 {
        location.root.join(program_path)
    } else {
        program_path.to_path_buf()
    };
    let status = std::process::Command::new(&resolved_program)
        .args(runner_args)
        .args(argv.iter().skip(1))
        .current_dir(std::env::current_dir().unwrap_or_else(|_| location.root.clone()))
        .env(DELEGATED_ENV, "1")
        .status()
        .with_context(|| format!("Failed to start the project runner '{}'", runner.join(" ")))?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

#[cfg(test)]
mod test_runner_api {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const TASK_FILE: &str = r#"
        [[tasks]]
        id = "prepare"
        label = "Prepare"
        builtin = "prepare-device"

        [[tasks]]
        id = "check"
        label = "Check"
        deps = ["prepare"]
        command = ["sh", "-c", "test \"$0 $1\" = \"dev-1 a.js\" && test \"$2\" = b.js", "${DEVICE}", "${TARGETS}"]
    "#;

    static BUILTIN_RAN: AtomicBool = AtomicBool::new(false);

    #[expect(clippy::unnecessary_wraps, reason = "matches the Builtin signature")]
    fn prepare_device(context: &TaskContext, _log_path: &Path) -> Result<TaskActionResult> {
        BUILTIN_RAN.store(true, Ordering::SeqCst);
        context.set_variable("DEVICE", vec!["dev-1".to_owned()]);
        Ok(if context.dry_run {
            TaskActionResult::DryRun
        } else {
            TaskActionResult::Passed
        })
    }

    #[cfg(unix)]
    #[test]
    fn runs_registered_builtins_and_variable_providers() {
        let temp = tempfile::tempdir().expect("should create temp dir");
        let task_file = temp.path().join("xtask.toml");
        std::fs::write(&task_file, TASK_FILE).expect("should write task file");

        let runner = Runner::new()
            .builtin("prepare-device", prepare_device)
            .variable("TARGETS", |_root| Ok(vec!["a.js".to_owned(), "b.js".to_owned()]));
        let argv: Vec<OsString> = ["xtask", "-O", "plain", "-K", "never", "-C"]
            .into_iter()
            .map(OsString::from)
            .chain(std::iter::once(task_file.into_os_string()))
            .collect();
        let code = runner.run_with_args(&argv).expect("run should start");
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(BUILTIN_RAN.load(Ordering::SeqCst));

        let ids = runner.resolve_task_ids(&argv).expect("tasks should resolve");
        assert_eq!(ids, ["prepare", "check"]);
    }

    #[test]
    fn delegation_does_not_execute_runner_for_version_or_completion() {
        let temp = tempfile::tempdir().expect("temp dir");
        let task_file = temp.path().join("xtask.toml");
        std::fs::write(
            &task_file,
            "[settings]\nrunner = [\"definitely_nonexistent_xtask_runner_binary_xyz\"]\n",
        )
        .expect("write task file");

        let runner = Runner::new().with_delegation();
        let version_argv: Vec<OsString> = ["xtask", "-C"]
            .into_iter()
            .map(OsString::from)
            .chain([task_file.clone().into_os_string(), OsString::from("--version")])
            .collect();
        let code = runner
            .run_with_args(&version_argv)
            .expect("--version must not execute [settings].runner");
        assert_eq!(code, ExitCode::SUCCESS);

        let completion_argv: Vec<OsString> = ["xtask", "-C"]
            .into_iter()
            .map(OsString::from)
            .chain([
                task_file.into_os_string(),
                OsString::from("completion"),
                OsString::from("zsh"),
            ])
            .collect();
        let code = runner
            .run_with_args(&completion_argv)
            .expect("completion must not execute [settings].runner");
        assert_eq!(code, ExitCode::SUCCESS);
    }
}
