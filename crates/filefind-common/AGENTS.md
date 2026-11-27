# filefind-common agent notes

Shared library of the filefind crates.
filefind is a fast file indexing and search tool, built first for Windows.
It reads the NTFS Master File Table (MFT) directly for fast initial scans,
and follows the USN journal for incremental change detection.

The general workspace rules in the root `AGENTS.md` apply here.
The other filefind crates have their own `AGENTS.md`:
`crates/filefind-cli`, `crates/filefind-daemon`, and `crates/filefind-tray`.

## Modules

- `lib.rs` - Library root, re-exports, path classification, and drive mapping helpers
- `config.rs` - User configuration file handling
- `database.rs` - `SQLite` index database operations
- `database/schema.rs` - Versioned index schema, migrated as the `filefind_index` component of `cli-tools-db`
- `index_updates.rs` - `IndexUpdater`, which applies the moves and removals other tools report as `PathChange`
- `ipc.rs` - Daemon IPC client and protocol, behind the `ipc` feature
- `test_support.rs` - `IndexFixture` for tests of tools that update the index, behind the `test-support` feature
- `types.rs` - File entries, volumes, and file change events

## Database

- `SQLite` stores the index, by default in `filefind/filefind.db` under the local data directory.
- Files are keyed by their full path, with a unique index on `full_path` and a case-insensitive index on `name`.
  Directories are ordinary rows with `is_directory` set.
- Volume serial numbers identify drives, so a reconnected drive keeps its entries.
- The schema version is recorded in `schema_migrations` through `cli_tools_db::sqlite::migrate_component`.
  Bump `INDEX_SCHEMA_VERSION` and add a migration step for every schema change.
- `update_file_path()` updates a file's stored path after a move and clears its MFT reference.
- `rebase_files_under_path()` moves a directory and every entry under it in one statement.
- The CLI and the daemon open the database directly. WAL mode lets the CLI write while the daemon runs.

## IPC

- Named pipes on Windows, Unix domain sockets on other platforms.
- Postcard serialized commands and responses, framed with a two byte length prefix.
- Commands: stop, status, rescan, pause, resume, ping, and prune.
- The client in `ipc.rs` is used by the tray app.
  The server lives in `crates/filefind-daemon/src/ipc_server.rs`.

## Configuration

User configuration is read from `~/.config/filefind.toml`,
or from the file the `FILEFIND_CONFIG` environment variable names.
`FILEFIND_DB` overrides the index database path.
Tool tests set it, and `FILEFIND_CONFIG` to a missing file, so they never touch the user's real index.
See `filefind.toml` in the repo root for an example.
Remember to update the example config file when adding new config options.

### Daemon options

- `paths` - Drives, directories, or network paths to index
- `exclude` - Directories to exclude from indexing
- `exclude_patterns` - Glob patterns for files to exclude
- `scan_interval_seconds` - Rescan interval for non-NTFS drives in seconds
- `log_level` - Log level: error, warn, info, debug, trace
- `verbose` - Enable verbose output
- `database_path` - Custom database file location
- `force_clean_scan` - Force clean scan mode, which deletes all entries before inserting

### CLI options

- `format` - Output format: "list", "name", "grouped", or "info"
- `max_results` - Maximum results to show
- `color` - Enable colored output
- `case_sensitive` - Case-sensitive search by default
- `show_hidden` - Show hidden files in results
- `hyperlinks` - Emit OSC 8 terminal hyperlinks: "auto", "always", or "never"
- `hyperlink_scheme` - URI scheme used for hyperlinks, "file" by default

## Index updates from other tools

cli-tools tools that rename, move, or delete files report them as `cli_tools_common::PathChange`
and apply them with `update_default_index` at the end of a run, printing the warnings it returns.
The updater only moves and removes entries that are already indexed and never adds new ones,
since the daemon's exclude rules decide what belongs in the index.
A move out of the configured `paths` removes the entries, and a change the daemon already applied changes nothing.
