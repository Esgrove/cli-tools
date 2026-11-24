# cli-tools-xtask

Parallel task runner driven by an `xtask.toml` task file.

It runs a project's checks, tests, and other tasks in parallel
while respecting dependencies, priorities, and exclusive resource locks,
streams live output, writes a full log per task,
and ends with a recap of the failed tasks and a one line result.

```shell
cargo run --bin xtask -- -h
cargo test -p cli-tools-xtask
cargo install --locked --path crates/xtask
```

## Running tasks

`xtask` looks for `xtask.toml` in the current directory and its parents,
stopping at the root of the git repository.
Outside a git repository only the current directory is checked.
Use `-C, --config <PATH>` to point at another file.
Relative paths in the task file resolve against the directory that contains it.

```shell
# Run every task the flags enable
xtask

# Project flags declared in the task file combine like any short options
xtask -ac

# Run selected tasks and their prerequisites
xtask cargo-fmt cargo-clippy

# Show what would run
xtask --list
xtask --dryrun -a
```

Each task's outcome is printed once, when it finishes.
The run then ends with the failure recap, followed by the totals, the log directory, and any notes:

```console
=== FAILURES ===

FAIL cargo-clippy — Rust lints (14.2s)
  Error:  Command exited with status 101: cargo clippy --workspace -- -Dwarnings
  Log:    target/xtask-logs/20261124T120000Z/cargo-clippy.log
  Re-run: xtask --all cargo-clippy
  ...

Result: 27 passed, 1 failed, 1 blocked, 0 skipped (wall: 412.0s, cumulative: 1032.5s, speedup: 2.51x)
Logs:   target/xtask-logs/20261124T120000Z
Note:   Rust coverage: target/llvm-cov/html/index.html
```

The output adapts to the terminal.
`--output auto` uses the interactive progress view on a terminal, a CI log stream when `CI` is set,
and plain timestamped lines otherwise.
`--color auto` honors `NO_COLOR`, `CLICOLOR_FORCE`, `GITHUB_ACTIONS`, and `TERM=dumb`.
In CI mode on GitHub Actions, failures are also emitted as `::error` annotations and grouped log sections.

Defaults for `output`, `color`, `jobs`, `tail_lines`, and `fail_fast`
can be set in the `[xtask]` section of `~/.config/cli-tools.toml`.
Command line options override the task file `[settings]`, which override the user config.

## Task file

```toml
[settings]
# Log directory relative to the project root (default: target/xtask-logs)
log_dir = "target/test-logs"
# Default concurrency (default: min(CPUs, 6))
jobs = 6
# Command shown in the re-run hint of a failed task (default: xtask)
rerun_prefix = "./test.sh"
# Project runner that the generic xtask binary hands the run over to, see "Rust builtins"
runner = ["cargo", "run", "--quiet", "--package", "xtask", "--"]

# Project flags become command line options. They cannot reuse the generic options.
[flags.all]
short = "a"
help = "Also run static checks and UI tests"
implies = ["web"]

[flags.web]
short = "w"
help = "Run the browser tests"

[flags.show-ui]
short = "u"
help = "Show the simulator window"
requires = ["all"]
platforms = ["macos"]

# Named conditions combine flags with !, &&, ||, and parentheses.
# The built-in conditions macos, linux, windows, unix, ci, and dryrun are always available.
[conditions]
full-web = "web && !ci"

# A scope selected with --scope also selects the scopes it includes.
[scopes.mobile]
includes = ["ios", "android"]

# Environment changes shared by several tasks.
[env_profiles.offline]
set = { SQLX_OFFLINE = "true" }
remove = ["DATABASE_URL"]

[[tasks]]
id = "cargo-clippy"
label = "Rust lints"
priority = 2                # 1 runs first, larger values later (default: 5)
deps = ["web-deps"]         # must be enabled and pass first
after = ["cargo-fmt"]       # ordering that only applies when both tasks run
locks = ["cargo-target"]    # no other task holding the same lock runs at the same time
scopes = ["rust"]
when = "all"                # condition expression, the task always runs when unset
env_profile = "offline"
env = { RUSTFLAGS = "-Dwarnings" }
require_commands = ["cargo", { name = "cargo-nextest", hint = "Install it with: cargo install --locked cargo-nextest" }]
skip_if_missing = "cargo-clippy"
notes = ["Shown at the end of the run when the task passes"]
command = ["cargo", "clippy", "--workspace", "--all-targets"]

[[tasks]]
id = "ios-tests"
label = "iOS tests"
when = "macos"
commands = [
    { argv = ["xcodegen", "generate"], cwd = "ios" },
    { argv = ["xcodebuild", "test", "-destination", "id=${IOS_SIMULATOR_ID}"], cwd = "ios", pipe_through = ["xcbeautify"] },
]
```

