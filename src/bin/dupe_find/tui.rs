use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use itertools::Itertools;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use cli_tools::print_yellow;

use cli_tools::video_info::VideoInfo;

use crate::dupe_find::{DuplicateGroup, FileInfo};

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

    const fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    const fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.edit_buffer.len() {
            self.cursor_pos += 1;
        }
    }

    fn insert_char(&mut self, c: char) {
        self.edit_buffer.insert(self.cursor_pos, c);
        self.cursor_pos += 1;
    }

    fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            self.edit_buffer.remove(self.cursor_pos - 1);
            self.cursor_pos -= 1;
        }
    }

    fn delete_char_forward(&mut self) {
        if self.cursor_pos < self.edit_buffer.len() {
            self.edit_buffer.remove(self.cursor_pos);
        }
    }
}

/// Run interactive TUI mode for handling duplicates
pub fn run_interactive(duplicates: &[DuplicateGroup], metadata: &HashMap<PathBuf, VideoInfo>) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let actions = interactive_loop(&mut terminal, duplicates, metadata)?;

    // Restore terminal
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;

    // Apply actions after terminal is restored so warnings display correctly
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
        let group = &duplicates[group_index];
        let sorted_files: Vec<&FileInfo> = group.files.iter().sorted_by_key(|f| &f.path).collect();

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
    files: &[&FileInfo],
    current_group: usize,
    total_groups: usize,
    metadata: &HashMap<PathBuf, VideoInfo>,
) -> anyhow::Result<DuplicateAction> {
    let best_index = find_best_file_index(files);
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

            if state.editing {
                match key_event.code {
                    KeyCode::Esc => state.stop_editing(),
                    KeyCode::Enter => {
                        if state.rename_only {
                            if state.edit_buffer.is_empty() {
                                state.stop_editing();
                            } else {
                                return Ok(DuplicateAction::RenameOnly {
                                    rename_index: state.selected,
                                    new_name: state.edit_buffer.clone(),
                                });
                            }
                        } else {
                            let new_name = if state.edit_buffer.is_empty() {
                                None
                            } else {
                                Some(state.edit_buffer.clone())
                            };
                            return Ok(DuplicateAction::Keep {
                                keep_index: state.selected,
                                new_name,
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
                        let selected_file = files[state.selected];
                        state.start_editing(&selected_file.stem);
                    }
                    KeyCode::Char('n') => {
                        let selected_file = files[state.selected];
                        state.start_editing(&selected_file.stem);
                        state.rename_only = true;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Format a metadata detail line for a single file.
fn format_file_detail_lines(
    file: &FileInfo,
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
        let size_str = cli_tools::format_size(meta.size_bytes.unwrap_or(0));

        let duration_str = meta
            .duration
            .map_or_else(|| "N/A".to_string(), cli_tools::format_duration_seconds);

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
    files: &[&FileInfo],
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

    // Calculate how many lines the details section needs
    // 2 for borders + 3 lines per file (path, metadata, separator)
    let details_height = (files.len() as u16)
        .saturating_mul(3)
        .saturating_add(2)
        .min(area.height / 2);

    // Create layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),                // Header
            Constraint::Length(file_list_height), // File list (compact)
            Constraint::Length(details_height),   // File details
            Constraint::Length(3),                // Status/Edit area
            Constraint::Length(3),                // Help
            Constraint::Min(0),                   // Spacer (unused space below)
        ])
        .split(area);

    // Header
    let header_text = format!("Duplicate Group {}/{}: {}", current_group + 1, total_groups, key);
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).title("Duplicate Finder"));
    frame.render_widget(header, chunks[0]);

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
    frame.render_stateful_widget(list, chunks[1], list_state);

    // File details panel
    let mut detail_lines: Vec<Line> = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let lines = format_file_detail_lines(file, index, state.selected, metadata);
        detail_lines.extend(lines);
    }

    let details = Paragraph::new(detail_lines).block(Block::default().borders(Borders::ALL).title("File Details"));
    frame.render_widget(details, chunks[2]);

    // Status/Edit area
    let status_content = if state.editing {
        let before_cursor = &state.edit_buffer[..state.cursor_pos];
        let after_cursor = &state.edit_buffer[state.cursor_pos..];
        let extension = &files[state.selected].extension;
        format!("New name: {before_cursor}│{after_cursor}.{extension}")
    } else if state.confirming {
        format!(
            "Keep '{}' and delete {} other file(s)? (y/n)",
            files[state.selected].filename,
            files.len() - 1
        )
    } else {
        format!("Selected: {}", files[state.selected].filename)
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
    frame.render_widget(status, chunks[3]);

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

    frame.render_widget(help, chunks[4]);
}

/// Apply all collected actions
fn apply_actions(duplicates: &[DuplicateGroup], actions: &[GroupAction]) -> anyhow::Result<()> {
    let actionable: Vec<&GroupAction> = actions
        .iter()
        .filter(|a| !matches!(a.action, DuplicateAction::Skip | DuplicateAction::Quit))
        .collect();
    let total = actionable.len();

    for (number, group_action) in actionable.into_iter().enumerate() {
        let group = &duplicates[group_action.group_index];
        let sorted_files: Vec<&FileInfo> = group.files.iter().sorted_by_key(|f| &f.path).collect();

        println!(
            "{}",
            colored::Colorize::bold(colored::Colorize::white(
                format!("── Group {}/{total} ──", number + 1).as_str()
            ))
        );

        match &group_action.action {
            DuplicateAction::Keep { keep_index, new_name } => {
                let keep_file = sorted_files[*keep_index];

                // Handle rename if specified
                if let Some(new_stem) = new_name {
                    let new_filename = format!("{new_stem}.{}", keep_file.extension);
                    let new_path = keep_file.path.with_file_name(&new_filename);

                    if new_path != keep_file.path {
                        println!("{}", colored::Colorize::cyan("Rename:"));
                        cli_tools::show_diff(
                            &cli_tools::path_to_string_relative(&keep_file.path),
                            &cli_tools::path_to_string_relative(&new_path),
                        );
                        std::fs::rename(&keep_file.path, &new_path)?;
                    }
                }

                // Delete other files
                for (i, file) in sorted_files.iter().enumerate() {
                    if i != *keep_index {
                        // Use direct delete for network paths since trash doesn't work there
                        let result = if cli_tools::is_network_path(&file.path) {
                            println!("{}: {}", colored::Colorize::red("Delete"), file.path.display());
                            std::fs::remove_file(&file.path)
                        } else {
                            println!("{}: {}", colored::Colorize::yellow("Trash"), file.path.display());
                            trash::delete(&file.path).map_err(std::io::Error::other)
                        };
                        if let Err(e) = result {
                            print_yellow!("Failed to delete {}: {e}", file.path.display());
                        }
                    }
                }
            }
            DuplicateAction::RenameOnly { rename_index, new_name } => {
                let rename_file = sorted_files[*rename_index];
                let new_filename = format!("{new_name}.{}", rename_file.extension);
                let new_path = rename_file.path.with_file_name(&new_filename);

                if new_path != rename_file.path {
                    println!("{}", colored::Colorize::cyan("Rename:"));
                    cli_tools::show_diff(
                        &cli_tools::path_to_string_relative(&rename_file.path),
                        &cli_tools::path_to_string_relative(&new_path),
                    );
                    std::fs::rename(&rename_file.path, &new_path)?;
                }
            }
            DuplicateAction::Skip | DuplicateAction::Quit => unreachable!(),
        }
    }

    Ok(())
}

/// Find the index of the best file to preselect based on resolution and codec.
fn find_best_file_index(files: &[&FileInfo]) -> usize {
    files
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            let (res_a, x265_a) = score_file(a);
            let (res_b, x265_b) = score_file(b);

            // First compare by resolution (higher is better)
            res_a.cmp(&res_b).then_with(|| {
                // If resolution is equal, prefer x265
                x265_a.cmp(&x265_b)
            })
        })
        .map_or(0, |(idx, _)| idx)
}

/// Score a file based on resolution and codec labels.
/// Higher score = better quality. Returns (`resolution_score`, `has_x265`).
fn score_file(file: &FileInfo) -> (u8, bool) {
    let filename_lower = file.filename.to_lowercase();

    // Resolution score: higher resolution = higher score
    let resolution_score = if filename_lower.contains(".2160p") {
        4
    } else if filename_lower.contains(".1440p") {
        3
    } else if filename_lower.contains(".1080p") {
        2
    } else {
        u8::from(filename_lower.contains(".720p"))
    };

    let has_x265 = filename_lower.contains(".x265");

    (resolution_score, has_x265)
}
