//! Main application logic for the system tray.
//!
//! This module handles the tray icon, menu creation, background IPC worker,
//! and platform-native event loops for the filefind tray application.

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use objc2::MainThreadMarker;
#[cfg(target_os = "macos")]
use objc2::rc::autoreleasepool;
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use tracing::{error, info, trace, warn};
use tray_icon::menu::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::ShellExecuteW;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, SW_HIDE, TranslateMessage, WM_QUIT,
};

use cli_tools_common::format_number;
use filefind_common::{DaemonStateInfo, DaemonStatus, IpcClient};

use crate::icons;

/// Menu item identifiers.
mod menu_ids {
    pub const STATUS: &str = "status";
    pub const START: &str = "start";
    pub const STOP: &str = "stop";
    pub const RESCAN: &str = "rescan";
    pub const QUIT: &str = "quit";
}

/// Polling interval for daemon status checks on the background worker thread.
const UPDATE_INTERVAL: Duration = Duration::from_secs(1);

/// Short delay after issuing Start or Stop before polling status again.
const WAIT_INTERVAL: Duration = Duration::from_millis(500);

/// Event loop pump slice so menu and status updates stay responsive without busy-spinning.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Commands sent from the UI event loop to the background IPC worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCommand {
    /// Launch the daemon process.
    Start,
    /// Send a Stop IPC request to the running daemon.
    Stop,
    /// Send a Rescan IPC request to the running daemon.
    Rescan,
    /// Shut down the background worker thread.
    Shutdown,
}

/// Holds references to menu items that need dynamic state updates.
struct MenuItems {
    status: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    rescan: MenuItem,
}

/// Enabled flags for the three interactive daemon control menu items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuEnabledStates {
    /// Whether "Start daemon" is enabled.
    start: bool,
    /// Whether "Stop daemon" is enabled.
    stop: bool,
    /// Whether "Rescan" is enabled.
    rescan: bool,
}

/// Channels connecting the UI event loop to the background IPC worker thread.
struct IpcWorkerHandle {
    /// Sends Start, Stop, Rescan, and Shutdown commands to the worker thread.
    command_sender: Sender<WorkerCommand>,
    /// Receives daemon status snapshots polled by the worker thread.
    status_receiver: Receiver<DaemonStatus>,
}

/// Run the tray application on the platform-native event loop.
///
/// This function does not return under normal operation until the user selects Quit.
/// Quitting closes only the tray application and leaves any running daemon intact.
pub fn run() -> Result<()> {
    info!("Starting tray application");

    #[cfg(target_os = "macos")]
    {
        run_macos()
    }

    #[cfg(windows)]
    {
        run_windows()
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        run_gtk()
    }
}

/// Run the macOS menu-bar application on the main-thread `AppKit` event loop.
///
/// Initializes `NSApplication` with an accessory activation policy (no Dock icon)
/// and calls `finishLaunching` before creating the status item so fullscreen spaces
/// and `AppKit` menus behave correctly.
#[cfg(target_os = "macos")]
fn run_macos() -> Result<()> {
    let main_thread = MainThreadMarker::new().context("filefind-tray must run on the main thread")?;
    let application = NSApplication::sharedApplication(main_thread);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    application.finishLaunching();

    let (tray_icon, menu_items) = create_tray_icon()?;
    let worker = spawn_ipc_worker();
    let menu_channel = MenuEvent::receiver();
    let mut previous_state: Option<DaemonStateInfo> = None;
    let mut should_quit = false;

    while !should_quit {
        autoreleasepool(|_| {
            drain_status_updates(&worker.status_receiver, &tray_icon, &menu_items, &mut previous_state);
            if drain_menu_events(menu_channel, &worker.command_sender) {
                should_quit = true;
                return;
            }

            let until = NSDate::dateWithTimeIntervalSinceNow(EVENT_POLL_INTERVAL.as_secs_f64());
            // SAFETY: `NSDefaultRunLoopMode` is an immutable AppKit/Foundation constant on the main thread.
            #[allow(unsafe_code)]
            let mode = unsafe { NSDefaultRunLoopMode };
            if let Some(event) =
                application.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), mode, true)
            {
                application.sendEvent(&event);
                application.updateWindows();
            }
        });
    }

    let _ = worker.command_sender.send(WorkerCommand::Shutdown);
    drop(tray_icon);
    Ok(())
}

