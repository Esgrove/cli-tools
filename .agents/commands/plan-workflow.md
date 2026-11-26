---
name: plan-workflow
description: Create or adjust implementation plans following cli-tools commit, testing, coding guideline, and verification standards. Use whenever Plan Mode is active or when creating, updating, or refining an implementation plan.
argument-hint: "[goal or plan to adjust]"
---

# Plan implementation work

Apply these standards whenever Plan Mode is active, when authoring a new implementation plan,
or when adjusting an existing plan.
Arguments: $ARGUMENTS
Treat arguments or instructions appended to this command as the goal, scope, or plan to adjust.
If the arguments line still shows a literal `$ARGUMENTS` placeholder, use the text the user wrote after the command.

## Creating or adjusting a plan

- When creating a new plan, structure every task, test, commit, and verification step around the rules below.
- When adjusting an existing plan, preserve the requested scope and technical direction
  while adding any missing commit, regression test, coding guideline, or verification steps.
- Keep plans proportional to the task complexity without omitting the required testing, commit, or verification steps.

## Task and commit structure

- Structure each subitem or task as one or more logical, self-contained commits.
- Follow `.agents/rules/commit-messages.md` and `.agents/commands/commit.md` for commit splitting and message format.
- Use the crate directory name or area prefix when appropriate (for example `slb:`, `dirmove:`, `common:`, or `docs:`),
  and never use `feat:` or `chore:`.
- Include a concise commit body explaining what changed and why.
- Do not amend existing commits or push changes unless explicitly asked.

## Bugfix and shortcoming tests

- Whenever a task fixes an identified bug, issue, or shortcoming,
  plan test cases that reproduce the issue before implementing the fix.
- Confirm those tests fail against the existing behavior,
  then verify they pass after the fix to demonstrate the resolution and prevent future regressions.
- Follow the testing conventions in `AGENTS.md` and `.agents/rules/code-organization.md`,
  including separate root-level `#[cfg(test)]` modules with no nesting.

## Per-task lightweight verification

- Require lightweight, targeted checks and tests for each task and commit
  rather than deferring all validation to the end.
- Prefer fast scoped checks for the affected crate or files, such as `cargo test -p <package>`,
  targeted `cargo clippy`, `shfmt` and `shellcheck -x` for shell scripts, and `slb` on touched Markdown or sources.
- Require running the repository pre-commit hooks (`prek`, configured in `.pre-commit-config.yaml`) for each commit.

## Coding guidelines

Plan for the repository's organization, style, and documentation conventions from the start
rather than deferring cleanup to the end:

- **Structure and boundaries**: keep shared helpers in `cli-tools-common` or `cli-tools-media` instead of binaries,
  keep visibility narrow such as `pub(super)` for sibling-only helpers,
  and split non-test files past 1000 lines by concern.
  Follow Rust declaration order:
  enums, structs, implementations in struct order with constructors
  then public before private methods, and functions last.
- **Style and APIs**:
  keep clippy pedantic and nursery clean, use `anyhow::Result<T>` with no plain `unwrap` in production code,
  use descriptive full names with no single-character variables,
  and give every `clap` CLI argument both a short and a long option.
- **Docs, comments, and config**: start every Rust file with `//!` module docs, add docstrings to structs and functions,
  and describe current behavior rather than change history.
  Use semantic line breaks without trailing comments, dashes, or semicolons in prose,
  and update `README.md` usage output or `cli-tools.toml` whenever CLI flags or config options change.
- **Guideline check for larger edits**: when a task adds modules, changes public APIs, or refactors existing code,
  include a lightweight guideline pass over the changed files matching `.agents/commands/guidelines-review.md`,
  using `AGENTS.md`, `.agents/rules/code-organization.md`, and `.agents/rules/comments.md` as authority.

## Final full verification

- Always include a final task after all implementation items are complete to run the full project checks,
  tests, build, and lints, following `.agents/commands/all-checks.md`.
- Require fixing any findings, warnings, formatting issues, or test failures surfaced by the full run before finishing.

## Plan checklist

Before presenting or updating a plan, verify that it covers:

- [ ] Each subitem or task maps to one or more logical commits with project prefixes and a concise commit body
- [ ] Every bugfix or shortcoming task writes reproducing tests first, then verifies the fix and prevents regression
- [ ] Each task includes targeted lightweight checks, tests, and pre-commit hooks (`prek`)
- [ ] Implementation tasks explicitly adhere to repository coding and documentation guidelines
- [ ] A final task runs the full project checks, tests, build, and lints and fixes any findings or errors
