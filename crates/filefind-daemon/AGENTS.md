# filefind-daemon agent notes

The `filefindd` background service that builds the index and follows file system changes.
See `crates/filefind-common/AGENTS.md` for the database, IPC protocol, and configuration,
and the root `AGENTS.md` for the general workspace rules.

## Modules

- `main.rs` - Daemon entry point and CLI argument handling
- `daemon.rs` - Core daemon logic and lifecycle
- `ipc_server.rs` - IPC server handling the client commands
- `mft.rs` - NTFS MFT reading
- `pruner.rs` - Database pruning for stale entries
- `scanner.rs` - File scanning with MFT reads and directory walking
- `usn.rs` - USN journal monitoring
- `usn_buffer.rs` - Bounds checked readers for raw USN record buffers, shared by MFT and USN parsing
- `watcher.rs` - File system watcher for non-NTFS drives
- `watcher/pending_events.rs` - Debouncing watcher events and pairing the two halves of a rename

## MFT reading on NTFS drives

- Requires administrator privileges.
- Reads the Master File Table directly for fast initial indexing, millions of files in seconds.
- Works with both full drives and specific directories.

## USN journal on NTFS drives

- Tracks file system changes, so the daemon asks what changed since a USN instead of rescanning.
- Handles creates, deletes, renames, and modifications, including renames made by other programs.
- Also used for incremental stale entry cleanup after scans.

## Scan modes

- **Incremental mode**, the default, scans and upserts entries, then cleans up stale entries.
  NTFS volumes use the USN journal to find deleted and renamed files.
  Non-NTFS volumes run the volume pruner after the scan.
- **Clean mode**, from `--force` or `force_clean_scan`,
  deletes all existing entries of a volume before inserting new ones.
  Use it for complete rebuilds.
  Clean mode is used automatically when the database is empty.

## File watcher on non-NTFS and network drives

- Falls back to the `notify` crate, which uses `ReadDirectoryChangesW` on Windows.
- Walks the directories for the initial scan.
- Receives change notifications in real time, debounced before they reach the daemon.
- Renames become a single rename change when both the old and the new name are known.
  An old name without a new one within the debounce period is a delete, and a new name without an old one is a create.
- A renamed indexed directory moves its whole subtree in the index, and a deleted one removes it.

## Path types

- **Drive roots** like "C:" and "D:" use fast MFT scanning of the whole drive.
- **Local directories** like "C:\Users\Documents" use MFT scanning filtered to the path.
- **Mapped network drives** like "Z:" try MFT scanning first and fall back to walking.
- **UNC paths** like "\\server\share" are walked, since they have no drive letter.

## IPC server

The server runs in a dedicated thread and talks to the main daemon loop through tokio channels.
The shared `IpcServerState` uses atomic types to share status information safely between threads.
