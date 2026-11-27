//! Interactive terminal interface for reviewing and resolving duplicate groups.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use itertools::Itertools;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use cli_tools_common::{OverwritePolicy, PathChange, RenameOutcome, print_yellow};

use cli_tools_media::video_info::VideoInfo;

use cli_tools_dupefind::{DupeFileInfo, DuplicateGroup};

/// Action to perform on a duplicate group
#[derive(Debug, Clone)]
enum DuplicateAction {
    /// Keep the file at this index, delete others
    Keep {
        keep_index: usize,
        new_name: Option<String>,
    },
    /// Rename the selected file without deleting any others
    RenameOnly { rename_index: usize, new_name: String },
    /// Skip this group
    Skip,
    /// Quit the interactive session
    Quit,
}

/// An action to apply to a specific duplicate group.
#[derive(Debug)]
struct GroupAction {
    /// Index of the duplicate group this action applies to.
    group_index: usize,
    /// The action to perform on the group.
    action: DuplicateAction,
}

/// State for the interactive TUI
struct TuiState {
    /// Current selection index in the file list
    selected: usize,
    /// Whether we're in rename mode
    editing: bool,
    /// Whether editing is rename-only (keep all files)
    rename_only: bool,
    /// The new filename being edited
    edit_buffer: String,
    /// Cursor position in edit buffer
    cursor_pos: usize,
    /// Whether to show confirmation dialog
    confirming: bool,
}

/// RAII guard that leaves the alternate screen and disables raw mode on drop.
struct TerminalGuard {
    active: bool,
}

impl TuiState {
    const fn new() -> Self {
        Self {
            selected: 0,
            editing: false,
            rename_only: false,
            edit_buffer: String::new(),
            cursor_pos: 0,
            confirming: false,
        }
    }

    const fn select_next(&mut self, max: usize) {
        if self.selected < max.saturating_sub(1) {
            self.selected += 1;
        }
    }

    const fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn start_editing(&mut self, initial: &str) {
        self.editing = true;
        self.edit_buffer = initial.to_string();
        self.cursor_pos = 0;
    }

    fn stop_editing(&mut self) {
        self.editing = false;
        self.rename_only = false;
        self.edit_buffer.clear();
        self.cursor_pos = 0;
    }

    fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.edit_buffer.floor_char_boundary(self.cursor_pos - 1);
        }
    }

    fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.edit_buffer.len() {
            self.cursor_pos = self.edit_buffer.ceil_char_boundary(self.cursor_pos + 1);
        }
    }

    fn insert_char(&mut self, character: char) {
        self.edit_buffer.insert(self.cursor_pos, character);
        self.cursor_pos += character.len_utf8();
    }

    fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            let previous_position = self.edit_buffer.floor_char_boundary(self.cursor_pos - 1);
            self.edit_buffer.replace_range(previous_position..self.cursor_pos, "");
            self.cursor_pos = previous_position;
        }
    }

    fn delete_char_forward(&mut self) {
        if let Some(character) = self
            .edit_buffer
            .get(self.cursor_pos..)
            .and_then(|remaining| remaining.chars().next())
        {
            self.edit_buffer
                .replace_range(self.cursor_pos..self.cursor_pos + character.len_utf8(), "");
        }
    }
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, returning a guard that restores them on drop.
    fn enter() -> anyhow::Result<Self> {
        let mut stdout = std::io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        if let Err(error) = enable_raw_mode() {
            let _ = stdout.execute(LeaveAlternateScreen);
            return Err(error.into());
        }
        Ok(Self { active: true })
    }

    /// Restore the terminal explicitly and surface any I/O error.
    fn restore(&mut self) -> anyhow::Result<()> {
        if self.active {
            self.active = false;
            disable_raw_mode()?;
            std::io::stdout().execute(LeaveAlternateScreen)?;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let _ = disable_raw_mode();
            let _ = std::io::stdout().execute(LeaveAlternateScreen);
        }
    }
}

/// Run interactive TUI mode for handling duplicates
pub fn run_interactive(duplicates: &[DuplicateGroup], metadata: &HashMap<PathBuf, VideoInfo>) -> anyhow::Result<()> {
    let mut guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let actions = interactive_loop(&mut terminal, duplicates, metadata)?;

    // Restore terminal before applying actions so warnings display correctly.
    guard.restore()?;

    if !actions.is_empty() {
        apply_actions(duplicates, &actions)?;
    }

    Ok(())
}

/// Main interactive loop - returns collected actions to be applied after terminal is restored
fn interactive_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    duplicates: &[DuplicateGroup],
    metadata: &HashMap<PathBuf, VideoInfo>,
) -> anyhow::Result<Vec<GroupAction>> {
    let mut group_index = 0;
    let mut actions: Vec<GroupAction> = Vec::new();

    while group_index < duplicates.len() {
        let group = duplicates
            .get(group_index)
            .with_context(|| format!("Duplicate group index {group_index} is out of bounds"))?;
        let sorted_files: Vec<&DupeFileInfo> = group.files.iter().sorted_by_key(|f| &f.path).collect();

        let action = handle_duplicate_group(
            terminal,
            &group.display_name(),
            &sorted_files,
            group_index,
            duplicates.len(),
            metadata,
        )?;

        match action {
            DuplicateAction::Quit => break,
            DuplicateAction::Skip => {
                group_index += 1;
            }
            DuplicateAction::Keep { keep_index, new_name } => {
                actions.push(GroupAction {
                    group_index,
                    action: DuplicateAction::Keep { keep_index, new_name },
                });
                group_index += 1;
            }
            DuplicateAction::RenameOnly { rename_index, new_name } => {
                actions.push(GroupAction {
                    group_index,
                    action: DuplicateAction::RenameOnly { rename_index, new_name },
                });
                group_index += 1;
            }
        }
    }

    Ok(actions)
}

