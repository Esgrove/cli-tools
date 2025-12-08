use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use itertools::Itertools;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::FileInfo;

use cli_tools::print_warning;

/// Action to perform on a duplicate group
#[derive(Debug, Clone)]
enum DuplicateAction {
    /// Keep the file at this index, delete others
    Keep {
        keep_index: usize,
        new_name: Option<String>,
    },
    /// Skip this group
    Skip,
    /// Quit the interactive session
    Quit,
}

/// State for the interactive TUI
struct TuiState {
    /// Current selection index in the file list
    selected: usize,
    /// Whether we're in rename mode
    editing: bool,
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
        self.cursor_pos = self.edit_buffer.len();
    }

    fn stop_editing(&mut self) {
        self.editing = false;
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
pub fn run_interactive(duplicates: &[(String, Vec<FileInfo>)]) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let actions = interactive_loop(&mut terminal, duplicates)?;

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
    duplicates: &[(String, Vec<FileInfo>)],
) -> anyhow::Result<Vec<(usize, DuplicateAction)>> {
    let mut group_index = 0;
    let mut actions: Vec<(usize, DuplicateAction)> = Vec::new();

    while group_index < duplicates.len() {
        let (key, files) = &duplicates[group_index];
        let sorted_files: Vec<&FileInfo> = files.iter().sorted_by_key(|f| &f.path).collect();

        let action = handle_duplicate_group(terminal, key, &sorted_files, group_index, duplicates.len())?;

        match action {
            DuplicateAction::Quit => break,
            DuplicateAction::Skip => {
                group_index += 1;
            }
            DuplicateAction::Keep { keep_index, new_name } => {
                actions.push((group_index, DuplicateAction::Keep { keep_index, new_name }));
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
) -> anyhow::Result<DuplicateAction> {
    let mut state = TuiState::new();
    let mut list_state = ListState::default();
    list_state.select(Some(0));

    loop {
        terminal.draw(|frame| {
            render_ui(frame, key, files, &state, &mut list_state, current_group, total_groups);
        })?;

        if let Event::Key(key_event) = event::read()? {
            if key_event.kind != KeyEventKind::Press {
                continue;
            }

            if state.editing {
                match key_event.code {
                    KeyCode::Esc => state.stop_editing(),
                    KeyCode::Enter => {
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
                    KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(DuplicateAction::Quit);
                    }
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
                    _ => {}
                }
            }
        }
    }
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
) {
    let area = frame.area();

    // Create layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(10),   // File list
            Constraint::Length(3), // Status/Edit area
            Constraint::Length(3), // Help
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
            let path_str = file.path.display().to_string();
            let style = if i == state.selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(format!("{prefix}{path_str}")).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Files (↑/↓ to select)"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], list_state);

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

    let status =
        Paragraph::new(status_content)
            .style(status_style)
            .block(Block::default().borders(Borders::ALL).title(if state.editing {
                "Rename (Enter to confirm, Esc to cancel)"
            } else {
                "Status"
            }));
    frame.render_widget(status, chunks[2]);

    // Help
    let help_text = if state.editing {
        "Type new name | Enter: confirm | Esc: cancel"
    } else if state.confirming {
        "y: confirm | n: cancel"
    } else {
        "Enter: keep selected | r: rename & keep | s: skip | q: quit"
    };
    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title("Help"));
    frame.render_widget(help, chunks[3]);
}

/// Apply all collected actions
fn apply_actions(duplicates: &[(String, Vec<FileInfo>)], actions: &[(usize, DuplicateAction)]) -> anyhow::Result<()> {
    for (group_idx, action) in actions {
        let (_, files) = &duplicates[*group_idx];
        let sorted_files: Vec<&FileInfo> = files.iter().sorted_by_key(|f| &f.path).collect();

        if let DuplicateAction::Keep { keep_index, new_name } = action {
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
                        print_warning!("Failed to delete {}: {e}", file.path.display());
                    }
                }
            }
        }
    }

    Ok(())
}
