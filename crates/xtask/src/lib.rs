//! Parallel task runner driven by an `xtask.toml` task file.
//!
//! The task file declares project flags, named conditions, scopes, and a graph of tasks
//! with dependencies, priorities, and exclusive resource locks.
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

        if let Some(XtaskCommand::Completion { shell, install }) = parsed.args.command {
            generate_completion(
                shell,
                install,
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

/// Run the project runner from `[settings]` with the same arguments and return its exit code.
fn delegate(runner: &[String], argv: &[OsString], location: &TaskFileLocation) -> Result<ExitCode> {
    let Some((program, runner_args)) = runner.split_first() else {
        anyhow::bail!("The runner setting is empty");
    };
    let status = std::process::Command::new(program)
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
    }
}