/// Handle a single duplicate group interactively
fn handle_duplicate_group(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    key: &str,
    files: &[&DupeFileInfo],
    current_group: usize,
    total_groups: usize,
    metadata: &HashMap<PathBuf, VideoInfo>,
) -> anyhow::Result<DuplicateAction> {
    if files.is_empty() {
        return Ok(DuplicateAction::Skip);
    }
    let best_index = find_best_file_index_with_metadata(files, metadata);
    let mut state = TuiState::new();
    state.selected = best_index;
    let mut list_state = ListState::default();
    list_state.select(Some(best_index));

    loop {
        terminal.draw(|frame| {
            render_ui(
                frame,
                key,
                files,
                &state,
                &mut list_state,
                current_group,
                total_groups,
                metadata,
            );
        })?;

        if let Event::Key(key_event) = event::read()? {
            if key_event.kind != KeyEventKind::Press {
                continue;
            }
            if key_event.modifiers.contains(KeyModifiers::CONTROL) && matches!(key_event.code, KeyCode::Char('c' | 'C'))
            {
                return Ok(DuplicateAction::Quit);
            }

            if state.editing {
                if let Some(action) = handle_editing_key(key_event.code, &mut state, files) {
                    return Ok(action);
                }
            } else if state.confirming {
                match key_event.code {
                    KeyCode::Char('y' | 'Y') => {
                        return Ok(DuplicateAction::Keep {
                            keep_index: state.selected,
                            new_name: None,
                        });
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        state.confirming = false;
                    }
                    _ => {}
                }
            } else {
                match key_event.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(DuplicateAction::Quit),
                    KeyCode::Up | KeyCode::Char('k') => {
                        state.select_prev();
                        list_state.select(Some(state.selected));
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        state.select_next(files.len());
                        list_state.select(Some(state.selected));
                    }
                    KeyCode::Char('s') => return Ok(DuplicateAction::Skip),
                    KeyCode::Enter => {
                        state.confirming = true;
                    }
                    KeyCode::Char('r') => {
                        let selected_file = files.get(state.selected).context("Selected file is unavailable")?;
                        state.start_editing(&selected_file.stem);
                    }
                    KeyCode::Char('n') => {
                        let selected_file = files.get(state.selected).context("Selected file is unavailable")?;
                        state.start_editing(&selected_file.stem);
                        state.rename_only = true;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Handle a key press while the inline rename editor is active.
fn handle_editing_key(code: KeyCode, state: &mut TuiState, files: &[&DupeFileInfo]) -> Option<DuplicateAction> {
    match code {
        KeyCode::Esc => state.stop_editing(),
        KeyCode::Enter => {
            if state.rename_only {
                if state.edit_buffer.trim().is_empty() {
                    state.stop_editing();
                } else if files
                    .get(state.selected)
                    .is_some_and(|file| build_renamed_path(&file.path, &state.edit_buffer, &file.extension).is_ok())
                {
                    return Some(DuplicateAction::RenameOnly {
                        rename_index: state.selected,
                        new_name: state.edit_buffer.clone(),
                    });
                }
            } else if state.edit_buffer.trim().is_empty() {
                return Some(DuplicateAction::Keep {
                    keep_index: state.selected,
                    new_name: None,
                });
            } else if files
                .get(state.selected)
                .is_some_and(|file| build_renamed_path(&file.path, &state.edit_buffer, &file.extension).is_ok())
            {
                return Some(DuplicateAction::Keep {
                    keep_index: state.selected,
                    new_name: Some(state.edit_buffer.clone()),
                });
            }
        }
        KeyCode::Backspace => state.delete_char(),
        KeyCode::Delete => state.delete_char_forward(),
        KeyCode::Left => state.move_cursor_left(),
        KeyCode::Right => state.move_cursor_right(),
        KeyCode::Home => state.cursor_pos = 0,
        KeyCode::End => state.cursor_pos = state.edit_buffer.len(),
        KeyCode::Char(c) => state.insert_char(c),
        _ => {}
    }
    None
}

/// Format a metadata detail line for a single file.
fn format_file_detail_lines(
    file: &DupeFileInfo,
    index: usize,
    selected: usize,
    metadata: &HashMap<PathBuf, VideoInfo>,
) -> Vec<Line<'static>> {
    let is_selected = index == selected;
    let prefix = if is_selected { "► " } else { "  " };

    let base_style = if is_selected {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let label_style = if is_selected {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut lines = Vec::new();

    // File path line
    let path_str = file.path.display().to_string();
    lines.push(Line::from(Span::styled(format!("{prefix}{path_str}"), base_style)));

    // Metadata details
    if let Some(meta) = metadata.get(&file.path) {
        let size_str = cli_tools_common::format_size(meta.size_bytes.unwrap_or(0));

        let duration_str = meta
            .duration
            .map_or_else(|| "N/A".to_string(), cli_tools_common::format_duration_seconds);

        let resolution_str = meta.resolution_string().unwrap_or_else(|| "N/A".to_string());

        let codec_str = meta.codec.as_deref().unwrap_or("N/A");

        let bitrate_str = meta
            .bitrate_kbps
            .map_or_else(|| "N/A".to_string(), |kbps| format!("{:.1} Mbps", kbps as f64 / 1000.0));

        let detail_line = Line::from(vec![
            Span::styled("     ".to_string(), base_style),
            Span::styled("Size: ", label_style),
            Span::styled(format!("{size_str:<12}"), base_style),
            Span::styled("Duration: ", label_style),
            Span::styled(format!("{duration_str:<14}"), base_style),
            Span::styled("Resolution: ", label_style),
            Span::styled(format!("{resolution_str:<12}"), base_style),
            Span::styled("Codec: ", label_style),
            Span::styled(format!("{codec_str:<10}"), base_style),
            Span::styled("Bitrate: ", label_style),
            Span::styled(bitrate_str, base_style),
        ]);
        lines.push(detail_line);
    } else {
        lines.push(Line::from(Span::styled(
            "     (metadata unavailable)".to_string(),
            label_style,
        )));
    }

    // Empty separator line between files
    lines.push(Line::from(""));

    lines
}

/// Render the TUI
#[allow(clippy::too_many_arguments)]
fn render_ui(
    frame: &mut Frame,
    key: &str,
    files: &[&DupeFileInfo],
    state: &TuiState,
    list_state: &mut ListState,
    current_group: usize,
    total_groups: usize,
    metadata: &HashMap<PathBuf, VideoInfo>,
) {
    let area = frame.area();

    // Calculate how many lines the file list needs (just path lines)
    // 2 for borders + 1 per file
    let file_list_height = (files.len() as u16).saturating_add(2).min(area.height / 3);

    // Calculate how many lines the details section needs 2 for borders + 3 lines per file (path, metadata, separator)
    let details_height = (files.len() as u16)
        .saturating_mul(3)
        .saturating_add(2)
        .min(area.height / 2);

    // Create layout
    let [
        header_area,
        file_list_area,
        details_area,
        status_area,
        help_area,
        _spacer_area,
    ] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            // Header
            Constraint::Length(3),
            // File list (compact)
            Constraint::Length(file_list_height),
            // File details
            Constraint::Length(details_height),
            // Status/Edit area
            Constraint::Length(3),
            // Help
            Constraint::Length(3),
            // Spacer (unused space below)
            Constraint::Min(0),
        ])
        .areas(area);

    let Some(selected_file) = files.get(state.selected) else {
        return;
    };

    // Header
    let header_text = format!("Duplicate Group {}/{}: {}", current_group + 1, total_groups, key);
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).title("Duplicate Finder"));
    frame.render_widget(header, header_area);

    // File list
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let prefix = if i == state.selected { "► " } else { "  " };
            let style = if i == state.selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(format!("{prefix}{}", file.filename)).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Files (↑/↓ to select)"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, file_list_area, list_state);

    // File details panel
    let mut detail_lines: Vec<Line> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let lines = format_file_detail_lines(file, index, state.selected, metadata);
        detail_lines.extend(lines);
    }

    let details = Paragraph::new(detail_lines).block(Block::default().borders(Borders::ALL).title("File Details"));
    frame.render_widget(details, details_area);

    // Status/Edit area
    let status_content = if state.editing {
        let (before_cursor, after_cursor) = state
            .edit_buffer
            .split_at_checked(state.cursor_pos)
            .unwrap_or((&state.edit_buffer, ""));
        format!("New name: {before_cursor}│{after_cursor}.{}", selected_file.extension)
    } else if state.confirming {
        format!(
            "Keep '{}' and delete {}? [y/N]",
            selected_file.filename,
            cli_tools_common::count_label(files.len() - 1, "other file", "other files")
        )
    } else {
        format!("Selected: {}", selected_file.filename)
    };

    let status_style = if state.editing {
        Style::default().fg(Color::Green)
    } else if state.confirming {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    let edit_title = if state.rename_only {
        "Rename Only (Enter to confirm, Esc to cancel)"
    } else {
        "Rename & Keep (Enter to confirm, Esc to cancel)"
    };

    let status = Paragraph::new(status_content).style(status_style).block(
        Block::default()
            .borders(Borders::ALL)
            .title(if state.editing { edit_title } else { "Status" }),
    );
    frame.render_widget(status, status_area);

    // Help
    let help_text = if state.editing {
        "Type new name | Enter: confirm | Esc: cancel"
    } else if state.confirming {
        "y: confirm | n: cancel"
    } else {
        "Enter: keep selected | r: rename & keep | n: rename only | s: skip | q: quit"
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title("Help"));

    frame.render_widget(help, help_area);
}

/// Apply all collected actions, moving deleted files to the trash when possible.
fn apply_actions(duplicates: &[DuplicateGroup], actions: &[GroupAction]) -> anyhow::Result<()> {
    let mut changes = Vec::new();
    let result = apply_actions_with(duplicates, actions, &cli_tools_common::trash_or_delete, &mut changes);
    for warning in filefind_common::update_default_index(&changes) {
        print_yellow!("Could not update the filefind index: {warning}");
    }
    result
}

/// Apply all collected actions, removing files with the given delete function.
///
/// Failures on one group are recorded and reported without aborting remaining groups.
/// Completed renames and deletions are appended to `changes`.
fn apply_actions_with(
    duplicates: &[DuplicateGroup],
    actions: &[GroupAction],
    delete_file: &dyn Fn(&Path) -> std::io::Result<()>,
    changes: &mut Vec<PathChange>,
) -> anyhow::Result<()> {
    let actionable: Vec<&GroupAction> = actions
        .iter()
        .filter(|a| !matches!(a.action, DuplicateAction::Skip | DuplicateAction::Quit))
        .collect();
    let total = actionable.len();

    let lock_store = if cfg!(test) {
        None
    } else {
        cli_tools_db::locks::ActiveLockStore::open_default("dupefind").ok()
    };
    let mut scan_cache = if cfg!(test) {
        None
    } else {
        cli_tools_media::scan_cache::ScanCache::open().ok()
    };

    let mut errors: Vec<String> = Vec::new();
    for (number, group_action) in actionable.into_iter().enumerate() {
        let Some(group) = duplicates.get(group_action.group_index) else {
            errors.push(format!(
                "Duplicate group index {} is out of bounds",
                group_action.group_index
            ));
            continue;
        };
        let sorted_files: Vec<&DupeFileInfo> = group.files.iter().sorted_by_key(|f| &f.path).collect();

        println!(
            "{}",
            colored::Colorize::bold(colored::Colorize::white(
                format!("── Group {}/{total} ──", number + 1).as_str()
            ))
        );

        let result = apply_single_group_action(
            &sorted_files,
            &group_action.action,
            delete_file,
            lock_store.as_ref(),
            scan_cache.as_mut(),
            changes,
        );
        if let Err(error) = result {
            cli_tools_common::print_error!("{error}");
            errors.push(error.to_string());
        }
    }

    if !errors.is_empty() {
        anyhow::bail!("{}", errors.join("; "));
    }
    Ok(())
}

/// Apply one group's decision, respecting active cross-tool locks and updating `ScanCache`.
fn apply_single_group_action(
    sorted_files: &[&DupeFileInfo],
    action: &DuplicateAction,
    delete_file: &dyn Fn(&Path) -> std::io::Result<()>,
    lock_store: Option<&cli_tools_db::locks::ActiveLockStore>,
    mut scan_cache: Option<&mut cli_tools_media::scan_cache::ScanCache>,
    changes: &mut Vec<PathChange>,
) -> anyhow::Result<()> {
    match action {
        DuplicateAction::Keep { keep_index, new_name } => {
            let keep_file = sorted_files
                .get(*keep_index)
                .copied()
                .context("Selected file to keep is unavailable")?;
            let keep_path = &keep_file.path;
            let keep_metadata = keep_path
                .symlink_metadata()
                .with_context(|| format!("Cannot verify selected file to keep: {}", keep_path.display()))?;
            anyhow::ensure!(
                keep_metadata.is_file(),
                "Selected file to keep is not a regular file: {}",
                keep_path.display()
            );

            let mut others: Vec<&DupeFileInfo> = sorted_files
                .iter()
                .enumerate()
                .filter(|(index, file)| *index != *keep_index && !same_location(&file.path, &keep_file.path))
                .map(|(_, file)| *file)
                .collect();

            if let Some(new_stem) = new_name {
                let new_path = build_renamed_path(&keep_file.path, new_stem, &keep_file.extension)?;
                if new_path != keep_file.path {
                    if let Some(store) = lock_store
                        && let Ok(Some(holder)) =
                            store.is_any_locked_by_other(&[keep_file.path.clone(), new_path.clone()])
                    {
                        print_yellow!(
                            "Skipping locked file (held by {} pid {}): {}",
                            holder.tool,
                            holder.pid,
                            keep_file.path.display()
                        );
                        return Ok(());
                    }
                    if let Some(position) = others.iter().position(|file| same_location(&file.path, &new_path)) {
                        let occupant = others.remove(position);
                        if !delete_duplicate(
                            &occupant.path,
                            delete_file,
                            lock_store,
                            scan_cache.as_deref_mut(),
                            changes,
                        ) {
                            print_yellow!("Keeping original name since {} still exists", new_path.display());
                            delete_all(&others, delete_file, lock_store, scan_cache, changes);
                            return Ok(());
                        }
                    }
                    if rename_file(&keep_file.path, &new_path)? {
                        changes.push(PathChange::moved_file(&keep_file.path, &new_path));
                        if let Some(cache) = scan_cache.as_deref_mut() {
                            let _ = cache.move_cached_path(&keep_file.path, &new_path);
                        }
                    }
                }
            }

            delete_all(&others, delete_file, lock_store, scan_cache, changes);
        }
        DuplicateAction::RenameOnly { rename_index, new_name } => {
            let file_to_rename = sorted_files
                .get(*rename_index)
                .copied()
                .context("Selected file to rename is unavailable")?;
            let new_path = build_renamed_path(&file_to_rename.path, new_name, &file_to_rename.extension)?;

            if new_path != file_to_rename.path {
                if let Some(store) = lock_store
                    && let Ok(Some(holder)) =
                        store.is_any_locked_by_other(&[file_to_rename.path.clone(), new_path.clone()])
                {
                    print_yellow!(
                        "Skipping locked file (held by {} pid {}): {}",
                        holder.tool,
                        holder.pid,
                        file_to_rename.path.display()
                    );
                    return Ok(());
                }
                if rename_file(&file_to_rename.path, &new_path)? {
                    changes.push(PathChange::moved_file(&file_to_rename.path, &new_path));
                    if let Some(cache) = scan_cache {
                        let _ = cache.move_cached_path(&file_to_rename.path, &new_path);
                    }
                }
            }
        }
        DuplicateAction::Skip | DuplicateAction::Quit => unreachable!(),
    }
    Ok(())
}

/// Build a validated sibling destination path for a TUI rename, rejecting path traversal or separators.
fn build_renamed_path(original: &Path, new_stem: &str, extension: &str) -> anyhow::Result<PathBuf> {
    let trimmed = new_stem.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." || trimmed.contains(['/', '\\', '\0']) {
        anyhow::bail!("Invalid rename target '{new_stem}': must be a single filename without path separators");
    }
    let file_name = if extension.is_empty() {
        trimmed.to_string()
    } else {
        format!("{trimmed}.{extension}")
    };
    Ok(original.with_file_name(file_name))
}

/// Rename a file without replacing a different existing file, printing the change.
fn rename_file(source: &Path, target: &Path) -> anyhow::Result<bool> {
    println!("{}", colored::Colorize::cyan("Rename:"));
    cli_tools_common::show_diff(
        &cli_tools_common::path_to_string_relative(source),
        &cli_tools_common::path_to_string_relative(target),
    );
    let outcome = cli_tools_common::safe_rename(source, target, OverwritePolicy::Skip)
        .with_context(|| format!("Failed to rename {}", source.display()))?;
    if outcome == RenameOutcome::SkippedExisting {
        print_yellow!("Skipping rename, file already exists: {}", target.display());
        return Ok(false);
    }
    Ok(true)
}

/// Delete every given file, reporting failures without stopping.
fn delete_all(
    files: &[&DupeFileInfo],
    delete_file: &dyn Fn(&Path) -> std::io::Result<()>,
    lock_store: Option<&cli_tools_db::locks::ActiveLockStore>,
    mut scan_cache: Option<&mut cli_tools_media::scan_cache::ScanCache>,
    changes: &mut Vec<PathChange>,
) {
    for file in files {
        delete_duplicate(&file.path, delete_file, lock_store, scan_cache.as_deref_mut(), changes);
    }
}

/// Delete one duplicate file and return whether it was removed.
fn delete_duplicate(
    path: &Path,
    delete_file: &dyn Fn(&Path) -> std::io::Result<()>,
    lock_store: Option<&cli_tools_db::locks::ActiveLockStore>,
    scan_cache: Option<&mut cli_tools_media::scan_cache::ScanCache>,
    changes: &mut Vec<PathChange>,
) -> bool {
    if let Some(store) = lock_store
        && let Ok(Some(holder)) = store.is_locked_by_other(path)
    {
        print_yellow!(
            "Skipping locked file (held by {} pid {}): {}",
            holder.tool,
            holder.pid,
            path.display()
        );
        return false;
    }
    // Network paths are deleted directly since trash doesn't work there
    if cli_tools_common::is_network_path(path) {
        println!("{}: {}", colored::Colorize::red("Delete"), path.display());
    } else {
        println!("{}: {}", colored::Colorize::yellow("Trash"), path.display());
    }
    if let Err(error) = delete_file(path) {
        print_yellow!("Failed to delete {}: {error}", path.display());
        return false;
    }
    changes.push(PathChange::removed_file(path));
    if let Some(cache) = scan_cache {
        let _ = cache.remove_cached_path(path);
    }
    true
}

/// Check whether two paths name the same location, resolving case and links.
///
/// Hard links to the same data are separate names, so they do not count as the same location.
fn same_location(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Find the index of the best file to preselect using filename heuristics only.
#[cfg(test)]
pub fn find_best_file_index(files: &[&DupeFileInfo]) -> usize {
    find_best_file_index_with_metadata(files, &HashMap::new())
}

/// Find the index of the best file to keep using probed [`VideoInfo`] metadata when available,
/// falling back to filename resolution and codec labels when metadata is absent.
///
/// Files outside any `Duplicates` directory are always preferred over copies already inside `Duplicates`.
pub fn find_best_file_index_with_metadata(files: &[&DupeFileInfo], metadata: &HashMap<PathBuf, VideoInfo>) -> usize {
    files
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| {
            let left_outside = !is_in_duplicates_directory(&left.path);
            let right_outside = !is_in_duplicates_directory(&right.path);

            let left_score = rank_file(left, metadata.get(&left.path));
            let right_score = rank_file(right, metadata.get(&right.path));

            left_outside
                .cmp(&right_outside)
                .then_with(|| left_score.cmp(&right_score))
        })
        .map_or(0, |(idx, _)| idx)
}

/// Return whether any ancestor directory component of `path` is named `Duplicates`.
fn is_in_duplicates_directory(path: &Path) -> bool {
    path.parent().is_some_and(|parent| {
        parent.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("Duplicates"))
        })
    })
}

