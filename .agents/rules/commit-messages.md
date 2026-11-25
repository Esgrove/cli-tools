---
description: Commit message format and prefix conventions
trigger: model_decision
alwaysApply: false
---

# Commit messages

- Start with the crate or area prefix, followed by a colon and a concise imperative summary.
  Examples include `slb:`, `xtask:`, `rsbump:`, `common:`, `media:`, `docs:`, `ci:`, and `build:`.
  Use the crate directory name, not the `cli-tools-` package name.
- Use `cargo update` as the whole message for a dependency refresh commit.
- Choose the most specific applicable area. Omit a prefix when none of the established areas fits naturally.
- Never use `feat:` or `chore:`.
- Keep each commit focused on one logical unit of work.
