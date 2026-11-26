---
name: guidelines-review
description: Apply a lightweight, behavior-preserving cleanup against the project's existing organization and style rules.
argument-hint: "[all|<crate>] [changes|path] [check-only]"
---

# Review and apply project guidelines

Check the selected code against the repository's existing guidelines and make small, justified cleanup edits.
This is lighter than a full correctness review or an all-checks run.
Arguments: $ARGUMENTS
Treat arguments or instructions appended to this command as scope, file selection, and mode.
If the arguments line still shows a literal `$ARGUMENTS` placeholder, use the text the user wrote after the command.
Default to the whole workspace, applying safe fixes.
`check-only` reports without editing.

## Scope

- `all`, or no scope: all first-party code and configuration.
- A crate directory name under `crates/`: that crate.

An explicit file or directory narrows that scope.
`changes` limits cleanup to staged, unstaged, and non-ignored untracked changes relative to `HEAD`,
plus nearby declarations that must move together for a coherent cleanup.
Do not infer `changes` from a dirty working tree.
A clean `changes` selection is a no-op, not permission for a workspace-wide sweep.
Exclude `target/`, checked-in binaries, and `crates/slb/tests/fixtures`.

## Preparation

Read root `AGENTS.md`, the selected crates' `README.md`,
`.agents/rules/code-organization.md`, and `.agents/rules/comments.md`.
Record `git status --short` before edits.
State the selected scope and whether fixes or `check-only` mode are selected.
Use written rules and existing crate boundaries as authority.
Do not invent conventions or edit rules to fit code.
Preserve unrelated work. Do not stage, commit, or run destructive Git commands.

## Lightweight checklist

### Organization

- Declaration order: enums, structs, implementations in struct order, then functions.
  Constructors first in implementations, then public before private methods.
- Source files past 1000 lines of non-test code are split by concern, each new file with its own `//!` docs.
  Defer large module moves and report them with a concrete reason.
- Shared helpers live in `cli-tools-common` or `cli-tools-media`, never in a binary.
  Visibility is as narrow as possible (`pub(super)` for sibling-only helpers).
- Test modules are separate root-level `#[cfg(test)]` modules with no nesting.
- Each crate has a short `README.md`, and every benchmark imports library code instead of duplicating it.

### Style, naming, and documentation

- No plain `unwrap`, no single character variable names, and full names over abbreviations.
- `anyhow` `Result<T>` returns, `clap` derive with a short and a long option for every argument.
- Every Rust file starts with `//!` documentation, and structs and functions have docstrings.
- Semantic line breaks in comments, docstrings, and Markdown, with no trailing comments,
  no dashes or semicolons in prose, and no change-history wording.
  Reformat comments only in code touched by the cleanup.
- Keep user-facing text, CLI flags, config keys, and public library signatures unchanged in a style-only cleanup.
- `README.md` usage output and `cli-tools.toml` match the current CLI arguments.

## Apply safe fixes

Prefer a small coherent batch of mechanical or clearly behavior-preserving edits.
For a broad scope, inventory every selected crate first, then work through bounded batches.
Do not stop after the first file and claim the whole workspace was checked.
If the cleanup cannot fit one pass, report inspected areas and deferred batches explicitly.
Review every formatter or lint autofix.
In `check-only` mode, do not run autofixing commands.
If a correctness bug or risky refactor is discovered, report it separately instead of expanding the task.

## Proportionate verification

- **Rust edits**: `cargo fmt`, `cargo clippy --workspace --all-targets -- -Dwarnings`, and `cargo test --workspace`
  (or targeted crate tests plus workspace clippy).
- **Shell edits**: `shfmt -d <paths>` (or `-w`) and `shellcheck -x <paths>`.
- **Prose and comment edits**: `slb` (check-only) or `slb -f` (fix) on touched files when available.

Do not launch the full suite from `.agents/commands/all-checks.md` for cosmetic changes.
Report unavailable tools and tests not run honestly.

## Completion

Review the final diff for accidental behavior changes and unrelated formatting churn.
Summarize the scope inspected, guideline categories corrected, checks run,
and remaining violations or risky changes deferred for a full review.
In `check-only` mode, provide concise file and line findings tied to the written rule.
Do not claim a full correctness audit from this lighter pass.
