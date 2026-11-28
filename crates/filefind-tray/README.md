# filefind-tray

System tray and macOS menu bar application for controlling the filefind daemon.

This application provides a minimal graphical interface for managing the
`filefindd` background indexing service on Windows, macOS, and Linux.

## Features

- **Status indicator**: Tray icon color shows daemon state (green for running, orange for scanning, gray for stopped)
- **Tooltip**: Shows indexed file and directory counts
- **Quick controls**: Start, stop, and rescan from the tray menu

## Usage

```
System tray application that controls the filefind daemon

Usage: filefind-tray [OPTIONS] [COMMAND]

Commands:
  completion  Generate shell completion scripts
  help        Print this message or the help of the given subcommand(s)

Options:
  -f, --foreground   Run in foreground with console logging
  -l, --log <LEVEL>  Set the log level [possible values: error, warn, info, debug, trace]
  -v, --verbose      Enable verbose logging (shortcut for --log debug)
  -h, --help         Print help (see more with '--help')
  -V, --version      Print version
```

Simply run the application:

```shell
filefind-tray
```

When launched from a terminal, `filefind-tray` detaches once and returns the shell prompt.
Click the menu bar icon on macOS (or right-click the system tray icon on Windows and Linux) to open the menu.
Use `--foreground` to keep the process attached with console logging.

### Subcommands

#### completion

Generate shell completion scripts:

```
Generate shell completion scripts

Usage: filefind-tray completion [OPTIONS] <SHELL>

Arguments:
  <SHELL>  Shell to generate completion for [possible values: bash, elvish, fish, powershell, zsh]

Options:
  -I, --install  Install the completion script to the appropriate location
  -v, --verbose  Enable verbose logging (shortcut for --log debug)
  -h, --help     Print help
```

### Menu options

| Option | Description |
| --- | --- |
| **Status** | Shows current daemon state and file count |
| **Start daemon** | Starts the sibling `filefindd` process (`filefindd start`) |
| **Stop daemon** | Sends a Stop IPC command to the running daemon |
| **Rescan** | Sends a Rescan IPC command for all indexed roots |
| **About Filefind** | Shows the version, git commit and branch, build time, and rustc version |
| **Quit** | Exits only the tray application and leaves any running daemon intact |

## Platform notes

- **macOS**: Runs on the main-thread `AppKit` event loop with an accessory activation policy (no Dock icon),
  initializes the event loop before creating the status item,
  and locates the sibling `filefindd` executable without depending on `PATH`.
- **Linux**: Uses the AppIndicator backend driven by the GTK event loop.
  Building requires `libgtk-3-dev`, `libayatana-appindicator3-dev`, and `libxdo-dev`,
  and running requires an active graphical display session.
- **Windows**: Runs on the Win32 message loop and uses `ShellExecuteW` with `runas` when starting `filefindd`
  so the daemon can elevate for NTFS MFT and USN Journal access.

## Auto-start on login (Windows)

To have the tray application start automatically when you log in on Windows:

```powershell
$action = New-ScheduledTaskAction -Execute "$env:USERPROFILE\.cargo\bin\filefind-tray.exe"
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName "filefind tray" -Action $action -Trigger $trigger -Settings $settings
```

## Architecture

1. Runs a platform-native event loop (`AppKit` on macOS, Win32 on Windows, GTK on Linux)
2. Polls daemon status and runs Start, Stop, and Rescan on a background worker thread so the UI never stalls on IPC
3. Communicates with `filefindd` over `\\.\pipe\filefind-daemon` on Windows
   or the per-user Unix domain socket on macOS and Linux (overridable via `FILEFIND_IPC_PATH`)
4. Launches `filefindd start` directly to start the daemon, then uses IPC for status, stop, and rescan
