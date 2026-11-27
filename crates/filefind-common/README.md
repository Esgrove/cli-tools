# filefind-common

Shared library for the filefind file indexing and search system.

This crate provides common functionality used by all filefind crates and by sibling tools that update an existing index:

- **Configuration** (`config`): User configuration file handling (`~/.config/filefind.toml`, `FILEFIND_CONFIG`, `FILEFIND_DB`)
- **Database** (`database`): `SQLite` index database operations and versioned schema migrations via `cli-tools-db`
- **Scope** (`scope`): Shared component-aware root ownership,
  `exclude` and `exclude_patterns` rules, hidden-entry checks, and UTF-8 path validation
- **External index updates** (`index_updates`):
  `IndexUpdater` and `update_default_index` applying `cli_tools_common::PathChange` batches with symlink-alias resolution
- **IPC** (`ipc`, behind the `ipc` feature):
  Daemon IPC client and postcard protocol (Windows named pipes and Unix domain sockets)
- **Types** (`types`): Common data structures (`FileEntry`, `IndexedVolume`, `VolumeType`, `FileChangeEvent`)
- **Test support** (`test_support`, behind the `test-support` feature):
  `IndexFixture` for seeding single-root and multi-root test indexes

## Cargo features

All optional adapters are off by default so index-only consumers avoid extra dependencies:

- `clap`: Derives `clap::ValueEnum` on `HyperlinkMode` and `LogLevel`
- `ipc`: Enables `postcard` serialization and the daemon IPC protocol and client
- `logging`: Enables `tracing_subscriber::filter::LevelFilter` conversions on `LogLevel`
- `test-support`: Enables `IndexFixture` for integration tests

## Crates using `filefind-common`

- [`filefind-cli`](../filefind-cli): Command-line search interface (`features = ["clap"]`)
- [`filefind-daemon`](../filefind-daemon): Background indexing service (`features = ["clap", "ipc", "logging"]`)
- [`filefind-tray`](../filefind-tray): System tray and macOS menu bar application (`features = ["clap", "ipc", "logging"]`)
- External cli-tools binaries (`dirmove`, `dots`, `dupefind`, `flipdate`, `qtorrent`, `rxrename`, `vconvert`, `vres`):
  Default minimal features
