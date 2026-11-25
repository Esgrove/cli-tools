---
name: all-checks
description: Run the complete cli-tools verification suite, fix issues, and report remaining coverage gaps.
argument-hint: "[all|<crate>] [check-only] [coverage] [bench] [additional instructions]"
---

# Run all checks

Run the project's checks, not just a plan or a list of commands.
Arguments: $ARGUMENTS
Treat arguments or instructions appended to this command as the requested scope, mode, and constraints.
If the arguments line still shows a literal `$ARGUMENTS` placeholder, use the text the user wrote after the command.
Default to the whole workspace, fixing issues found by the checks and rerunning affected validation.
`check-only` runs non-autofixing checks and reports issues without editing tracked files.
State the selected scope, opt-in tiers, and mode before starting.

## Scope and preparation

- `all`, or no scope: the whole workspace.
- A crate directory name under `crates/` (for example `slb` or `common`) narrows checks to `-p cli-tools-<name>`
  or the crate's own package name, as listed in its `Cargo.toml`.
  Also check dependents when a shared crate (`common`, `media`) changed.

Optional tier flags:

- `coverage`: collect a `cargo-llvm-cov` report.
- `bench`: run the relevant Criterion benchmarks with `cargo bench -- --quick`.

Read root `AGENTS.md`, `README.md`, `.pre-commit-config.yaml`, and the READMEs of the selected crates first.
The current files are authoritative if this checklist becomes stale.

1. Record `git status --short` and the toolchain versions
   (`cargo`, `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`, `slb`, `shfmt`, `shellcheck`, `prek`).
   Preserve existing work. Do not stage, commit, or run destructive Git commands.
   Do not silently install global tools.
2. Run independent checks separately so one failure does not hide later checks.
   Preserve exit codes when capturing logs.

## Rust workspace

Autofix steps are skipped in `check-only` mode.

1. Autofix: `cargo clippy --fix --allow-dirty --workspace`
   and `cargo clippy --fix --allow-dirty --workspace --tests`, then `cargo fmt`.
   In `check-only` mode use `cargo fmt --check` instead.
2. `cargo clippy --workspace --all-targets -- -Dwarnings`.
3. `cargo test --workspace`
   (or `cargo nextest run --workspace` followed by `cargo test --workspace --doc`, because nextest skips doctests).
4. `cargo deny check` when a `deny.toml` exists. Report it as not configured otherwise.

## Other tooling

- **Shell scripts**: `shfmt -d` (check-only) or `shfmt -w` (fix), and `shellcheck -x`, on the root `*.sh` files.
- **Semantic line breaks**: `slb` (check-only) or `slb -f` (fix) on touched Markdown and sources.
  Record an explicit omission if `slb` is absent.
- **Hooks**: `prek run --all-files` in fix mode as a final check, reviewing any hook edits.
  Do not run it in `check-only` mode.
- **Docs**: when CLI arguments changed, confirm the `README.md` usage output matches `cargo run --bin <name> -- -h`
  and that `cli-tools.toml` documents new config options.

## Coverage and benchmarks (when requested)

- `cargo llvm-cov nextest --workspace` for a text report, or `cargo llvm-cov report --html`.
- `cargo bench --bench <suite> -- --quick` for the suites affected by the change.
  `./simd-bench.sh` covers the SIMD kernels.

## Completion and reporting

Wait for every started check and record its exit status.
Keep passing, failing, blocked, and skipped outcomes distinct, with exact commands.
Unless `check-only` was requested, fix issues with minimal scoped changes, inspect every autofix,
and rerun failed checks until they pass or a concrete blocker remains.
Never weaken tests, lints, or assertions to obtain a pass.
Finish with failures first, then validation gaps,
then a final `git status --short` comparison that identifies any unexpected modifications.
