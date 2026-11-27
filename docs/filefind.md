# filefind

Fast file indexing and search tool,
built first for Windows with directory walking and native watching on macOS and Linux.

On Windows, `filefindd` uses NTFS Master File Table (MFT) direct reading for fast initial indexing
and USN Journal monitoring for incremental updates.
On macOS and Linux, `filefindd` walks explicit configured directories and monitors them with FSEvents or inotify.

## Features

- **Fast NTFS scanning on Windows**: Reads the MFT directly, bypassing Windows file APIs
- **Real-time updates**: Monitors the USN Journal on Windows NTFS drives
  and uses native watchers (FSEvents, inotify, ReadDirectoryChangesW) elsewhere
- **Instant search**: Queries millions of indexed files in `SQLite`
- **Flexible search**: Supports substring, glob patterns (`*` and `?`), and regular expressions
- **Smart pattern expansion**: Automatically searches for "some.name", "some name", and "somename"
- **Background daemon**: Runs quietly and keeps the index up to date across live changes and restarts
- **File moving**: Moves matching files to a directory with progress,
  disk space checks, cross-tool locks, and graceful abort
- **Clickable results**: Wraps result paths in OSC 8 terminal hyperlinks that open in the default application
- **Cross-tool index updates**:
  `dirmove`, `dots`, `dupefind`, `flipdate`, `qtorrent`, `rxrename`, `vconvert`, and `vres` update an existing index
  after each run

## Components

filefind is made of four crates in the cli-tools workspace:

- **filefind-common**: Shared library with database schema, path scope rules, configuration, external index updates,
  and optional `clap`, `ipc`, and `logging` adapters
- **filefind-daemon**: Background service (`filefindd`) that indexes and monitors file systems
- **filefind-cli**: Command-line interface (`filefind`) for searching the file index and moving matching files
- **filefind-tray**: System tray and macOS menu bar application (`filefind-tray`) for daemon control

## Installation

### Build from source

```shell
# Build all binaries
./build.sh

# Install to Cargo bin directory
./install.sh
```

On Linux, building `filefind-tray` requires the GTK 3 and AppIndicator development libraries:

```shell
sudo apt-get install -y libgtk-3-dev libayatana-appindicator3-dev libxdo-dev
```

## Quick start on macOS and Linux

On macOS and Linux, `filefindd` requires explicit absolute directories in `~/.config/filefind.toml`
so personal files are never scanned silently.
No administrator privileges are needed.

```shell
# 1. Create a minimal config with explicit absolute directories
mkdir -p ~/.config
cat > ~/.config/filefind.toml <<'EOF'
[daemon]
paths = ["/Users/you/Documents", "/Users/you/Projects"]
exclude = ["node_modules", "target", ".git"]
exclude_patterns = ["*.tmp", "~$*", ".DS_Store"]
EOF

# 2. Verify the configured roots are accessible
filefindd detect

# 3. Perform an initial scan (or scan any directory directly without a config)
filefindd scan

# 4. Search the index
filefind "README"
filefind "*.pdf"

# 5. Start the background daemon, check status, and stop it cleanly
filefindd start
filefindd status
filefindd stop
```

## Usage

### Daemon

```shell
# Start in background (spawns detached process)
filefindd start

# Start in foreground (stays attached to terminal)
filefindd start -f

# Start with forced full rescan
filefindd start -r

# Check daemon status
filefindd status

# Stop the daemon
filefindd stop

# Trigger a one-time scan of all configured roots
filefindd scan

# Scan a specific path directly
filefindd scan "/Users/you/Projects"

# Force a clean scan (delete existing entries before inserting new ones)
filefindd scan --force

# Show index statistics
filefindd stats

# List indexed volumes
filefindd volumes
filefindd volumes --detailed

# Detect NTFS drives on Windows or verify configured roots on Unix
filefindd detect

# Prune confirmed-missing entries (preserves permission-denied entries and offline roots)
filefindd prune

# Reset (delete) the database
filefindd reset
```

