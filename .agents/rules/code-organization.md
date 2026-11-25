---
description: Code organization, module boundaries, and file structure for the cli-tools workspace
trigger: glob
globs: crates/**, *.sh
paths:
  - "crates/**"
  - "*.sh"
alwaysApply: false
---

# Code organization

These rules apply to first-party Rust sources and shell scripts in the workspace.

## Workspace and crate boundaries

- Each tool is its own crate under `crates/` with a package name prefixed by `cli-tools-`.
- Shared helpers live in `cli-tools-common` and `cli-tools-media`.
  A helper that is not specific to one tool belongs in a shared library crate,
  such as `crates/common/src/utils.rs` or `crates/common/src/diff.rs`, never inside a binary.
- Tools with reusable logic expose it as a library from the same crate.
  A crate with both a library and a binary keeps the library in `src/lib.rs` and the binary in `src/bin/<name>/main.rs`.
  A binary only crate keeps its binary in `src/main.rs`.
- When extracting algorithmic code for benchmarking, move the pure functions and types to the library part of the crate
  and import them from the benchmark. Do not duplicate code in benchmark files.
- The root `Cargo.toml` is a virtual workspace holding shared package metadata, dependency versions, lints, and profiles.
  Every crate uses `workspace = true` for these.
- A new tool is a new crate that only depends on the workspace crates and dependencies it actually uses,
  so a project that pulls in one tool does not build the others.
- Give each crate a short `README.md`.

## Rust declaration order

- All enums before structs.
- All struct definitions before any implementations.
- Implementations only after the last struct definition, in the order of the struct definitions.
- Functions after implementations.
- In implementations, put constructors first: `new` first,
  then other associated functions that return `Self` (`default`, `from_*`, and similar).
- After constructors, order public methods before private methods.
- Keep visibility as narrow as possible.
  A helper that only crosses into a sibling module of the same parent is `pub(super)`, not `pub`.

## File size

- Once a source file passes 1000 lines, split it into smaller modules.
  Going over 1000 is fine when most of the file is test code.
- Split along the concerns the file has grown into, one file per concern,
  and give each new file its own `//!` documentation naming that concern.

## Tests

- Never nest modules inside test modules.
  Every test module is a separate root-level `#[cfg(test)]` module, and none is wrapped in a parent `mod tests`.
- When several files share test helpers, put them in a `#[cfg(test)]` gated module declared from `mod.rs`.
  See `crates/slb/src/test_helpers.rs`.

## Code style

- Rust 2024 edition, with clippy pedantic and nursery lints enabled.
- No plain `unwrap` in non-test code. Use proper error handling, or `.expect()` in constants.
  Tests may use `unwrap()`, and use `.expect()` only when its message adds real diagnostic value.
- Use `anyhow` with `Result<T>` return types for error handling.
- Use `clap` derive macros. Every CLI argument has both a short and a long option.
- Use descriptive names, no single character variables, and full names over abbreviations.
- Performance targets are Apple Silicon NEON (M1 and newer) and x86-64 with AVX2 as the minimum.
  Memory use is not an optimization goal, so trade memory for speed freely. See "Performance targets" in `AGENTS.md`.

## Shell scripts

- Keep scripts in separate files, formatted with `shfmt` and linted with `shellcheck -x`.
- Source `common.sh` for shared helpers.
- Start every script with a short comment explaining what it does,
  and put a brief doc comment above every function.
