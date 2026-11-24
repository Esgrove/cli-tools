//! Rust task implementations that projects register with [`Runner::builtin`](crate::Runner::builtin).
//!
//! A task in the task file refers to a builtin by name with `builtin = "<name>"`.
//! Builtins cover setup work that is awkward to express as commands,
//! such as selecting or booting a device and exporting its identifier to later tasks.

use std::path::Path;

use anyhow::Result;

use crate::scheduler::{TaskActionResult, TaskContext};

/// A task implemented in Rust.
///
/// Any `Fn(&TaskContext, &Path) -> Result<TaskActionResult> + Send + Sync` closure or function implements it.
/// The path is the task's log file.
pub trait Builtin: Send + Sync {
    /// Run the task.
    ///
    /// Builtins also run during `--dryrun`, with [`TaskContext::dry_run`] set,
    /// so they can export placeholder variables that later tasks show in their dry run output.
    /// A builtin should not change anything in a dry run and should return [`TaskActionResult::DryRun`].
    ///
    /// # Errors
    ///
    /// Returns an error to mark the task as failed.
    fn run(&self, context: &TaskContext, log_path: &Path) -> Result<TaskActionResult>;
}

impl<F> Builtin for F
where
    F: Fn(&TaskContext, &Path) -> Result<TaskActionResult> + Send + Sync,
{
    fn run(&self, context: &TaskContext, log_path: &Path) -> Result<TaskActionResult> {
        self(context, log_path)
    }
}
