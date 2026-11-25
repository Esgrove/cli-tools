//! Interactive `indicatif` progress layout for the `rich` output mode.
//!
//! Holds the overall progress bar state, the adaptive per-task console preview height,
//! and the terminal wrapper that keeps redraws from soft wrapping in terminals such as iTerm2.

use std::io;
use std::sync::Mutex;

use console::Term;
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle, TermLike};

use super::{
    DEFAULT_EXPANDED_TAIL_LINES, DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS, MAX_EXPANDED_CONCURRENCY,
    MIN_EXPANDED_TERMINAL_ROWS, StylePalette,
};
use crate::scheduler::TaskOutcome;

/// Where the last write left the cursor within an `indicatif` redraw.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrawWritePhase {
    AtLineStart,
    ExpectingFiller,
    AfterLine,
}

/// Overall progress bar and outcome counters of the interactive view.
pub(super) struct InteractiveState {
    pub(super) multi: MultiProgress,
    pub(super) overall_bar: ProgressBar,
    pub(super) task_style: ProgressStyle,
    pub(super) fixed_geometry: Option<(usize, usize)>,
    pub(super) total_tasks: usize,
    pub(super) completed_tasks: usize,
    pub(super) passed_tasks: usize,
    pub(super) failed_tasks: usize,
    pub(super) skipped_tasks: usize,
    pub(super) blocked_tasks: usize,
}

/// Terminal wrapper that replaces right margin filler with an end of line clear and explicit newlines.
#[derive(Debug)]
pub(super) struct CleanLineTerm {
    inner: Term,
    phase: Mutex<DrawWritePhase>,
}

impl InteractiveState {
    pub(super) fn record_outcome(&mut self, outcome: &TaskOutcome) {
        self.completed_tasks = self.completed_tasks.saturating_add(1);
        match outcome {
            TaskOutcome::Passed => self.passed_tasks = self.passed_tasks.saturating_add(1),
            TaskOutcome::Failed(_) => self.failed_tasks = self.failed_tasks.saturating_add(1),
            TaskOutcome::Skipped(_) => self.skipped_tasks = self.skipped_tasks.saturating_add(1),
            TaskOutcome::Blocked(_) => self.blocked_tasks = self.blocked_tasks.saturating_add(1),
            TaskOutcome::DryRun => {}
        }
        self.overall_bar.set_position(self.completed_tasks as u64);
    }

    pub(super) fn status_message(&self, palette: &StylePalette, active_len: usize) -> String {
        let pass_part = if self.passed_tasks > 0 {
            palette.pass.apply_to(format!("{} pass", self.passed_tasks)).to_string()
        } else {
            palette.dim.apply_to("0 pass").to_string()
        };
        let fail_part = if self.failed_tasks > 0 {
            palette.fail.apply_to(format!("{} fail", self.failed_tasks)).to_string()
        } else {
            palette.dim.apply_to("0 fail").to_string()
        };
        let running_part = palette.info.apply_to(format!("{active_len} running")).to_string();
        format!("{pass_part} │ {fail_part} │ {running_part}")
    }

    pub(super) fn terminal_geometry(&self) -> (usize, usize) {
        if let Some(fixed) = self.fixed_geometry {
            return fixed;
        }
        let (rows, cols) = Term::stderr().size();
        let resolved_rows = if rows > 0 {
            usize::from(rows)
        } else {
            DEFAULT_TERMINAL_ROWS
        };
        let resolved_cols = if cols > 0 {
            usize::from(cols)
        } else {
            DEFAULT_TERMINAL_COLS
        };
        (resolved_rows, resolved_cols)
    }
}

impl CleanLineTerm {
    pub(super) const fn new(inner: Term) -> Self {
        Self {
            inner,
            phase: Mutex::new(DrawWritePhase::AtLineStart),
        }
    }

    fn set_phase(&self, next: DrawWritePhase) {
        *self.phase.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }

    fn advance_write_str<W: FnMut(&str) -> io::Result<()>>(&self, input: &str, mut write: W) -> io::Result<()> {
        let mut phase = self.phase.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if input == "\r" {
            *phase = DrawWritePhase::AtLineStart;
            drop(phase);
            return write("\r");
        }
        match *phase {
            DrawWritePhase::AtLineStart => {
                *phase = DrawWritePhase::ExpectingFiller;
                drop(phase);
                write(input)
            }
            DrawWritePhase::ExpectingFiller => {
                *phase = DrawWritePhase::AfterLine;
                drop(phase);
                write("\x1b[0K")
            }
            DrawWritePhase::AfterLine => {
                *phase = DrawWritePhase::ExpectingFiller;
                drop(phase);
                write("\n")?;
                write(input)
            }
        }
    }
}