/// Run the Windows system tray message pump.
#[cfg(windows)]
#[allow(unsafe_code)]
fn run_windows() -> Result<()> {
    let (tray_icon, menu_items) = create_tray_icon()?;
    let worker = spawn_ipc_worker();
    let menu_channel = MenuEvent::receiver();
    let mut previous_state: Option<DaemonStateInfo> = None;
    let mut should_quit = false;
    let mut msg: MSG = unsafe { std::mem::zeroed() };

    while !should_quit {
        drain_status_updates(&worker.status_receiver, &tray_icon, &menu_items, &mut previous_state);
        if drain_menu_events(menu_channel, &worker.command_sender) {
            should_quit = true;
            // SAFETY: PostQuitMessage is always safe to call on the UI thread.
            unsafe {
                PostQuitMessage(0);
            }
        }

        // SAFETY: PeekMessageW, TranslateMessage, and DispatchMessageW operate on a valid MSG struct.
        while unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if msg.message == WM_QUIT {
                should_quit = true;
                break;
            }

            unsafe {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }

        if !should_quit {
            std::thread::sleep(EVENT_POLL_INTERVAL);
        }
    }

    let _ = worker.command_sender.send(WorkerCommand::Shutdown);
    drop(tray_icon);
    Ok(())
}

/// Run the Linux/BSD AppIndicator tray application on the GTK event loop.
#[cfg(all(unix, not(target_os = "macos")))]
fn run_gtk() -> Result<()> {
    gtk::init().context("Failed to initialize GTK (is a graphical display session available?)")?;

    let (tray_icon, menu_items) = create_tray_icon()?;
    let worker = spawn_ipc_worker();
    let menu_channel = MenuEvent::receiver();
    let mut previous_state: Option<DaemonStateInfo> = None;
    let mut should_quit = false;

    while !should_quit {
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }

        drain_status_updates(&worker.status_receiver, &tray_icon, &menu_items, &mut previous_state);
        if drain_menu_events(menu_channel, &worker.command_sender) {
            should_quit = true;
            break;
        }

        std::thread::sleep(EVENT_POLL_INTERVAL);
    }

    let _ = worker.command_sender.send(WorkerCommand::Shutdown);
    drop(tray_icon);
    Ok(())
}