### CLI

Search for files:

```shell
# Basic search (auto-expands patterns: "some.name" also searches "some name" and "somename")
filefind "document.pdf"

# Glob pattern search
filefind "*.mp4"

# Regex search
filefind -r "IMG_\d{4}\.jpg"

# Exact pattern matching (disable auto-expansion)
filefind -e "some.name"

# Case-sensitive search
filefind -c "README"

# Search in specific drives (Windows)
filefind -d C -d D "project"

# Show only files
filefind -f "config.toml"

# Show only directories
filefind -D "projects"

# Move matching files to a directory (matches already under it stay in place)
filefind "*.mp4" --move /path/to/Videos

# Move with force overwrite of existing files
filefind "*.mp4" --move /path/to/Videos --force

# List output (one full path per line)
filefind -l "*.mp4"
filefind -o list "*.mp4"

# Name-only output (filenames without directory paths)
filefind -N "*.mp4"
filefind -o name "*.mp4"

# Info output (paths with file sizes)
filefind -i "*.mp4"
filefind -o info "*.mp4"

# Limit files shown per directory
filefind -n 10 "*.txt"

# Force clickable hyperlinks even when output is piped or redirected
filefind -L always "*.mp4"

# Disable clickable hyperlinks
filefind -L never "*.mp4"

# Show index statistics
filefind stats

# List all indexed volumes
filefind volumes

# Find duplicate files sharing the same case-insensitive stem
filefind duplicates

# Generate shell completion (bash, zsh, fish, powershell)
filefind completion zsh
filefind completion powershell

# Install shell completion to standard location
filefind completion zsh --install
filefind completion powershell --install
```

### System tray and menu bar application

The tray application provides a minimal interface to control the daemon:

```shell
# Start detached in the background
filefind-tray

# Run in the foreground with console logging
filefind-tray --foreground
```

Features:

- **Status indicator**: Icon color shows daemon state (green for running, gray for stopped, orange for scanning)
- **Tooltip**: Shows indexed file and directory counts
- **Menu options**: Start daemon, Stop daemon, Rescan, About Filefind,
  and Quit (Quit closes only the tray and leaves a running daemon intact)

On macOS, `filefind-tray` runs as an accessory menu bar app (no Dock icon),
locates the sibling `filefindd` binary next to itself without relying on `PATH`,
and opens its menu on a normal click.
On Linux, `filefind-tray` runs the GTK event loop required by AppIndicator and requires an active graphical session.

## Runtime paths and permissions

| Item | Windows | macOS | Linux | Override |
| --- | --- | --- | --- | --- |
| Config file | `~/.config/filefind.toml` | `~/.config/filefind.toml` | `~/.config/filefind.toml` | `FILEFIND_CONFIG` |
| Database | `%LOCALAPPDATA%\filefind\filefind.db` | `~/Library/Application Support/filefind/filefind.db` | `${XDG_DATA_HOME:-~/.local/share}/filefind/filefind.db` | `FILEFIND_DB` or `[daemon].database_path` |
| IPC endpoint | `\\.\pipe\filefind-daemon` | `~/Library/Caches/filefind/filefind-daemon.sock` | `${XDG_RUNTIME_DIR:-${XDG_CACHE_HOME:-~/.cache}}/filefind/filefind-daemon.sock` | `FILEFIND_IPC_PATH` |
| Daemon logs | `~/logs/filefind/` | `~/logs/filefind/` | `~/logs/filefind/` | Only written when running detached |

On Unix, `filefindd` creates its private socket directory with mode `0700`,
restricts the socket file to mode `0600`,
and holds an exclusive lock file next to the socket for the lifetime of the daemon
so a second instance cannot replace a live daemon's socket.

## Configuration

Configuration is read from `~/.config/filefind.toml` or `FILEFIND_CONFIG`.
Malformed TOML fails fast with the file path and parse error.
See `filefind.toml` in the repository root for a complete example.