impl TermLike for CleanLineTerm {
    fn width(&self) -> u16 {
        // Reserve 1 margin column so truncated lines never hit the rightmost terminal column.
        self.inner.size().1.saturating_sub(1).max(1)
    }

    fn height(&self) -> u16 {
        self.inner.size().0.max(1)
    }

    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        self.set_phase(DrawWritePhase::AtLineStart);
        self.inner.move_cursor_up(n)
    }

    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        self.set_phase(DrawWritePhase::AtLineStart);
        self.inner.move_cursor_down(n)
    }

    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        self.set_phase(DrawWritePhase::AtLineStart);
        self.inner.move_cursor_right(n)
    }

    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        self.set_phase(DrawWritePhase::AtLineStart);
        self.inner.move_cursor_left(n)
    }

    fn write_line(&self, s: &str) -> io::Result<()> {
        let needs_newline = {
            let mut phase = self.phase.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_after = *phase == DrawWritePhase::AfterLine;
            *phase = DrawWritePhase::AtLineStart;
            was_after
        };
        if needs_newline {
            self.inner.write_str("\n")?;
        }
        self.inner.write_line(s)
    }

    fn write_str(&self, s: &str) -> io::Result<()> {
        // `indicatif::DrawState::draw_to_term` writes each line as two consecutive `write_str` calls:
        // 1. `write_str(line)` (content)
        // 2. `write_str(&" ".repeat(line_filler))` (spaces to reach the right margin and rely on auto-wrap)
        // Relying on right-margin pending-wrap (DECAWM) breaks `move_cursor_up` on terminals such as iTerm2,
        // gluing `PASS` lines side-by-side and scrolling the progress bar down on every steady tick.
        // Instead, emit `\x1b[0K` on the filler call and `\n` before the next line within the same frame,
        // leaving the cursor on the final bar line (matching `bar_count - 1` on the next redraw).
        self.advance_write_str(s, |chunk| self.inner.write_str(chunk))
    }

    fn clear_line(&self) -> io::Result<()> {
        self.set_phase(DrawWritePhase::AtLineStart);
        self.inner.clear_line()
    }

    fn flush(&self) -> io::Result<()> {
        self.set_phase(DrawWritePhase::AtLineStart);
        self.inner.flush()
    }
}

/// Compute the effective console preview line count per active task, clamped to terminal height.
///
/// - `0`: spinner, task ID, elapsed timer, and lock badge only (1 row per task).
/// - `1`: compact Gradle-style 1-line slot with latest output line inline (1 row per task).
/// - `>= 2`: expanded mini-console with 1 header row plus `N` output rows (`N + 1` rows per task).
#[must_use]
pub fn compute_effective_tail(active_count: usize, rows: usize, explicit_tail: Option<usize>) -> usize {
    let target = explicit_tail.unwrap_or(
        if active_count > MAX_EXPANDED_CONCURRENCY || rows < MIN_EXPANDED_TERMINAL_ROWS {
            1
        } else {
            DEFAULT_EXPANDED_TAIL_LINES
        },
    );

    if active_count == 0 || target == 0 {
        return 0;
    }

    // Reserve 1 row for the overall progress bar and 1 row of terminal margin.
    let available_rows = rows.saturating_sub(2);
    if available_rows < active_count {
        return 0;
    }

    if target == 1 {
        return 1;
    }

    let needed_rows = active_count.saturating_mul(target.saturating_add(1));
    if needed_rows <= available_rows {
        return target;
    }

    let max_rows_per_task = available_rows.checked_div(active_count).unwrap_or(0);
    if max_rows_per_task >= 3 {
        max_rows_per_task.saturating_sub(1)
    } else {
        usize::from(max_rows_per_task >= 1)
    }
}

pub(super) fn build_overall_progress_style(colors_enabled: bool) -> ProgressStyle {
    let template = if colors_enabled {
        "[{bar:20.cyan/dim}] {pos}/{len} tasks │ {msg} │ {elapsed_precise}"
    } else {
        "[{bar:20}] {pos}/{len} tasks │ {msg} │ {elapsed_precise}"
    };
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("█▓░")
}

pub(super) fn build_task_spinner_style(colors_enabled: bool) -> ProgressStyle {
    let template = if colors_enabled {
        "  {spinner:.cyan} {prefix} ({elapsed_tenths}){msg}"
    } else {
        "  {spinner} {prefix} ({elapsed_tenths}){msg}"
    };
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "])
        .with_key(
            "elapsed_tenths",
            |state: &ProgressState, writer: &mut dyn std::fmt::Write| {
                let _ = write!(writer, "{:.1}s", state.elapsed().as_secs_f64());
            },
        )
}

#[cfg(test)]
mod test_adaptive_tail {
    use super::*;

