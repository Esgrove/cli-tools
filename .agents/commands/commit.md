---
name: commit
description: Commit completed work in separate, logical units.
---

# Commit work

Inspect the worktree and review the complete diff before committing.
Identify which changes belong to the current task.
Leave unrelated or pre-existing changes untouched.

Split independent changes into separate commits.
Keep changes that form one coherent feature or fix together.
Stage only the files or hunks for the logical unit being committed, then verify the staged diff before creating its commit.
Repeat for each remaining unit.

Use the message format and prefix rules in `.agents/rules/commit-messages.md`.
Do not amend existing commits or push changes.
