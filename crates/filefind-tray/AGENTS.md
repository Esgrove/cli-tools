# filefind-tray agent notes

The `filefind-tray` system tray application that controls the filefind daemon over IPC.
See `crates/filefind-common/AGENTS.md` for the IPC protocol,
and the root `AGENTS.md` for the general workspace rules.

## Modules

- `main.rs` - Tray app entry point
- `app.rs` - Main application logic and event loop
- `icons.rs` - Tray icon generation

## Behaviour

- Minimal UI for daemon control.
- The icon color shows the daemon status: green when running, gray when stopped, and orange while scanning.
- Menu items: Start, Stop, Rescan, About, and Quit.
- The tooltip shows the indexed file and directory counts.
