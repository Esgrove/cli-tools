---
name: code-review
description: Review workspace correctness, safety, architecture, and tests, optionally limited to one crate.
argument-hint: "[all|<crate>] [changes|base=<ref>] [focus]"
---

# Review the project

Perform an evidence-based code review.
Do not modify code unless the user explicitly requests fixes.
Arguments: $ARGUMENTS
Treat arguments or instructions appended to this command as scope, review target, and focus.
If the arguments line still shows a literal `$ARGUMENTS` placeholder, use the text the user wrote after the command.

## Select scope and target

Default to reviewing the current first-party code across the whole workspace, not just the latest diff.
Do not infer a narrower scope from an open file, current directory, or recent conversation.

- `all`, or no selector: every crate under `crates/`, the root scripts, and the workspace manifests.
- A crate directory name under `crates/`:
  that crate, plus the shared crates (`common`, `media`) only where it uses them.

An explicit file or directory further narrows the scope.
`changes` selects staged, unstaged, and non-ignored untracked changes relative to `HEAD`.
`base=<ref>` selects branch changes from the merge base of that ref and `HEAD`, plus local changes.
Resolve the named ref without changing branches or fetching unless permitted.
If an explicitly requested ref is missing, ask rather than guessing.
For either diff mode, read the full affected functions and callers and report issues introduced by the changes.
A clean diff is an empty review, not permission to silently audit the whole workspace.
Do not interpolate arbitrary arguments into shell commands.
Exclude `target/`, checked-in binaries, and `crates/slb/tests/fixtures/slb`.

## Load project guidance

Read root `AGENTS.md`, `README.md`, the selected crates' `README.md`, and record `git status --short`.
Read the applicable `.agents/rules/` files.
Use repository instructions as the authority, not personal style preferences.
A focused style and organization cleanup belongs in `.agents/commands/guidelines-review.md`.

## Review workflow

1. State the interpreted scope, target, and focus before investigating.
2. Inspect implementation, call sites, invariants, error paths, and the tests that establish the contract.
   Prioritize risky boundaries over repetitive code.
3. Where useful, split independent crates among read-only reviewers with explicit scope.
   Reconcile findings centrally and do not let reviewers edit the working tree.
4. Validate each candidate finding against the actual behavior and existing tests.
   Find a concrete triggering input and trace it to an observable failure.
   Run a focused existing test or a safe scratch reproduction where practical.
   Drop speculative findings instead of padding the report.
5. Run only the checks needed to substantiate findings, in non-autofixing modes.
   See `.agents/commands/all-checks.md` for commands, but do not run the complete suite by default.

### Project-specific checklist

- **Filesystem safety**: tools that rename, move, or delete files
  (`dirmove`, `dots`, `vres`, `flipdate`, `rxrename`, `qtorrent`, `vconvert`) must handle name collisions,
  dry run modes, symlinks, case-only renames, non-UTF-8 names, and partial failure without losing data.
- **Shared crates**: behavior changes in `common` and `media` are checked against every dependent crate.
  SIMD kernels in `common/src/simd.rs` match the scalar code they replace on every input length and alignment.
- **Prose tooling**: `slb` formatting is idempotent, never alters code, string literals, or fenced blocks,
  and keeps its check and fix pipelines in agreement.
- **External processes and data**: `ffprobe`/`ffmpeg` invocations avoid shell interpolation,
  and the `SQLite` scan cache stays consistent when files change.
  Config parsing for `~/.config/cli-tools.toml` has defaults for every option.
- **Rust safety and style**: no plain `unwrap` or panics in production code, `anyhow` error context,
  clap short and long options for every argument, and clippy pedantic and nursery cleanliness.
- **Architecture and guideline violations**:
  report violations only when they create a concrete maintenance or safety problem.
  Keep cosmetic preferences and broad speculative refactors out of the defect list.

## Report

Put actionable findings first, ordered by severity.
For each finding, give a concise title, repository-relative file and precise line range,
the triggering conditions, the observable consequence, and the evidence or reproduction.
Use priorities consistently: P0 for a release-blocking critical failure, P1 for a high-impact defect,
P2 for a normal defect, and P3 for a low-impact actionable issue.
Do not assign a high priority without explaining the impact and reach.

Then summarize the areas inspected, checks actually run, and remaining coverage gaps.
If there are no confirmed findings, say so explicitly and still state the review limits.
Do not imply the workspace is bug-free or that the whole scope was reviewed when only a subset was.
Verify that the review left existing work unchanged.