/// Composite quality rank for one duplicate candidate.
fn rank_file(file: &DupeFileInfo, info: Option<&VideoInfo>) -> (u64, u8, u64, u64, u8, u8) {
    let (filename_res, filename_codec) = score_file(file);
    info.map_or_else(
        || {
            (
                u64::from(filename_res),
                filename_codec,
                0,
                0,
                filename_res,
                filename_codec,
            )
        },
        |info| {
            let pixels = info
                .resolution
                .map_or_else(|| u64::from(filename_res), |res| res.pixel_count());
            let codec_rank = info.codec.as_deref().map_or(filename_codec, codec_quality_rank);
            let bitrate = info.bitrate_kbps.unwrap_or(0);
            let size = info.size_bytes.unwrap_or(0);
            (pixels, codec_rank, bitrate, size, filename_res, filename_codec)
        },
    )
}

/// Rank codecs for duplicate selection: modern HEVC/AV1 > H.264/AVC > other/unknown.
fn codec_quality_rank(codec: &str) -> u8 {
    match codec.to_ascii_lowercase().as_str() {
        "av1" | "hevc" | "h265" | "x265" => 2,
        "h264" | "avc" | "x264" | "vp9" => 1,
        _ => 0,
    }
}

/// Score a file based on resolution and codec labels when video metadata is unavailable.
fn score_file(file: &DupeFileInfo) -> (u8, u8) {
    let filename_lower = file.filename.to_lowercase();

    let resolution_score = if filename_lower.contains("2160p") || filename_lower.contains(".4k") {
        4
    } else if filename_lower.contains("1440p") {
        3
    } else if filename_lower.contains("1080p") {
        2
    } else {
        u8::from(filename_lower.contains("720p"))
    };

    let codec_score = if filename_lower.contains("x265")
        || filename_lower.contains("hevc")
        || filename_lower.contains("h265")
        || filename_lower.contains("av1")
    {
        2
    } else {
        u8::from(filename_lower.contains("x264") || filename_lower.contains("h264"))
    };

    (resolution_score, codec_score)
}