```toml
[daemon]
# Windows: drive letters, directories, or UNC paths. Empty auto-detects local NTFS drives.
# macOS and Linux: explicit absolute directories are required.
paths = ["/Users/you/Documents", "/Users/you/Projects"]

# Directories and path components to exclude from indexing.
# Plain entries match whole path components. Use "*text*" to match any substring.
exclude = ["node_modules", "target", ".git"]

# Filename glob patterns to exclude from indexing.
exclude_patterns = ["*.tmp", "~$*", "Thumbs.db", "*.parts"]

# Periodic rescan interval for non-NTFS and walked roots in seconds.
scan_interval_seconds = 3600

# Log level: "error", "warn", "info", "debug", "trace"
log_level = "info"

# Force clean scan (delete existing entries before inserting new ones).
# force_clean_scan = false

[cli]
# Default output format: "list", "name", "grouped", or "info"
format = "grouped"

# Maximum number of results to show (0 = unlimited)
max_results = 100

# Enable colored output
color = true

# Case-sensitive search by default
case_sensitive = false

# Show hidden files in results
show_hidden = false

# Emit clickable terminal hyperlinks for result paths: "auto", "always", or "never"
hyperlinks = "auto"

# URI scheme used for terminal hyperlinks
hyperlink_scheme = "file"
```

## How it works

### Windows NTFS (MFT and USN Journal)

On Windows NTFS drives, `filefindd` reads the Master File Table directly from disk
and monitors the USN Journal for incremental changes.
Administrator privileges are required on Windows for MFT and USN Journal access.

### macOS, Linux, and non-NTFS drives

On macOS, Linux, network shares, and non-NTFS drives,
`filefindd` walks configured directories concurrently and follows live changes with native watchers
(FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on Windows).
Walked roots are reconciled on startup and periodically at `scan_interval_seconds`,
and healthy watchers stay armed across scans so queued changes are retained and replayed.

### Pattern expansion

When searching without glob or regex mode, filefind automatically expands dot-separated patterns.
For example, searching for "some.name" also searches for "some name" and "somename".
Use `-e` (exact) mode to disable this.

### Clickable results

Result paths are wrapped in OSC 8 terminal hyperlinks.
Ctrl+Click (Windows Terminal, Warp) or Cmd+Click (iTerm2)
opens the file with the application the operating system associates with its type,
and directories open in the file manager.
Use `-L always` to force links (for example when paging with `less -R`)
and `-L never` when a terminal or multiplexer does not handle them.

## Index safety and recovery

Search rendering is strictly read-only.
Inaccessible or missing directories are highlighted in red without deleting database rows,
so transient network timeouts or permission errors never purge indexed subtrees.
Explicit pruning (`filefindd prune`) removes only entries confirmed absent (`NotFound`),
preserving permission-denied subtrees and offline or unmounted volume roots.

Configured paths are scanned independently, and unreadable child subdirectories are logged and skipped
without discarding the rest of the root scan.
Failed scans retain their previous entries and USN position, and the daemon retries reconciliation while running.
Watcher overflow flags and failed event applications trigger automatic reconciliation.

Forced moves (`filefind --move --force`) compare BLAKE3 hashes only between independent regular files
and use entry-aware collision checks so symlinks and dangling destinations are never clobbered or left dangling.
Stored database keys stay separate from mapped display paths so UNC-mapped moves reconcile cleanly.

## Development

### Benchmarks and testing

```shell
# Run database Criterion benchmarks
cargo bench -p filefind-common --bench database -- --quick

# Run the elevated Windows NTFS tests in disposable temporary directories
cargo test -p filefind-daemon test_live_ -- --ignored

# Run the isolated Unix daemon lifecycle integration tests (macOS and Linux)
cargo test -p filefind-daemon --test unix_daemon_lifecycle_tests
```

CI builds, lints, and tests the workspace on Linux, macOS, and Windows.

## License

MIT
