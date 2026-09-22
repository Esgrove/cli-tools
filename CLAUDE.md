# Agent instructions

## Project Overview

This is a Rust project containing multiple CLI utility tools.
Each tool is a separate binary defined in `src/bin/`.
The shared library code lives in `src/lib.rs` and related modules.

## Build and Test Commands

After making code changes, always run:

```shell
cargo clippy --fix --allow-dirty
cargo clippy --fix --allow-dirty --tests
cargo fmt
cargo test
```

### Other commands

```shell
# Build all binaries
cargo build

# Build a specific binary
cargo build --bin <name>

# Run a specific binary
cargo run --bin <name> -- [args]

# Format code
cargo fmt

# Run tests with standard cargo test (faster)
cargo test

# Run tests with nextest (slower, better output)
cargo nextest run

# Run tests with coverage report (text output)
cargo llvm-cov nextest

# Build with debug symbols for a profiler, since the release profile carries none
cargo build --profile profiling --bin <name>

# Run all benchmarks
cargo bench

# Run a specific benchmark suite
cargo bench --bench date
cargo bench --bench dir_move
cargo bench --bench dupe_find
cargo bench --bench format
cargo bench --bench lib
cargo bench --bench resolution
cargo bench --bench semantic_line_breaks
cargo bench --bench simd

# Run benchmarks matching a filter pattern
cargo bench -- "normalize_stem"

# Quick benchmark run (fewer iterations, faster feedback)
cargo bench -- --quick
```

### Required tools

Install these tools for testing:

```shell
cargo install cargo-nextest
cargo install cargo-llvm-cov
```

## Benchmarks