### Variables

`${NAME}` in arguments and environment values expands to a variable set by an earlier task,
by a builtin, or by a variable provider, and falls back to the process environment.
An argument that is exactly `${NAME}` expands to every value of a list variable as separate arguments.

Command tasks export variables by writing `KEY=VALUE` lines to the file named by `$XTASK_ENV`.
Repeating a key makes a list.
Lines written to `$XTASK_NOTES` are printed at the end of the run.
A task that uses an exported variable should depend on the task that exports it.

Every command also gets `XTASK_ROOT`, `XTASK_LOG_DIR`, `XTASK_TASK_ID`, `XTASK_DRYRUN`,
and `XTASK_FLAG_<NAME>` set to `1` or `0` for each project flag, for example `XTASK_FLAG_SHOW_UI`.

## Rust builtins

Setup that is awkward to express as commands can be written in Rust.
A project adds a small binary crate that depends on this library and registers builtins,
which tasks then refer to with `builtin = "<name>"`:

```toml
[dependencies]
cli-tools-xtask = { git = "https://github.com/Esgrove/cli-tools" }
```

```rust
use std::path::Path;
use std::process::ExitCode;

use cli_tools_xtask::{Runner, TaskActionResult, TaskContext};

fn prepare_simulator(context: &TaskContext, log_path: &Path) -> anyhow::Result<TaskActionResult> {
    if context.dry_run {
        context.set_variable("IOS_SIMULATOR_ID", vec!["<simulator>".to_owned()]);
        return Ok(TaskActionResult::DryRun);
    }
    context.log_note(log_path, "Booting simulator")?;
    context.set_variable("IOS_SIMULATOR_ID", vec![boot_simulator()?]);
    Ok(TaskActionResult::Passed)
}

fn main() -> ExitCode {
    Runner::new()
        .builtin("prepare-ios-simulator", prepare_simulator)
        .variable("WEB_TARGETS", |root| list_web_targets(root))
        .run()
}
```

Builtins also run during `--dryrun` so they can export placeholder values.
`TaskContext` gives access to the project flags (`context.flag("coverage")`), `--fail-fast`,
shared variables, notes, and `run_logged` for running commands with live output and logging.

`Runner::resolve_task_ids` returns the tasks a command line would run without running them,
so a project's tests can check which tasks its flags enable.

Setting `runner` in `[settings]` to the command that starts
that binary lets the generic `xtask` binary hand the run over to it,
so plain `xtask` works in the project as well.

## Library layout

- `lib.rs` - The `Runner` builder and the public API
- `builtin.rs` - The `Builtin` trait for Rust tasks
- `cli.rs` - Generic arguments, project flags added at runtime, and output mode detection
- `config.rs` - Task file discovery, schema, and validation
- `condition.rs` - The boolean expression parser for `when` and `[conditions]`
- `runner.rs` - Turning the task file into scheduled tasks, variable expansion, and exports
- `scheduler.rs` - The parallel scheduler and the `TaskContext`
- `process.rs` - Running commands with live output and per-task logs
- `ui/` - The interactive progress view, the line output, and the end of run summary

The interactive view uses `indicatif` with `console` styles,
since `indicatif` templates and terminal handling are built on `console`.