/// Spawn a background worker thread that handles all synchronous IPC polling and daemon commands.
fn spawn_ipc_worker() -> IpcWorkerHandle {
    let (command_sender, command_receiver) = mpsc::channel::<WorkerCommand>();
    let (status_sender, status_receiver) = mpsc::channel::<DaemonStatus>();

    std::thread::Builder::new()
        .name("tray-ipc-worker".to_string())
        .spawn(move || {
            let ipc_client = IpcClient::new();
            info!("Checking initial daemon status...");
            if status_sender.send(get_daemon_status(&ipc_client)).is_err() {
                return;
            }

            loop {
                match command_receiver.recv_timeout(UPDATE_INTERVAL) {
                    Ok(WorkerCommand::Start) => {
                        info!("Starting daemon...");
                        if let Err(error) = start_daemon() {
                            error!("Failed to start daemon: {error:#}");
                        }
                        std::thread::sleep(WAIT_INTERVAL);
                        if status_sender.send(get_daemon_status(&ipc_client)).is_err() {
                            break;
                        }
                    }
                    Ok(WorkerCommand::Stop) => {
                        info!("Stopping daemon...");
                        match ipc_client.stop_daemon() {
                            Ok(()) => info!("Stop command sent successfully"),
                            Err(error) => error!("Failed to stop daemon: {error:#}"),
                        }
                        std::thread::sleep(WAIT_INTERVAL);
                        if status_sender.send(get_daemon_status(&ipc_client)).is_err() {
                            break;
                        }
                    }
                    Ok(WorkerCommand::Rescan) => {
                        info!("Triggering rescan...");
                        match ipc_client.rescan() {
                            Ok(()) => info!("Rescan command sent successfully"),
                            Err(error) => error!("Failed to trigger rescan: {error:#}"),
                        }
                        if status_sender.send(get_daemon_status(&ipc_client)).is_err() {
                            break;
                        }
                    }
                    Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if status_sender.send(get_daemon_status(&ipc_client)).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .expect("Failed to spawn tray IPC worker thread");

    IpcWorkerHandle {
        command_sender,
        status_receiver,
    }
}

/// Drain pending daemon status snapshots from the worker and apply the latest one to the UI.
fn drain_status_updates(
    status_receiver: &Receiver<DaemonStatus>,
    tray_icon: &TrayIcon,
    menu_items: &MenuItems,
    previous_state: &mut Option<DaemonStateInfo>,
) {
    let mut latest = None;
    while let Ok(status) = status_receiver.try_recv() {
        latest = Some(status);
    }
    if let Some(status) = latest {
        *previous_state = apply_tray_status(tray_icon, menu_items, &status, *previous_state);
    }
}

/// Drain pending menu events, dispatching daemon commands to the worker and returning true on Quit.
fn drain_menu_events(
    menu_channel: &tray_icon::menu::MenuEventReceiver,
    command_sender: &Sender<WorkerCommand>,
) -> bool {
    let mut quit_requested = false;
    while let Ok(event) = menu_channel.try_recv() {
        match event.id.0.as_str() {
            menu_ids::START => {
                let _ = command_sender.send(WorkerCommand::Start);
            }
            menu_ids::STOP => {
                let _ = command_sender.send(WorkerCommand::Stop);
            }
            menu_ids::RESCAN => {
                let _ = command_sender.send(WorkerCommand::Rescan);
            }
            menu_ids::QUIT => {
                info!("Quitting tray application");
                quit_requested = true;
            }
            _ => {}
        }
    }
    quit_requested
}

/// Create the tray icon with menu.
fn create_tray_icon() -> Result<(TrayIcon, MenuItems)> {
    let icon = icons::create_stopped_icon()?;
    let (menu, menu_items) = create_menu()?;

    let tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Filefind - Stopped")
        .with_menu(Box::new(menu))
        .build()
        .context("Failed to build tray icon")?;

    Ok((tray_icon, menu_items))
}

/// Create the tray menu.
///
/// Returns the menu and the menu items that need dynamic state updates.
fn create_menu() -> Result<(Menu, MenuItems)> {
    let menu = Menu::new();

    // Status item (disabled, shows current state)
    let status_item = MenuItem::with_id(menu_ids::STATUS, "Status: Unknown", false, None);

    // Control items (initial state: daemon assumed stopped)
    let start_item = MenuItem::with_id(menu_ids::START, "Start daemon", true, None);
    let stop_item = MenuItem::with_id(menu_ids::STOP, "Stop daemon", false, None);
    let rescan_item = MenuItem::with_id(menu_ids::RESCAN, "Rescan", false, None);

    // Quit item
    let quit_item = MenuItem::with_id(menu_ids::QUIT, "Quit", true, None);

    // About item
    let about_item = PredefinedMenuItem::about(Some("About Filefind"), Some(about_metadata()));

    // Build the menu
    menu.append(&status_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&start_item)?;
    menu.append(&stop_item)?;
    menu.append(&rescan_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&about_item)?;
    menu.append(&quit_item)?;

    let menu_items = MenuItems {
        status: status_item,
        start: start_item,
        stop: stop_item,
        rescan: rescan_item,
    };

    Ok((menu, menu_items))
}

/// Compute enabled states for the interactive daemon control menu items.
const fn menu_enabled_states(state: DaemonStateInfo) -> MenuEnabledStates {
    match state {
        DaemonStateInfo::Stopped => MenuEnabledStates {
            start: true,
            stop: false,
            rescan: false,
        },
        DaemonStateInfo::Running => MenuEnabledStates {
            start: false,
            stop: true,
            rescan: true,
        },
        DaemonStateInfo::Scanning => MenuEnabledStates {
            start: false,
            stop: true,
            rescan: false,
        },
        DaemonStateInfo::Starting | DaemonStateInfo::Stopping => MenuEnabledStates {
            start: false,
            stop: false,
            rescan: false,
        },
    }
}

/// Apply a daemon status snapshot to the tray icon, tooltip, and menu items on the UI thread.
#[allow(clippy::unnecessary_wraps)]
fn apply_tray_status(
    tray_icon: &TrayIcon,
    menu_items: &MenuItems,
    status: &DaemonStatus,
    previous_state: Option<DaemonStateInfo>,
) -> Option<DaemonStateInfo> {
    let current_state = status.state;

    match previous_state {
        None => {
            info!(
                "Daemon status initialized: state={current_state:?}, files={}, directories={}",
                format_number(status.indexed_files),
                format_number(status.indexed_directories)
            );
        }
        Some(prev) if prev != current_state => {
            info!(
                "Daemon state changed: {prev:?} -> {current_state:?} (files: {}, directories: {})",
                format_number(status.indexed_files),
                format_number(status.indexed_directories)
            );
        }
        _ => {
            trace!(
                "Status update: {current_state:?}, files={}, directories={}",
                status.indexed_files, status.indexed_directories
            );
        }
    }

    let icon = match current_state {
        DaemonStateInfo::Running => icons::create_running_icon(),
        DaemonStateInfo::Scanning => icons::create_scanning_icon(),
        DaemonStateInfo::Starting | DaemonStateInfo::Stopping => icons::create_default_icon(),
        DaemonStateInfo::Stopped => icons::create_stopped_icon(),
    };

    if let Ok(icon) = icon {
        let _ = tray_icon.set_icon(Some(icon));
    }

    let tooltip = format_status_tooltip(status);
    let _ = tray_icon.set_tooltip(Some(tooltip));

    let status_text = format_status_menu_text(status);
    menu_items.status.set_text(status_text);

    let enabled = menu_enabled_states(current_state);
    menu_items.start.set_enabled(enabled.start);
    menu_items.stop.set_enabled(enabled.stop);
    menu_items.rescan.set_enabled(enabled.rescan);

    Some(current_state)
}

/// Format the status for the menu item.
fn format_status_menu_text(status: &DaemonStatus) -> String {
    match status.state {
        DaemonStateInfo::Running => format!("Status: Running ({} files)", format_number(status.indexed_files)),
        DaemonStateInfo::Scanning => format!("Status: Scanning ({} files)", format_number(status.indexed_files)),
        DaemonStateInfo::Starting => "Status: Starting...".to_string(),
        DaemonStateInfo::Stopping => "Status: Stopping...".to_string(),
        DaemonStateInfo::Stopped => "Status: Stopped".to_string(),
    }
}

/// Get the current daemon status.
fn get_daemon_status(ipc_client: &IpcClient) -> DaemonStatus {
    match ipc_client.get_status() {
        Ok(status) => {
            trace!("Daemon status: {:?}", status.state);
            status
        }
        Err(error) => {
            warn!("Failed to get daemon status: {}", error);
            DaemonStatus::default()
        }
    }
}

/// Format the status for the tooltip.
fn format_status_tooltip(status: &DaemonStatus) -> String {
    match status.state {
        DaemonStateInfo::Running => {
            format!(
                "Filefind - Running\nFiles: {}\nDirectories: {}",
                format_number(status.indexed_files),
                format_number(status.indexed_directories)
            )
        }
        DaemonStateInfo::Scanning => {
            format!("Filefind - Scanning...\nFiles: {}", format_number(status.indexed_files))
        }
        DaemonStateInfo::Starting => "Filefind - Starting...".to_string(),
        DaemonStateInfo::Stopping => "Filefind - Stopping...".to_string(),
        DaemonStateInfo::Stopped => "Filefind - Stopped".to_string(),
    }
}

/// Resolve the sibling `filefindd` binary next to `current_exe`, falling back to `PATH`.
fn resolve_daemon_executable(current_exe: Option<&Path>) -> PathBuf {
    let binary_name = if cfg!(windows) { "filefindd.exe" } else { "filefindd" };
    if let Some(exe) = current_exe {
        let sibling = exe.with_file_name(binary_name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(binary_name)
}

/// Start the daemon process with elevated privileges on Windows.
///
/// Uses `ShellExecuteW` with the `runas` verb to request UAC elevation
/// so the daemon has administrator rights for MFT and USN Journal access.
/// The daemon is started with `filefindd start` so it spawns detached without a console window.
#[cfg(windows)]
fn start_daemon() -> Result<()> {
    /// Encode an `OsStr` as a null-terminated UTF-16 vector for Windows API calls.
    fn to_wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;
    let exe_path = resolve_daemon_executable(Some(&current_exe));

    let verb = to_wide(OsStr::new("runas"));
    let file = to_wide(exe_path.as_os_str());
    let params = to_wide(OsStr::new("start"));

    // SAFETY: `ShellExecuteW` is a standard Windows API call.
    // All string pointers are valid null-terminated UTF-16 buffers that outlive the call.
    #[allow(unsafe_code)]
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        )
    };

    if (result as usize) <= 32 {
        anyhow::bail!("ShellExecuteW failed with code {result:?}, UAC prompt may have been declined");
    }

    Ok(())
}

/// Start the daemon process on Unix using the sibling `filefindd` binary when present.
#[cfg(not(windows))]
fn start_daemon() -> Result<()> {
    let current_exe = std::env::current_exe().ok();
    let daemon_path = resolve_daemon_executable(current_exe.as_deref());
    Command::new(&daemon_path)
        .arg("start")
        .spawn()
        .with_context(|| format!("Failed to start daemon process {}", daemon_path.display()))?;

    Ok(())
}

/// Application metadata for the About window.
///
/// Shows the multi-line build details with the version, git commit and branch, build time, and rustc version.
/// Windows and GTK show them under the description in `comments`,
/// and macOS shows them as `credits` since its About panel does not support `comments`.
fn about_metadata() -> AboutMetadata {
    let build_details = cli_tools_version::build_details();
    AboutMetadata {
        name: Some("Filefind".to_string()),
        version: Some(cli_tools_version::PACKAGE_VERSION.to_string()),
        authors: Some(vec!["Esgrove".to_string()]),
        comments: Some(format!("Fast file indexing and search\n\n{build_details}")),
        copyright: Some("MIT License".to_string()),
        license: Some("MIT".to_string()),
        website: Some("https://github.com/Esgrove/cli-tools".to_string()),
        website_label: Some("GitHub".to_string()),
        credits: Some(build_details),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
        assert_eq!(format_number(1_000_000_000), "1,000,000,000");
    }

    #[test]
    fn test_format_status_tooltip_stopped() {
        let status = DaemonStatus::default();
        let tooltip = format_status_tooltip(&status);
        assert!(tooltip.contains("Stopped"));
    }

    #[test]
    fn test_format_status_tooltip_running() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Running,
            indexed_files: 12345,
            indexed_directories: 678,
            ..Default::default()
        };
        let tooltip = format_status_tooltip(&status);
        assert!(tooltip.contains("Running"));
        assert!(tooltip.contains("12,345"));
        assert!(tooltip.contains("678"));
    }

    #[test]
    fn test_format_status_tooltip_scanning() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Scanning,
            indexed_files: 5000,
            ..Default::default()
        };
        let tooltip = format_status_tooltip(&status);
        assert!(tooltip.contains("Scanning"));
        assert!(tooltip.contains("5,000"));
    }

    #[test]
    fn test_format_status_menu_text_stopped() {
        let status = DaemonStatus::default();
        let text = format_status_menu_text(&status);
        assert_eq!(text, "Status: Stopped");
    }

    #[test]
    fn test_format_status_menu_text_running() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Running,
            indexed_files: 42_000,
            ..Default::default()
        };
        let text = format_status_menu_text(&status);
        assert!(text.starts_with("Status: Running"));
        assert!(text.contains("42,000"), "Should contain formatted file count");
    }

    #[test]
    fn test_format_status_menu_text_scanning() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Scanning,
            indexed_files: 1_234_567,
            ..Default::default()
        };
        let text = format_status_menu_text(&status);
        assert!(text.starts_with("Status: Scanning"));
        assert!(text.contains("1,234,567"), "Should contain formatted file count");
    }

    #[test]
    fn test_format_status_menu_text_starting() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Starting,
            ..Default::default()
        };
        let text = format_status_menu_text(&status);
        assert_eq!(text, "Status: Starting...");
    }

    #[test]
    fn test_format_status_menu_text_stopping() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Stopping,
            ..Default::default()
        };
        let text = format_status_menu_text(&status);
        assert_eq!(text, "Status: Stopping...");
    }

    #[test]
    fn test_format_status_menu_text_running_zero_files() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Running,
            indexed_files: 0,
            ..Default::default()
        };
        let text = format_status_menu_text(&status);
        assert!(text.starts_with("Status: Running"));
        assert!(text.contains("0 files"));
    }

    #[test]
    fn test_format_status_tooltip_starting() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Starting,
            ..Default::default()
        };
        let tooltip = format_status_tooltip(&status);
        assert_eq!(tooltip, "Filefind - Starting...");
    }

    #[test]
    fn test_format_status_tooltip_stopping() {
        let status = DaemonStatus {
            state: DaemonStateInfo::Stopping,
            ..Default::default()
        };
        let tooltip = format_status_tooltip(&status);
        assert_eq!(tooltip, "Filefind - Stopping...");
    }
}