#[cfg(test)]
mod test_tui_state {
    use super::*;

    #[test]
    fn selection_stays_within_bounds() {
        let mut state = TuiState::new();

        state.select_next(2);
        state.select_next(2);
        assert_eq!(state.selected, 1);

        state.select_prev();
        state.select_prev();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn editing_updates_buffer_and_cursor() {
        let mut state = TuiState::new();
        state.start_editing("name");
        state.insert_char('X');
        state.move_cursor_right();
        state.delete_char_forward();

        assert_eq!(state.edit_buffer, "Xnme");
        assert_eq!(state.cursor_pos, 2);

        state.stop_editing();
        assert!(!state.editing);
        assert_eq!(state.edit_buffer, "");
        assert_eq!(state.cursor_pos, 0);
    }
}

#[cfg(test)]
mod test_file_scoring {
    use std::path::PathBuf;

    use super::*;

    fn make_file(filename: &str) -> DupeFileInfo {
        DupeFileInfo::new(PathBuf::from(filename), "mp4".to_string())
    }

    #[test]
    fn prefers_higher_resolution_then_x265() {
        let lower_resolution = make_file("movie.720p.x265.mp4");
        let high_resolution_x264 = make_file("movie.1080p.x264.mp4");
        let high_resolution_x265 = make_file("movie.1080p.x265.mp4");
        let files = vec![&lower_resolution, &high_resolution_x264, &high_resolution_x265];

        assert_eq!(find_best_file_index(&files), 2);
    }

