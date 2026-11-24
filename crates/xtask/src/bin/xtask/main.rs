//! xtask - Run a project's tasks from `xtask.toml` in parallel.
//!
//! Finds the task file in the current directory or its parents and runs the tasks the given flags enable.
//! When the task file names a project specific runner in `[settings]`, the run is handed over to it,
//! so projects with Rust builtins work with plain `xtask` too.

use std::process::ExitCode;

fn main() -> ExitCode {
    cli_tools_xtask::Runner::new().with_delegation().run()
}
