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

# Run all benchmarks
cargo bench

# Run a specific benchmark suite
cargo bench --bench date
cargo bench --bench dir_move
cargo bench --bench dupe_find
cargo bench --bench format
cargo bench --bench lib
cargo bench --bench resolution

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

Benchmarks use [Criterion.rs](https://github.com/criterion-rs/criterion.rs) for
statistically rigorous microbenchmarking. Benchmark files live in `benches/`.

The core algorithmic functions benchmarked for `dir_move` and `dupe_find` are
extracted into library modules (`src/dir_move/` and `src/dupe_find/`) so that
benchmarks can import them directly without duplicating code.

### Benchmark structure

- `benches/date.rs` - Date parsing and reordering
- `benches/dir_move.rs` - Prefix grouping, file matching, contiguity checks
- `benches/dupe_find.rs` - Stem normalization, duplicate grouping, regex matching
- `benches/format.rs` - Dot-rename formatting pipeline
- `benches/lib.rs` - Shared utility functions
- `benches/resolution.rs` - Resolution labeling and regex matching

### Adding new benchmarks

When extracting algorithmic code from a binary for benchmarking, move the pure
functions and types to a library module under `src/` and have the binary
re-export from the library. Do not duplicate code in benchmark files.

## Project Structure

- `src/lib.rs` - Shared library code (utilities, macros, common functions)
- `src/config.rs` - User configuration file handling
- `src/date.rs` - Date parsing and formatting utilities
- `src/dir_move/` - Algorithmic types and functions for dir_move (prefix grouping, matching)
- `src/dupe_find/` - Algorithmic types and functions for dupe_find (normalization, grouping)
- `src/bin/` - Individual CLI tool binaries:
    - `dir_move.rs` → `dirmove` - Move files to matching directories
    - `divider.rs` → `div` - Print divider comments
    - `dots.rs` → `dots` - Rename files to use dot formatting
    - `flip_date.rs` → `flipdate` - Flip dates in filenames
    - `qtorrent` → `qtorrent` - Add torrents to qBittorrent with automatic file renaming, show torrent stats
    - `resolution.rs` → `vres` - Add video resolution to file names
    - `version_tag.rs` → `vtag` - Create git version tags for Rust projects
    - `video_convert` → `vconvert` - Video conversion to HEVC/MP4
    - `video_stats` → `vstats` - Collect and print video file statistics
    - `visa_parse.rs` → `visaparse` - Parse Finvoice XML credit card statements and collect data

## Code organization

- All enums before structs
- Put all struct definitions before any implementations.
- Implementations only after last struct definition in the order of struct definitions.
- Functions after implementations
- In implementations, Order public methods before private methods
- In implementations, put associated functions last

## Code Style and Conventions

- Uses Rust 2024 edition
- Clippy is configured with pedantic and nursery lints enabled
- Do not use plain unwrap. Use proper error handling or `.expect()` in constants and test cases.
- Use `anyhow` for error handling with `Result<T>` return types
- Use `clap` with derive macros for CLI argument parsing
- Use `colored` crate for terminal output coloring
- Common helper functions and macros like `print_error!` and `print_warning!` are defined in `src/lib.rs`
- Use descriptive variable and function names. No single character variables.
- Prefer full names over abbreviations. For example: `directories` instead of `dirs`.
- Create docstrings for structs and functions.
- Avoid trailing comments.

## Testing

- **NEVER use nested modules inside test modules** - all test modules must be separate root-level `#[cfg(test)]` modules
- Do NOT wrap test modules in a single parent `mod tests` module

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