    #[test]
    fn empty_file_list_defaults_to_first_index() {
        assert_eq!(find_best_file_index(&[]), 0);
    }

    #[test]
    fn prefers_probed_metadata_over_misleading_filename_labels() {
        let low_actual = make_file("movie.2160p.x265.mp4");
        let high_actual = make_file("movie_plain.mp4");
        let mut metadata = HashMap::new();
        metadata.insert(
            low_actual.path.clone(),
            VideoInfo {
                resolution: Some(cli_tools_media::Resolution::new(1280, 720)),
                codec: Some("h264".to_string()),
                bitrate_kbps: Some(2000),
                size_bytes: Some(100_000),
                ..Default::default()
            },
        );
        metadata.insert(
            high_actual.path.clone(),
            VideoInfo {
                resolution: Some(cli_tools_media::Resolution::new(3840, 2160)),
                codec: Some("hevc".to_string()),
                bitrate_kbps: Some(15_000),
                size_bytes: Some(500_000),
                ..Default::default()
            },
        );

        let files = vec![&low_actual, &high_actual];
        assert_eq!(find_best_file_index_with_metadata(&files, &metadata), 1);
    }
}

#[cfg(test)]
mod tui_test_helpers {
    use super::*;

    /// File info for a path, with the extension taken from the name.
    pub fn file(path: &str) -> DupeFileInfo {
        let extension = std::path::Path::new(path)
            .extension()
            .map_or_default(|value| value.to_string_lossy().to_lowercase());
        DupeFileInfo::new(PathBuf::from(path), extension)
    }