    #[test]
    fn adaptive_tail_switches_between_compact_and_expanded_and_clamps_to_height() {
        // <= 3 active tasks on a >= 24-row terminal expands to 3 tail lines.
        assert_eq!(compute_effective_tail(2, 30, None), 3);
        assert_eq!(compute_effective_tail(3, 24, None), 3);

        // > 3 active tasks or < 24 rows switches to compact 1-line mode.
        assert_eq!(compute_effective_tail(4, 30, None), 1);
        assert_eq!(compute_effective_tail(2, 20, None), 1);

        // Explicit --tail-lines overrides adaptive defaults while still clamping to terminal height.
        assert_eq!(compute_effective_tail(2, 40, Some(0)), 0);
        assert_eq!(compute_effective_tail(2, 40, Some(1)), 1);
        assert_eq!(compute_effective_tail(2, 40, Some(5)), 5);

        // 14 rows -> 12 available rows across 3 tasks -> 4 rows/task -> 3 tail lines.
        assert_eq!(compute_effective_tail(3, 14, Some(5)), 3);
        // 10 rows -> 8 available rows across 4 tasks -> 2 rows/task -> degrades to compact 1-line.
        assert_eq!(compute_effective_tail(4, 10, Some(4)), 1);
        // Tiny terminal where even 1 row per task cannot fit -> 0.
        assert_eq!(compute_effective_tail(6, 5, Some(3)), 0);
    }
}

#[cfg(test)]
mod test_interactive_reporter {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::cli::ResolvedOutputMode;
    use crate::scheduler::TaskReport;
    use crate::ui::{Reporter, ReporterBackend, UiConfig};

    #[test]
    fn interactive_reporter_transitions_from_compact_to_expanded_mini_console() {
        let config = UiConfig {
            mode: ResolvedOutputMode::Rich,
            colors_enabled: false,
            github_actions: false,
            tail_lines: None,
            dry_run: false,
            rerun_prefix: "xtask".to_owned(),
        };
        let reporter = Reporter::interactive_hidden(config, (30, 80));
        reporter.on_plan(5, 4, "all");

        // Start 4 tasks -> > 3 active tasks -> compact 1-line mode.
        for id in ["t1", "t2", "t3", "t4"] {
            reporter.on_task_start(id, id, &["ios-project".to_owned()]);
            reporter.on_task_output(id, "line one");
            reporter.on_task_output(id, "line two");
        }
        let compact_msg = reporter
            .lock_inner()
            .active
            .get("t1")
            .unwrap()
            .bar
            .as_ref()
            .unwrap()
            .message();
        assert_eq!(compact_msg, "  › line two");
        assert!(!compact_msg.contains('\n'));

        // Finish t4 -> 3 active tasks on a 30-row terminal -> expands to 3-line mini-console!
        let done = TaskReport {
            id: "t4".to_owned(),
            label: "t4".to_owned(),
            outcome: TaskOutcome::Passed,
            duration: Duration::from_millis(500),
            log_path: None,
        };
        reporter.on_task_finish(&done, Path::new("."));
        let expanded_msg = reporter
            .lock_inner()
            .active
            .get("t1")
            .unwrap()
            .bar
            .as_ref()
            .unwrap()
            .message();
        assert!(expanded_msg.contains("[ios-project]"));
        assert!(expanded_msg.contains("\n    │ line one"));
        assert!(expanded_msg.contains("\n    │ line two"));

        reporter.on_task_blocked("t5", "t5", "prerequisite failed");
        let guard = reporter.lock_inner();
        if let ReporterBackend::Interactive(state) = &guard.backend {
            assert_eq!(state.completed_tasks, 2);
            assert_eq!(state.passed_tasks, 1);
            assert_eq!(state.blocked_tasks, 1);
        }
    }
}

#[cfg(test)]
mod test_clean_line_term {
    use super::*;

    #[test]
    fn clean_line_term_replaces_right_margin_filler_with_eol_clear_and_explicit_newlines() {
        let term = CleanLineTerm::new(Term::buffered_stderr());
        let mut emitted = String::new();

        // Simulate `MultiProgress::println("  PASS  web-deps")` followed by two progress bar lines.
        for call in [
            "  PASS  web-deps",
            "          ",
            "[███] 1/2 tasks",
            "      ",
            "  ⠋ t2 (0.4s)",
            "        ",
        ] {
            term.advance_write_str(call, |chunk| {
                emitted.push_str(chunk);
                Ok(())
            })
            .unwrap();
        }
        term.flush().unwrap();

        assert_eq!(
            emitted,
            "  PASS  web-deps\x1b[0K\n[███] 1/2 tasks\x1b[0K\n  ⠋ t2 (0.4s)\x1b[0K"
        );
    }
}
