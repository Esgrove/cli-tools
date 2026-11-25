# Agent instructions

## Project Overview

This is a Rust workspace containing multiple CLI utility tools.
Each tool is its own crate under `crates/` with a package name prefixed by `cli-tools-`.
Shared helpers live in the `cli-tools-common` and `cli-tools-media` library crates.
Tools with reusable logic expose it as a library from the same crate,
so other projects can depend on just that crate and extend it.
Such a crate puts the dependencies only its binary uses behind a default `cli` feature,
lists it under `required-features` for the binary and the tests that run it,
and a library user turns it off with `default-features = false`.

## Build and Test Commands

After making code changes, always run:

```shell
cargo clippy --fix --allow-dirty --workspace
cargo clippy --fix --allow-dirty --workspace --tests
cargo fmt
cargo clippy --workspace --all-targets -- -Dwarnings
cargo test --workspace
```

### Other commands

```shell
# Build all binaries
cargo build --workspace

# Build a specific binary
cargo build --bin <name>

# Run a specific binary
cargo run --bin <name> -- [args]

# Format code
cargo fmt

# Run tests with standard cargo test (faster)
cargo test --workspace

# Run tests for a single crate
cargo test -p cli-tools-<name>

# Run tests with nextest (slower, better output)
cargo nextest run --workspace

# Run tests with coverage report (text output)
cargo llvm-cov nextest --workspace

# Run clippy, all tests, and every fuzz target, see ./test.sh -h
./test.sh

# Run the property tests with more generated cases
PROPTEST_CASES=20000 cargo test --workspace property

# Fuzz one target with the nightly toolchain
cd fuzz && cargo +nightly fuzz run slb_format -- -max_total_time=300

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
Benchmark files live in the `benches/` directory of the crate they measure.

The core algorithmic functions benchmarked for `dirmove`
and `dupefind` live in the library part of those crates
(`crates/dirmove/src/` and `crates/dupefind/src/`)
so that benchmarks can import them directly without duplicating code.

### Benchmark structure

- `crates/common/benches/date.rs` - Date parsing and reordering
- `crates/common/benches/lib.rs` - Shared utility functions
- `crates/common/benches/simd.rs` - SIMD byte scanning kernels against the scalar code they replace,
  from token to file length
- `crates/dirmove/benches/dir_move.rs` - Prefix grouping, file matching, contiguity checks
- `crates/dupefind/benches/dupe_find.rs` - Stem normalization, duplicate grouping, regex matching
- `crates/dots/benches/format.rs` - Dot-rename formatting pipeline
- `crates/media/benches/resolution.rs` - Resolution labeling and regex matching
- `crates/slb/benches/semantic_line_breaks.rs` - Prose tokenizing, boundary detection, paragraph reflow,
  comment scanning, and whole file checking and formatting

### SIMD benchmarks

`crates/common/src/simd.rs` detects the best SIMD level once per process.
`./simd-bench.sh` runs the benchmarks that go through the kernels and writes a summary table to `target/`.
See `docs/simd.md` for the evaluation and its results.

### Adding new benchmarks

When extracting algorithmic code from a binary for benchmarking,
move the pure functions and types to the library part of the tool's crate
and have the binary import them from there.
Do not duplicate code in benchmark files.

## Project Structure

The root `Cargo.toml` is a virtual workspace that holds the shared package metadata,
dependency versions, lints, and profiles.
Every crate uses `workspace = true` for these.
A crate with both a library and a binary keeps the library in `src/lib.rs`
and the binary in `src/bin/<name>/main.rs`.
A binary only crate keeps its binary in `src/main.rs`.

- `crates/common/` → `cli-tools-common` - Shared helpers used by every tool:
    - `lib.rs` - Print macros like `print_error!`, path resolution, formatting, user config path, shell completion
    - `utils.rs` - Shared path and text helpers (path to display string, relative paths, glob to regex)
    - `file_handling.rs` - Same file detection, collision aware and case only renames, batch target reservations,
      atomic file writes, and recursive directory copy
    - `diff.rs` - Coloured diff rendering (`color_diff` and `show_diff` for renames, `diff_lines` for files)
    - `date.rs` - Date parsing and formatting utilities
    - `simd.rs` - SIMD byte scanning kernels on `fearless_simd`
    - The `completion` feature adds `completion.rs` with shell completion generation,
      so library crates that take no command line arguments avoid clap
    - The `async` feature adds the tokio based directory entry helpers and the I/O semaphore
    - The `diff` feature adds `diff.rs`, so crates that do not render diffs avoid the `difference` dependency
    - The `test-support` feature adds `test_support.rs` with `TempTree`,
      which builds nested temporary trees and snapshots them for tests
- `crates/media/` → `cli-tools-media` - Media helpers:
    - `resolution.rs` - Video resolution parsing and labelling
    - `video_info.rs` - Video metadata from ffprobe
    - `file_hash.rs` - File hashing
    - `scan_cache.rs` - `SQLite` cache for directory scan results, behind the `scan-cache` feature
- `crates/dirmove/` → `dirmove` - Move files to matching directories.
  The binary holds the CLI arguments and subcommands, the run config, prompts, printing, progress bars,
  and the directory name database.
  The library reads no CLI arguments and prints nothing, reporting progress through observer traits instead.
  The library modules:
    - `roots.rs` - Input and output roots, root resolution, and display paths
    - `name_filter.rs` - Ignored prefix, ignored group name, and unwanted directory rules
    - `scanner.rs` - Scan options and collecting input files and directories and the candidate destination directories
    - `matcher.rs` - Matching files and directories to destination directories, and custom mappings
    - `candidates.rs` - File name parts and the prefix candidates that file names share
    - `grouping.rs` - Grouping options and grouping files by shared name prefixes for create mode
    - `prefix_index.rs` - Sorted prefix index used to find prefix candidates
    - `unpack.rs` - Deleting unwanted directories and unpacking configured directory names,
      reported to an `UnpackObserver`
    - `merge.rs` - Merging directory contents and finding prefixed directory merge pairs,
      reported to a `MergeObserver`
    - `mover.rs` - Reliable file moves with copy verification, reported to a `MoveObserver`
    - `types.rs` - The destination directory information shared by scanning and matching
    - `utils.rs` - Name normalization and filtering
- `crates/dots/` → `dots` - Rename files to use dot formatting. The library holds formatting and renaming.
- `crates/dupefind/` → `dupefind` - Find duplicate files. The library holds normalization and grouping.
- `crates/slb/` → `slb` - Check and format prose with semantic line breaks. The library modules:
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
- `crates/xtask/` → `xtask` - Parallel task runner driven by an `xtask.toml` task file.
  The library holds the `Runner` builder that projects extend with Rust builtins.
  See `crates/xtask/README.md` for the module layout.
- `crates/rsbump/` → `rsbump` - Bump Rust projects to the local toolchain version,
  pin `rust` Docker images to match, and optionally upgrade, clean, verify, and commit.
  The binary holds the CLI arguments, the run config, and printing the reports.
  The library modules:
    - `discovery.rs` - Finding projects, workspace member manifests, and Dockerfiles
    - `manifest.rs` - Rust versions, the local toolchain version, and the `rust-version` edit
    - `dockerfile.rs` - Pinning `rust` images in Dockerfiles
    - `registry.rs` - Docker Hub digest lookup
    - `project.rs` - The per-project steps, recorded as report entries, and processing projects in parallel
- `crates/qtorrent/` → `qtorrent` - Add torrents to qBittorrent with automatic file renaming, show torrent stats.
  The binary holds the CLI arguments, the user config, the interactive add workflow with its prompts,
  moving and trashing torrent files, the info subcommand, and all printing.
  The library modules:
    - `qbittorrent.rs` - qBittorrent `WebUI` API client and its response types
    - `connection.rs` - Connection settings and opening an authenticated session
    - `torrent.rs` - Bencode torrent parsing, info hash, and file filtering
    - `utils.rs` - Parsed torrent info, name formatting helpers, and the downloaded directory helpers
    - `naming.rs` - Naming options and the `NameSuggester` for suggested, internal, and custom names
    - `tags.rs` - Default tags and the prefix and path tag overwrite rules
    - `collector.rs` - Collecting torrent files from the input paths with include and exclude globs
    - `add.rs` - Parsing torrents, finding existing ones, and the `TorrentAdder`, reported to an `AddObserver`
    - `add/file_updates.rs` - Dots renaming the files of an added torrent and skipping excluded files
    - `statistics.rs` - Torrent status classification, sorting, and per status counts and sizes
    - `stats.rs` - Outcome counters of one add run
- Binary only crates:
    - `crates/div/` → `div` - Print divider comments
    - `crates/flipdate/` → `flipdate` - Flip dates in filenames
    - `crates/rxrename/` → `rxrename` - Remove a trailing "_1" from file names
    - `crates/thumbs/` → `thumbs` - Create video thumbnail sheets
    - `crates/vconvert/` → `vconvert` - Video conversion to HEVC/MP4
    - `crates/vres/` → `vres` - Add video resolution to file names
    - `crates/vstats/` → `vstats` - Collect and print video file statistics
    - `crates/visaparse/` → `visaparse` - Parse Finvoice XML credit card statements and collect data
    - `crates/vtag/` → `vtag` - Create git version tags for a project (Rust, C++, Python)

Give each crate a short `README.md`.
Add a new tool as a new crate under `crates/`,
and only depend on the workspace crates and dependencies it actually uses,
so a project that pulls in one tool does not build the others.

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
  A helper that is not specific to the tool belongs in a shared library crate
  such as `crates/common/src/utils.rs` or `crates/common/src/diff.rs`, never inside a binary.
- Keep an item's visibility as narrow as it can be.
  A helper that only crosses into a sibling module of the same parent is `pub(super)`, not `pub`.

## Code Style and Conventions

- Uses Rust 2024 edition
- Clippy is configured with pedantic and nursery lints enabled
- Do not use plain unwrap in non-test code. Use proper error handling, or `.expect()` in constants.
  Tests may use `unwrap()`.
  Use `.expect()` in a test only when its message adds real diagnostic value without making the test much longer.
- Use `anyhow` for error handling with `Result<T>` return types
- Use `clap` with derive macros for CLI argument parsing
- Every CLI argument must provide both a short and a long option
- Use `colored` crate for terminal output coloring
- Common helper functions and macros like `print_error!`
  and `print_yellow!` are defined in `crates/common/src/lib.rs`
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

- Cover happy paths, edge cases, and nested directory trees.
  Build trees with `cli_tools_common::test_support::TempTree`,
  from the `test-support` feature of `cli-tools-common` enabled as a dev-dependency.
- Property tests use `proptest`. Put them in `tests/<crate>_property_tests.rs`,
  or in a root-level `test_*_properties` module when they need private items.
  Turn every shrunk failure into an example unit test in the owning module.
- Fuzz targets live in `fuzz/fuzz_targets/`, a separate nightly workspace. See `fuzz/README.md`.
- **NEVER use nested modules inside test modules** - all test modules must be separate root-level `#[cfg(test)]` modules
- Do NOT wrap test modules in a single parent `mod tests` module
- When a split leaves several files sharing test helpers,
  put the helpers in a `#[cfg(test)]` gated module declared from `mod.rs`,
  which is still a root-level `#[cfg(test)]` module.
  See `crates/slb/src/test_helpers.rs`.

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