#[cfg(test)]
mod test_tray_state_and_daemon_lookup {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn menu_enabled_states_match_every_daemon_lifecycle_state() {
        assert_eq!(
            menu_enabled_states(DaemonStateInfo::Stopped),
            MenuEnabledStates {
                start: true,
                stop: false,
                rescan: false,
            }
        );
        assert_eq!(
            menu_enabled_states(DaemonStateInfo::Starting),
            MenuEnabledStates {
                start: false,
                stop: false,
                rescan: false,
            }
        );
        assert_eq!(
            menu_enabled_states(DaemonStateInfo::Running),
            MenuEnabledStates {
                start: false,
                stop: true,
                rescan: true,
            }
        );
        assert_eq!(
            menu_enabled_states(DaemonStateInfo::Scanning),
            MenuEnabledStates {
                start: false,
                stop: true,
                rescan: false,
            }
        );
        assert_eq!(
            menu_enabled_states(DaemonStateInfo::Stopping),
            MenuEnabledStates {
                start: false,
                stop: false,
                rescan: false,
            }
        );
    }

    #[test]
    fn resolve_daemon_executable_prefers_sibling_binary_and_falls_back_to_path() {
        let temp = tempdir().unwrap();
        let tray_exe = temp.path().join(if cfg!(windows) {
            "filefind-tray.exe"
        } else {
            "filefind-tray"
        });
        let daemon_exe = temp
            .path()
            .join(if cfg!(windows) { "filefindd.exe" } else { "filefindd" });

        // Without a sibling binary, falls back to bare name on PATH.
        assert_eq!(
            resolve_daemon_executable(Some(&tray_exe)),
            PathBuf::from(if cfg!(windows) { "filefindd.exe" } else { "filefindd" })
        );

        // When sibling filefindd exists, resolves directly to its absolute path.
        fs::write(&daemon_exe, "").unwrap();
        assert_eq!(resolve_daemon_executable(Some(&tray_exe)), daemon_exe);
        assert_eq!(
            resolve_daemon_executable(None),
            PathBuf::from(if cfg!(windows) { "filefindd.exe" } else { "filefindd" })
        );
    }
}

#[cfg(test)]
mod test_about_metadata {
    use super::*;

    #[test]
    fn shows_the_package_version() {
        let metadata = about_metadata();
        assert_eq!(metadata.version.as_deref(), Some(cli_tools_version::PACKAGE_VERSION));
        assert_eq!(metadata.short_version, None);
    }

    #[test]
    fn comments_and_credits_show_the_build_details() {
        let metadata = about_metadata();
        let comments = metadata.comments.unwrap();
        let credits = metadata.credits.unwrap();
        assert!(comments.starts_with("Fast file indexing and search\n\n"));
        for value in [
            cli_tools_version::GIT_COMMIT,
            cli_tools_version::GIT_BRANCH,
            cli_tools_version::RUST_VERSION,
        ] {
            assert!(comments.contains(value), "{value} missing from comments:\n{comments}");
            assert!(credits.contains(value), "{value} missing from credits:\n{credits}");
        }
        assert!(credits.contains("Built:"));
        assert!(credits.contains(" UTC"));
    }
}