    /// Video info with the given dimensions, duration, and codec.
    pub fn video_info(width: u32, height: u32, codec: &str) -> VideoInfo {
        VideoInfo {
            size_bytes: Some(1024 * 1024),
            resolution: Some(cli_tools_media::Resolution::new(width, height)),
            duration: Some(120.0),
            codec: Some(codec.to_string()),
            bitrate_kbps: Some(4000),
            ..Default::default()
        }
    }

    /// Render the interface into a test buffer and return it as one string per row.
    pub fn render(
        files: &[&DupeFileInfo],
        state: &TuiState,
        metadata: &HashMap<PathBuf, VideoInfo>,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height))
            .expect("the test terminal should be created");
        let mut list_state = ListState::default();
        terminal
            .draw(|frame| render_ui(frame, "group-key", files, state, &mut list_state, 1, 3, metadata))
            .expect("the interface should render");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer.cell((column, row)).map_or(" ", ratatui::buffer::Cell::symbol))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod test_format_file_detail_lines {
    use super::tui_test_helpers::*;
    use super::*;

    /// The visible text of the rendered lines, one string per line.
    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn the_selected_file_is_marked_with_an_arrow() {
        let info = file("/videos/movie.1080p.mp4");

        let selected = text(&format_file_detail_lines(&info, 0, 0, &HashMap::new()));
        let other = text(&format_file_detail_lines(&info, 1, 0, &HashMap::new()));

        assert!(selected[0].starts_with("► "), "{selected:?}");
        assert!(other[0].starts_with("  "), "{other:?}");
    }

    #[test]
    fn the_path_is_shown_in_full() {
        let info = file("/videos/movie.1080p.mp4");

        let lines = text(&format_file_detail_lines(&info, 0, 0, &HashMap::new()));

        assert!(lines[0].contains("/videos/movie.1080p.mp4"), "{lines:?}");
    }

    #[test]
    fn known_metadata_is_listed_for_the_file() {
        let info = file("/videos/movie.mp4");
        let mut metadata = HashMap::new();
        metadata.insert(info.path.clone(), video_info(1920, 1080, "hevc"));

        let joined = text(&format_file_detail_lines(&info, 0, 0, &metadata)).join("\n");

        assert!(joined.contains("1080"), "the resolution should be shown: {joined}");
        assert!(joined.contains("hevc"), "the codec should be shown: {joined}");
    }

    #[test]
    fn a_file_without_metadata_still_renders() {
        let info = file("/videos/movie.mp4");

        let lines = format_file_detail_lines(&info, 0, 0, &HashMap::new());

        assert_ne!(lines, [] as [Line<'_>; 0]);
    }
}

#[cfg(test)]
mod test_render_ui {
    use super::tui_test_helpers::*;
    use super::*;

    #[test]
    fn shows_the_group_position_and_every_file_name() {
        let first = file("/videos/movie.1080p.mp4");
        let second = file("/videos/movie.720p.mp4");
        let files = vec![&first, &second];

        let rows = render(&files, &TuiState::new(), &HashMap::new(), 120, 40).join("\n");

        assert!(rows.contains("movie.1080p.mp4"), "{rows}");
        assert!(rows.contains("movie.720p.mp4"), "{rows}");
        assert!(rows.contains('3'), "the group count should be shown: {rows}");
    }

    #[test]
    fn edit_mode_shows_the_buffer_being_typed() {
        let only = file("/videos/movie.mp4");
        let files = vec![&only];
        let mut state = TuiState::new();
        state.start_editing("new name");

        let rows = render(&files, &state, &HashMap::new(), 120, 40).join("\n");

        assert!(rows.contains("new name"), "the edit buffer should be shown: {rows}");
    }

    #[test]
    fn the_confirmation_dialog_is_rendered() {
        let only = file("/videos/movie.mp4");
        let files = vec![&only];
        let mut state = TuiState::new();
        state.confirming = true;

        let rows = render(&files, &state, &HashMap::new(), 120, 40).join("\n");

        assert!(!rows.trim().is_empty(), "the dialog should draw something");
    }

    #[test]
    fn rename_only_mode_is_rendered() {
        let only = file("/videos/movie.mp4");
        let files = vec![&only];
        let mut state = TuiState::new();
        state.start_editing("renamed");
        state.rename_only = true;

        let rows = render(&files, &state, &HashMap::new(), 120, 40).join("\n");

        assert!(rows.contains("renamed"), "{rows}");
    }

    #[test]
    fn metadata_is_rendered_for_the_listed_files() {
        let video = PathBuf::from("/videos/movie.mp4");
        let only = file("/videos/movie.mp4");
        let files = vec![&only];
        let mut metadata = HashMap::new();
        metadata.insert(video, video_info(3840, 2160, "hevc"));

        let rows = render(&files, &TuiState::new(), &metadata, 120, 40).join("\n");

        assert!(rows.contains("2160"), "{rows}");
    }

    #[test]
    fn a_narrow_short_terminal_does_not_panic() {
        let only = file("/videos/movie.mp4");
        let files = vec![&only];

        let rows = render(&files, &TuiState::new(), &HashMap::new(), 20, 8);

        assert_eq!(rows.len(), 8);
    }

    #[test]
    fn many_files_do_not_overflow_the_layout() {
        let owned: Vec<DupeFileInfo> = (0..12)
            .map(|index| file(&format!("/videos/movie.{index}.mp4")))
            .collect();
        let files: Vec<&DupeFileInfo> = owned.iter().collect();

        let rows = render(&files, &TuiState::new(), &HashMap::new(), 100, 24);

        assert_eq!(rows.len(), 24);
    }
}

#[cfg(test)]
mod test_apply_actions {
    use super::tui_test_helpers::*;
    use super::*;

    /// Temporary directory holding one file per given name.
    fn group_in(directory: &std::path::Path, names: &[&str]) -> DuplicateGroup {
        let files = names
            .iter()
            .map(|name| {
                let path = directory.join(name);
                std::fs::write(&path, b"content").expect("file should be written");
                file(&path.to_string_lossy())
            })
            .collect();
        DuplicateGroup::new("movie".to_string(), files)
    }

    #[test]
    fn skip_and_quit_actions_do_nothing() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.mp4"]);
        let actions = vec![
            GroupAction {
                group_index: 0,
                action: DuplicateAction::Skip,
            },
            GroupAction {
                group_index: 0,
                action: DuplicateAction::Quit,
            },
        ];

        apply_actions(&[group], &actions).expect("skipped groups should succeed");

        assert!(directory.path().join("movie.mp4").is_file(), "nothing should change");
    }

    #[test]
    fn rename_only_renames_the_selected_file() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.mp4"]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::RenameOnly {
                rename_index: 0,
                new_name: "better.name".to_string(),
            },
        }];

        apply_actions(&[group], &actions).expect("the rename should succeed");

        assert!(directory.path().join("better.name.mp4").is_file());
        assert!(!directory.path().join("movie.mp4").exists());
    }

    #[test]
    fn a_rename_to_the_same_name_is_a_no_op() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.mp4"]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::RenameOnly {
                rename_index: 0,
                new_name: "movie".to_string(),
            },
        }];

        apply_actions(&[group], &actions).expect("the rename should succeed");

        assert!(directory.path().join("movie.mp4").is_file());
    }

    #[test]
    fn keeping_the_only_file_renames_it_without_deleting_anything() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.mp4"]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: Some("kept".to_string()),
            },
        }];

        apply_actions(&[group], &actions).expect("the rename should succeed");

        assert!(directory.path().join("kept.mp4").is_file());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("directory should be readable")
                .count(),
            1
        );
    }

    #[test]
    fn keeping_the_only_file_without_a_new_name_changes_nothing() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.mp4"]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: None,
            },
        }];

        apply_actions(&[group], &actions).expect("the action should succeed");

        assert!(directory.path().join("movie.mp4").is_file());
    }

    #[test]
    fn a_group_index_outside_the_list_is_an_error() {
        let actions = vec![GroupAction {
            group_index: 7,
            action: DuplicateAction::RenameOnly {
                rename_index: 0,
                new_name: "name".to_string(),
            },
        }];

        let error = apply_actions(&[], &actions).expect_err("an unknown group should fail");

        assert!(error.to_string().contains("out of bounds"), "{error}");
    }

    #[test]
    fn a_file_index_outside_the_group_is_an_error() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_in(directory.path(), &["movie.mp4"]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::RenameOnly {
                rename_index: 5,
                new_name: "name".to_string(),
            },
        }];

        let error = apply_actions(&[group], &actions).expect_err("an unknown file should fail");

        assert!(error.to_string().contains("unavailable"), "{error}");
    }

    /// Group of files written with the given contents.
    fn group_with_contents(directory: &std::path::Path, files: &[(&str, &str)]) -> DuplicateGroup {
        let files = files
            .iter()
            .map(|(name, content)| {
                let path = directory.join(name);
                std::fs::write(&path, content).expect("file should be written");
                file(&path.to_string_lossy())
            })
            .collect();
        DuplicateGroup::new("movie".to_string(), files)
    }

    /// Delete files directly so tests never touch the system trash.
    fn remove_file(path: &std::path::Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }

    /// Read a file in the directory, or `None` when it does not exist.
    fn read(directory: &std::path::Path, name: &str) -> Option<String> {
        std::fs::read_to_string(directory.join(name)).ok()
    }

    #[test]
    fn missing_keeper_does_not_delete_remaining_copies() {
        let tree = cli_tools_common::test_support::TempTree::new();
        let group = group_with_contents(tree.path(), &[("a.mp4", "VIDEO"), ("b.mp4", "VIDEO")]);
        let actions = [GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: None,
            },
        }];
        std::fs::remove_file(tree.join("a.mp4")).unwrap();

        let result = apply_actions_with(&[group], &actions, &remove_file, &mut Vec::new());

        assert_eq!(tree.files(), ["b.mp4"], "the remaining copy should not be deleted");
        assert_eq!(tree.read("b.mp4"), "VIDEO");
        assert!(result.unwrap_err().to_string().contains("a.mp4"));
    }

    #[test]
    fn directory_replacing_keeper_does_not_delete_rename_target() {
        let tree = cli_tools_common::test_support::TempTree::new();
        let group = group_with_contents(tree.path(), &[("a.mp4", "VIDEO"), ("b.mp4", "VIDEO")]);
        let actions = [GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: Some("b".to_string()),
            },
        }];
        std::fs::remove_file(tree.join("a.mp4")).unwrap();
        tree.dir("a.mp4");

        let result = apply_actions_with(&[group], &actions, &remove_file, &mut Vec::new());

        assert_eq!(tree.files(), ["b.mp4"], "the rename target should not be deleted");
        assert_eq!(tree.read("b.mp4"), "VIDEO");
        assert!(tree.join("a.mp4").is_dir());
        assert!(result.unwrap_err().to_string().contains("a.mp4"));
    }

    #[test]
    fn keep_and_rename_onto_another_group_member_keeps_the_kept_content() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_with_contents(directory.path(), &[("Movie.1080p.mkv", "KEEP"), ("Movie.mkv", "OTHER")]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: Some("Movie".to_string()),
            },
        }];

        apply_actions_with(&[group], &actions, &remove_file, &mut Vec::new()).expect("the action should succeed");

        assert_eq!(read(directory.path(), "Movie.mkv").as_deref(), Some("KEEP"));
        assert_eq!(read(directory.path(), "Movie.1080p.mkv"), None);
    }

    #[test]
    fn keep_and_rename_onto_an_unrelated_file_keeps_that_file() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        std::fs::write(directory.path().join("c.mp4"), "UNRELATED").expect("file should be written");
        let group = group_with_contents(directory.path(), &[("a.mp4", "A"), ("b.mp4", "B")]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: Some("c".to_string()),
            },
        }];

        apply_actions_with(&[group], &actions, &remove_file, &mut Vec::new()).expect("the action should succeed");

        assert_eq!(read(directory.path(), "c.mp4").as_deref(), Some("UNRELATED"));
        assert_eq!(read(directory.path(), "a.mp4").as_deref(), Some("A"));
        assert_eq!(read(directory.path(), "b.mp4"), None);
    }

    #[test]
    fn keep_with_rename_reports_the_rename_and_the_deletions() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_with_contents(
            directory.path(),
            &[
                ("Movie.1080p.mkv", "KEEP"),
                ("Movie.720p.mkv", "OTHER"),
                ("Movie.mp4", "THIRD"),
            ],
        );
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: Some("Movie".to_string()),
            },
        }];
        let mut changes = Vec::new();

        apply_actions_with(&[group], &actions, &remove_file, &mut changes).expect("the action should succeed");

        assert_eq!(
            changes,
            vec![
                PathChange::moved_file(
                    directory.path().join("Movie.1080p.mkv"),
                    directory.path().join("Movie.mkv")
                ),
                PathChange::removed_file(directory.path().join("Movie.720p.mkv")),
                PathChange::removed_file(directory.path().join("Movie.mp4")),
            ]
        );
    }

    #[test]
    fn rename_only_reports_the_rename() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_with_contents(directory.path(), &[("a.mp4", "A"), ("b.mp4", "B")]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::RenameOnly {
                rename_index: 0,
                new_name: "c".to_string(),
            },
        }];
        let mut changes = Vec::new();

        apply_actions_with(&[group], &actions, &remove_file, &mut changes).expect("the action should succeed");

        assert_eq!(
            changes,
            vec![PathChange::moved_file(
                directory.path().join("a.mp4"),
                directory.path().join("c.mp4")
            )]
        );
    }

    #[test]
    fn skipped_renames_and_failed_deletions_are_not_reported() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_with_contents(directory.path(), &[("a.mp4", "A"), ("b.mp4", "B")]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::RenameOnly {
                rename_index: 0,
                new_name: "b".to_string(),
            },
        }];
        let mut changes = Vec::new();
        let fail_delete = |_: &std::path::Path| -> std::io::Result<()> { Err(std::io::Error::other("refused")) };

        apply_actions_with(std::slice::from_ref(&group), &actions, &remove_file, &mut changes)
            .expect("the action should succeed");
        let keep = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::Keep {
                keep_index: 0,
                new_name: None,
            },
        }];
        apply_actions_with(&[group], &keep, &fail_delete, &mut changes).expect("the action should succeed");

        assert_eq!(changes, Vec::new());
    }

    #[test]
    fn build_renamed_path_rejects_path_traversal_and_handles_empty_extension() {
        let original = Path::new("/videos/show/clip.mp4");
        assert!(build_renamed_path(original, "../escape", "mp4").is_err());
        assert!(build_renamed_path(original, "sub/clip", "mp4").is_err());
        assert!(build_renamed_path(original, "..", "mp4").is_err());
        assert!(build_renamed_path(original, "  ", "mp4").is_err());
        assert_eq!(
            build_renamed_path(Path::new("/videos/clip"), "renamed", "").expect("empty ext"),
            PathBuf::from("/videos/renamed")
        );
    }

    #[test]
    fn rename_only_onto_an_existing_file_keeps_both_files() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let group = group_with_contents(directory.path(), &[("a.mp4", "A"), ("b.mp4", "B")]);
        let actions = vec![GroupAction {
            group_index: 0,
            action: DuplicateAction::RenameOnly {
                rename_index: 0,
                new_name: "b".to_string(),
            },
        }];

        apply_actions_with(&[group], &actions, &remove_file, &mut Vec::new()).expect("the action should succeed");

        assert_eq!(read(directory.path(), "a.mp4").as_deref(), Some("A"));
        assert_eq!(read(directory.path(), "b.mp4").as_deref(), Some("B"));
    }
}
