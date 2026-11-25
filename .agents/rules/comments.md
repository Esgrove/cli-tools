---
description: Commenting and documentation conventions for the Rust sources, scripts, and docs
trigger: glob
globs: crates/**, docs/**, *.sh, *.md
paths:
  - "crates/**"
  - "docs/**"
  - "*.sh"
  - "*.md"
alwaysApply: false
---

# Commenting conventions

## Comment formatting

Comments in code and files that a change touches, and the documentation related to them, may be reformatted.
Use semantic line breaks in Markdown, comments, and doc comments.
Start a new line at sentence boundaries or natural clause boundaries.
Target about 120 characters per line, but prefer a clean sentence break over a strict width.
Do not wrap prose by splitting a sentence at an arbitrary point in the middle.
Leave comments in files the change does not touch as they are, unless asked to reformat them.

- Avoid dashes and semicolons in prose. Use commas and periods to separate ideas.
- Quote literal user input values, for example "y", "yes", "n", "no", and "s".
- Avoid trailing comments. Put a comment on its own line above the code.
- `slb` owns comment wrapping, so run `slb` (check) or `slb -f` (fix) on touched files.
  Respect any `slb-ignore-file` directive.

## Comments describe the current state, not change history

Comments, including doc, block, and line comments and those in SQL, YAML, TOML, and shell scripts,
must document what the code currently does and why.
Do not narrate the change that introduced the code,
the previous state of the PR, or what something "used to" do.

Avoid phrases like:

- "Before this PR / change / refactor..."
- "Previously..." / "Formerly..." / "Earlier..."
- "Used to..." / "No longer..." / "Now both/have/holds..."
- "As of the X split / migration / refactor..."
- "This PR / commit / branch..."

A reader of the merged file should be able to understand the comment
without any knowledge of the diff that produced it.

## Module and item documentation

- Start every Rust source file with `//!` module-level documentation.
  Briefly describe the module's purpose, its main contents, and what it implements.
- Create docstrings for structs and functions.
- Give each crate a short `README.md`.
