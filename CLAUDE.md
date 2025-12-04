# CLAUDE.md

## Project Overview

This is a Rust project containing multiple CLI utility tools.
Each tool is a separate binary defined in `src/bin/`.
The shared library code lives in `src/lib.rs` and related modules.

## Build and Test Commands

After making code changes, always run:

```shell
cargo clippy --fix --allow-dirty
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
```

## Project Structure

- `src/lib.rs` - Shared library code (utilities, macros, common functions)
- `src/config.rs` - User configuration file handling
- `src/date.rs` - Date parsing and formatting utilities
- `src/bin/` - Individual CLI tool binaries:
    - `divider.rs` → `div` - Print divider comments
    - `dir_move.rs` → `dirmove` - Move directories
    - `dots.rs` → `dots` - Rename files to use dot formatting
    - `flip_date.rs` → `flipdate` - Flip dates in filenames
    - `resolution.rs` → `res` - Add video resolution to file names
    - `video_convert.rs` → `vconvert` - Video conversion to HEVC/MP4
    - `visa_parse.rs` → `visaparse` - Parse Finvoice XML credit card statements and collect data
    - `version_tag.rs` → `vtag` - Create git version tags for Rust projects

## Code organization

- Put all struct definitions before their implementations
- Functions after implementations
- In implementations, Order public methods before private methods
- In implementations, put associated functions last

## Code Style and Conventions

- Uses Rust 2024 edition
- Clippy is configured with pedantic and nursery lints enabled
- `unsafe_code` is forbidden
- `unwrap_used` and `enum_glob_use` are denied
- Use `anyhow` for error handling with `Result<T>` return types
- Use `clap` with derive macros for CLI argument parsing
- Use `colored` crate for terminal output coloring
- Common macros like `print_error!` and `print_warning!` are defined in `src/lib.rs`

## Configuration

User configuration is read from `~/.config/cli-tools.toml` with sections for each binary.
See `cli-tools.toml` in the repo root for an example.