Benchmarks use [Criterion.rs](https://github.com/criterion-rs/criterion.rs) for statistically rigorous microbenchmarking.
Benchmark files live in `benches/`.

The core algorithmic functions benchmarked for `dir_move` and `dupe_find` are extracted into library modules
(`src/dir_move/` and `src/dupe_find/`) so that benchmarks can import them directly without duplicating code.

### Benchmark structure

- `benches/date.rs` - Date parsing and reordering
- `benches/dir_move.rs` - Prefix grouping, file matching, contiguity checks
- `benches/dupe_find.rs` - Stem normalization, duplicate grouping, regex matching
- `benches/format.rs` - Dot-rename formatting pipeline
- `benches/lib.rs` - Shared utility functions
- `benches/resolution.rs` - Resolution labeling and regex matching
- `benches/semantic_line_breaks.rs` - Prose tokenizing, boundary detection, paragraph reflow,
  comment scanning, and whole file checking and formatting
- `benches/simd.rs` - SIMD byte scanning kernels against the scalar code they replace, from token to file length

### SIMD benchmarks

`src/simd.rs` reads the `CLI_TOOLS_SIMD` environment variable once per process.
"scalar" runs plain loops, "avx2" and the other x86 level names cap the SIMD level, and anything else auto detects.
`./simd-bench.sh` runs the affected benchmarks once per mode and writes a comparison table to `target/`.
See `docs/simd.md` for the evaluation and its results.

### Adding new benchmarks

When extracting algorithmic code from a binary for benchmarking,
move the pure functions and types to a library module under `src/` and have the binary re-export from the library.
Do not duplicate code in benchmark files.

## Project Structure

- `src/lib.rs` - Library root: module declarations, re-exports, and the shared helpers not yet split out
- `src/utils.rs` - Shared path and text helpers (path to display string, relative paths, glob to regex)
- `src/diff.rs` - Coloured diff rendering (`color_diff` and `show_diff` for renames, `diff_lines` for files)
- `src/date.rs` - Date parsing and formatting utilities
- `src/file_hash.rs` - File hashing
- `src/resolution.rs` - Video resolution parsing and labelling
- `src/scan_cache.rs` - Cache for directory scan results
- `src/simd.rs` - SIMD byte scanning kernels on `fearless_simd` with a scalar mode for comparison
- `src/video_info.rs` - Video metadata from ffprobe
- `src/dir_move/` - Algorithmic types and functions for dirmove (prefix grouping, matching)
- `src/dot_rename/` - Algorithmic types and functions for dots (formatting, renaming)
- `src/dupe_find/` - Algorithmic types and functions for dupefind (normalization, grouping)
- `src/semantic_line_breaks/` - Algorithmic types and functions for slb:
    - `options.rs` - Rule set and the options controlling checking and formatting
    - `paragraph.rs` - Hard break markers and the paragraph built from prose lines
    - `violation.rs` - Violation categories and the records the checker reports
    - `file_kind.rs` - File kind detection and the comment syntax each kind uses
    - `formatter.rs` - The check and fix pipeline, the public entry points
    - `tokenizer.rs` - Splitting a prose line into words and unbreakable atoms
    - `token.rs` - Prose token kind and the token built from one line
    - `boundaries.rs` - Sentence and clause boundary ranking, bracket and emphasis nesting
    - `rank.rs` - Break candidate ranking used to choose where a line breaks
    - `line_breaks.rs` - Break point planning and the cost model
    - `list_runs.rs` - Suppressing breaks inside single word list runs
    - `rewording.rs` - Segment building, semicolon and dash rewording
    - `reflow.rs` - Paragraph reflow, the entry point the formatter calls per paragraph
    - `comments.rs` - Comment block and trailing comment extraction
    - `docstrings.rs` - Python docstring parsing and quote placement
    - `scanner.rs` - String aware line scanner
    - `regex_literals.rs` - Regex literal versus division detection
    - `string_syntax.rs` - Per language string and comment syntax table
    - `markdown.rs` - Markdown aware paragraph splitting
    - `looks_like_code.rs` - Heuristic that treats a content line as source code
    - `line_ranges.rs` - Line selection ranges used to scope checking and fixing
    - `project_config.rs` - Line width discovery from project config files
    - `test_helpers.rs` - Shared builders for the prose unit tests
- `src/bin/` - Individual CLI tool binaries, each a directory with a `main.rs` unless noted:
    - `dir_move/` → `dirmove` - Move files to matching directories
    - `divider.rs` → `div` - Print divider comments
    - `dots/` → `dots` - Rename files to use dot formatting
    - `dupe_find/` → `dupefind` - Find duplicate files
    - `flip_date/` → `flipdate` - Flip dates in filenames
    - `qtorrent/` → `qtorrent` - Add torrents to qBittorrent with automatic file renaming, show torrent stats
    - `rx_rename.rs` → `rxrename` - Rename files with a regular expression
    - `semantic_line_breaks/` → `slb` - Check and format prose with semantic line breaks
    - `thumbnail/` → `thumbs` - Create video thumbnail sheets
    - `version_tag.rs` → `vtag` - Create git version tags for a project (Rust, C++, Python)
    - `video_convert/` → `vconvert` - Video conversion to HEVC/MP4
    - `video_resolution/` → `vres` - Add video resolution to file names
    - `video_stats/` → `vstats` - Collect and print video file statistics
    - `visa_parse/` → `visaparse` - Parse Finvoice XML credit card statements and collect data

## Code organization

- All enums before structs
- Put all struct definitions before any implementations.
- Implementations only after last struct definition in the order of struct definitions.
- Functions after implementations
- In implementations, put constructors first: `new` first, then other associated functions that return `Self`
  (`default`, `from_*`, and similar).
- After constructors, order public methods before private methods.

### File size

- Once a source file passes 1000 lines, split it into smaller modules.
  Going over 1000 is fine when most of the file is test code.
- Split along the concerns the file has grown into, one file per concern,
  and give each new file its own `//!` documentation naming that concern.
- Extract helper and unrelated parts instead of growing one file.
  A helper that is not specific to the tool belongs in a shared library module
  such as `src/utils.rs` or `src/diff.rs`, never inside a binary under `src/bin/`.
- Keep an item's visibility as narrow as it can be.
  A helper that only crosses into a sibling module of the same parent is `pub(super)`, not `pub`.

## Code Style and Conventions

- Uses Rust 2024 edition
- Clippy is configured with pedantic and nursery lints enabled
- Do not use plain unwrap. Use proper error handling or `.expect()` in constants and test cases.
- Use `anyhow` for error handling with `Result<T>` return types
- Use `clap` with derive macros for CLI argument parsing
- Every CLI argument must provide both a short and a long option
- Use `colored` crate for terminal output coloring
- Common helper functions and macros like `print_error!` and `print_warning!` are defined in `src/lib.rs`
- Use descriptive variable and function names. No single character variables.
- Prefer full names over abbreviations. For example: `directories` instead of `dirs`.
- Create docstrings for structs and functions.
- Avoid trailing comments.

### Comments and documentation formatting

Every Rust source file must start with module-level `//!` documentation.
Briefly describe the module's purpose, its main contents, and what it implements.
Always add this documentation when creating a new Rust source file.

Use semantic line breaks for comments, docstrings, and documentation.
Start a new line at sentence boundaries or natural clause boundaries.
Target about 120 characters per line, but prefer a clean sentence break over a strict width.
Avoid dashes and semicolons in prose.
Use commas and periods to separate ideas.
Quote literal user input values, for example "y", "yes", "n", "no", and "s".
Do not wrap prose by splitting a sentence at an arbitrary point in the middle.

## Testing

- **NEVER use nested modules inside test modules** - all test modules must be separate root-level `#[cfg(test)]` modules
- Do NOT wrap test modules in a single parent `mod tests` module
- When a split leaves several files sharing test helpers,
  put the helpers in a `#[cfg(test)]` gated module declared from `mod.rs`,
  which is still a root-level `#[cfg(test)]` module.
  See `src/semantic_line_breaks/test_helpers.rs`.

### Test module structure example

```rust
#[cfg(test)]
mod test_prefix_extraction {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn extracts_three_parts() { ... }
}

#[cfg(test)]
mod test_filtering {
    use super::*;

    #[test]
    fn removes_year() { ... }
}
```

## Git Commands

**NEVER run destructive git commands** including but not limited to:

- `git checkout -- <file>` (discards working directory changes)
- `git restore --staged <file>` (unstages changes)
- `git restore <file>` (discards changes)
- `git reset --hard`
- `git clean`
- `git stash drop`

These commands can permanently destroy uncommitted work.
If you need to undo changes, ask the user to do it manually.

## Documentation

When changing CLI arguments or adding new binaries, update the usage output in `README.md`.
Use the short `-h` flag to get concise output and replace the `.exe` suffix with the plain binary name:

```shell
cargo run --bin <name> -- -h
```

## Configuration

User configuration is read from `~/.config/cli-tools.toml` with sections for each binary.
See `cli-tools.toml` in the repo root for an example.
Remember to update the example config file when adding new config options or binaries.
