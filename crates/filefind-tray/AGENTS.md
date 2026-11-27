# filefind-tray agent notes

The `filefind-tray` system tray application that controls the filefind daemon over IPC.
See `crates/filefind-common/AGENTS.md` for the IPC protocol,
and the root `AGENTS.md` for the general workspace rules.

## Modules

- `main.rs` - Tray app entry point
- `app.rs` - Main application logic and event loop
- `icons.rs` - Tray icon generation

## Behaviour

- Minimal UI for daemon control on Windows, macOS, and Linux.
- The icon color shows the daemon status: green when running, gray when stopped, and orange while scanning.
- Menu items: Start, Stop, Rescan, About, and Quit (Quit exits only the tray and leaves a running daemon intact).
- The tooltip shows the indexed file and directory counts.
- Platform event loops:
  main-thread `AppKit` accessory app on macOS (event loop initialized before status item creation),
  Win32 message pump on Windows, and GTK event loop on Linux.
- Synchronous IPC polling and daemon actions run on a background worker thread so the UI thread never blocks.
