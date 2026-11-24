//! Terminal styling and progress reporting for task runs.
//!
//! The [`Reporter`] renders either the interactive progress view from [`interactive`]
//! or timestamped lines for the `ci` and `plain` modes,
//! and prints the end of run output from [`summary`].

mod interactive;
mod summary;

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use console::{Style, Term, truncate_str};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget};

use self::interactive::{
    CleanLineTerm, InteractiveState, build_overall_progress_style, build_task_spinner_style, compute_effective_tail,
};

use crate::cli::{EnvSnapshot, ResolvedOutputMode, RunOptions};
use crate::process::{read_log_tail, sanitize_log_line};
use crate::scheduler::{TaskOutcome, TaskReport};

const DEFAULT_EXPANDED_TAIL_LINES: usize = 3;
const MAX_EXPANDED_CONCURRENCY: usize = 3;
const MIN_EXPANDED_TERMINAL_ROWS: usize = 24;
const DEFAULT_TERMINAL_ROWS: usize = 24;
const DEFAULT_TERMINAL_COLS: usize = 100;
const MIN_RING_BUFFER_LINES: usize = 8;
const MAX_RING_BUFFER_LINES: usize = 64;
const MID_RUN_FAILURE_ALERT_LINES: usize = 8;
const PLAIN_FAILURE_RECAP_LINES: usize = 40;
const CI_FAILURE_RECAP_LINES: usize = 100;
const STEADY_TICK_MS: u64 = 100;
const INTERACTIVE_DRAW_HZ: u8 = 15;
const FAST_DURATION_SECS: f64 = 10.0;
const SLOW_DURATION_SECS: f64 = 30.0;

/// Resolved UI configuration driving terminal rendering, color styling, and failure hints.
#[derive(Clone, Debug)]
pub struct UiConfig {
    /// Concrete output mode (`Rich`, `Ci`, or `Plain`).
    pub mode: ResolvedOutputMode,
    /// Whether ANSI color escape codes should be emitted.
    pub colors_enabled: bool,
    /// Whether GitHub Actions workflow annotations (`::error`, `::group`) are active.
    pub github_actions: bool,
    /// Optional explicit override for console preview lines per active task.
    pub tail_lines: Option<usize>,
    /// Whether the run is a `--dryrun`.
    pub dry_run: bool,
    /// Command that re-runs one task with the same flags, without the task ID.
    pub rerun_prefix: String,
}

/// Precomputed `console::Style` instances honoring [`UiConfig::colors_enabled`].
#[derive(Clone, Debug)]
pub struct StylePalette {
    /// Bold green badge (`PASS`).
    pub pass: Style,
    /// Bold red badge and failure headers (`FAIL`).
    pub fail: Style,
    /// Bold yellow badge (`SKIP`).
    pub skip: Style,
    /// Bold magenta badge (`BLOCK`).
    pub block: Style,
    /// Bold cyan badge (`DRY`).
    pub dry: Style,
    /// Cyan status label (`START`, `PLAN`, `WAIT`).
    pub info: Style,
    /// Dimmed metadata, timestamps, fast durations, and live preview gutters.
    pub dim: Style,
    /// Yellow highlight for slow durations (`>= 30s`).
    pub slow: Style,
    /// Red gutter prefix (`│ `) for failure log excerpts.
    pub red_gutter: Style,
    /// Unstyled text for medium durations (`10s..30s`).
    pub plain: Style,
}

/// Fixed-capacity ring buffer storing the most recent sanitized output lines for one active task.
#[derive(Clone, Debug)]
pub struct TaskLineBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

/// Thread-safe progress, CI, and summary reporter shared across scheduler workers.
pub struct Reporter {
    config: UiConfig,
    palette: StylePalette,
    started_at: Instant,
    inner: Mutex<ReporterInner>,
}

struct ReporterInner {
    active: BTreeMap<String, ActiveTask>,
    backend: ReporterBackend,
}

#[derive(Debug)]
struct ActiveTask {
    locks: Vec<String>,
    started_at: Instant,
    buffer: TaskLineBuffer,
    bar: Option<ProgressBar>,
}

enum ReporterBackend {
    Interactive(Box<InteractiveState>),
    Line(LineTarget),
}

#[derive(Clone, Debug)]
enum LineTarget {
    Stdout,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
    #[cfg(test)]
    Silent,
}

impl UiConfig {
    /// Build a [`UiConfig`] from resolved run options and an environment snapshot.
    #[must_use]
    pub fn from_options(options: &RunOptions, env: &EnvSnapshot) -> Self {
        let resolved = options.resolve_ui(env);
        Self {
            mode: resolved.mode,
            colors_enabled: resolved.colors_enabled,
            github_actions: resolved.github_actions,
            tail_lines: options.tail_lines,
            dry_run: options.dry_run,
            rerun_prefix: options.rerun_prefix.clone(),
        }
    }

    /// Command that re-runs the given task with the same flags.
    #[must_use]
    pub fn rerun_command(&self, task_id: &str) -> String {
        format!("{} {}", self.rerun_prefix, crate::process::shell_quote(task_id))
    }

    /// Ring-buffer capacity per active task, accommodating both failure alerts and `--tail-lines`.
    #[must_use]
    pub fn ring_buffer_capacity(&self) -> usize {
        self.tail_lines
            .unwrap_or(DEFAULT_EXPANDED_TAIL_LINES)
            .clamp(MIN_RING_BUFFER_LINES, MAX_RING_BUFFER_LINES)
    }

    /// Maximum log tail lines printed per failed task in the end-of-run `FAILURES` recap.
    #[must_use]
    pub const fn failure_recap_lines(&self) -> usize {
        match self.mode {
            ResolvedOutputMode::Ci => CI_FAILURE_RECAP_LINES,
            ResolvedOutputMode::Rich | ResolvedOutputMode::Plain => PLAIN_FAILURE_RECAP_LINES,
        }
    }
}

impl StylePalette {
    /// Construct a palette with ANSI styling explicitly enabled or disabled on every style.
    #[must_use]
    pub const fn new(colors_enabled: bool) -> Self {
        Self {
            pass: Style::new().green().bold().force_styling(colors_enabled),
            fail: Style::new().red().bold().force_styling(colors_enabled),
            skip: Style::new().yellow().bold().force_styling(colors_enabled),
            block: Style::new().magenta().bold().force_styling(colors_enabled),
            dry: Style::new().cyan().bold().force_styling(colors_enabled),
            info: Style::new().cyan().force_styling(colors_enabled),
            dim: Style::new().dim().force_styling(colors_enabled),
            slow: Style::new().yellow().force_styling(colors_enabled),
            red_gutter: Style::new().red().force_styling(colors_enabled),
            plain: Style::new().force_styling(false),
        }
    }

    /// Style a `(1.2s)` parenthesized duration according to fast/medium/slow thresholds.
    #[must_use]
    pub fn style_paren_duration(&self, duration: Duration) -> String {
        let secs = duration.as_secs_f64();
        let text = format!("({secs:.1}s)");
        self.duration_style(secs).apply_to(text).to_string()
    }

    const fn duration_style(&self, secs: f64) -> &Style {
        if secs < FAST_DURATION_SECS {
            &self.dim
        } else if secs >= SLOW_DURATION_SECS {
            &self.slow
        } else {
            &self.plain
        }
    }
}

impl TaskLineBuffer {
    /// Create an empty ring buffer retaining up to `capacity` lines (minimum 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let bounded = capacity.max(1);
        Self {
            lines: VecDeque::with_capacity(bounded),
            capacity: bounded,
        }
    }

    /// Push a sanitized non-empty line, evicting the oldest line when full.
    pub fn push(&mut self, line: impl Into<String>) {
        let text = line.into();
        if text.trim().is_empty() {
            return;
        }
        if self.lines.len() >= self.capacity {
            let _ = self.lines.pop_front();
        }
        self.lines.push_back(text);
    }

    /// Return the most recently pushed line, if any.
    #[must_use]
    pub fn latest(&self) -> Option<&str> {
        self.lines.back().map(String::as_str)
    }

    /// Return up to the last `count` buffered lines in chronological order.
    #[must_use]
    pub fn tail(&self, count: usize) -> Vec<&str> {
        let skip = self.lines.len().saturating_sub(count);
        self.lines.iter().skip(skip).map(String::as_str).collect()
    }
}

impl std::fmt::Debug for Reporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporter")
            .field("config", &self.config)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

impl Reporter {
    /// Construct a [`Reporter`] matching `config` (`Rich` interactive UI or `Ci`/`Plain` stream).
    #[must_use]
    pub fn from_config(config: UiConfig) -> Self {
        if config.mode == ResolvedOutputMode::Rich && !config.dry_run {
            let term = Term::buffered_stderr();
            let target = if term.is_term() && !console::is_dumb() {
                ProgressDrawTarget::term_like_with_hz(Box::new(CleanLineTerm::new(term)), INTERACTIVE_DRAW_HZ)
            } else {
                ProgressDrawTarget::hidden()
            };
            Self::new_interactive(config, target, None)
        } else {
            Self::new_line(config)
        }
    }

    /// Construct a line-oriented reporter writing to standard output (`plain` or `ci` mode).
    #[must_use]
    pub fn new_line(config: UiConfig) -> Self {
        let palette = StylePalette::new(config.colors_enabled);
        Self {
            config,
            palette,
            started_at: Instant::now(),
            inner: Mutex::new(ReporterInner {
                active: BTreeMap::new(),
                backend: ReporterBackend::Line(LineTarget::Stdout),
            }),
        }
    }

    /// Construct a silent reporter that discards all output (for scheduler unit tests).
    #[cfg(test)]
    #[must_use]
    pub fn silent() -> Self {
        let config = UiConfig {
            mode: ResolvedOutputMode::Plain,
            colors_enabled: false,
            github_actions: false,
            tail_lines: None,
            dry_run: false,
            rerun_prefix: "xtask".to_owned(),
        };
        Self {
            palette: StylePalette::new(false),
            config,
            started_at: Instant::now(),
            inner: Mutex::new(ReporterInner {
                active: BTreeMap::new(),
                backend: ReporterBackend::Line(LineTarget::Silent),
            }),
        }
    }

    /// Construct a buffer-backed line reporter for deterministic unit tests.
    #[cfg(test)]
    #[must_use]
    pub fn line_to_buffer(config: UiConfig) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let palette = StylePalette::new(config.colors_enabled);
        let reporter = Self {
            config,
            palette,
            started_at: Instant::now(),
            inner: Mutex::new(ReporterInner {
                active: BTreeMap::new(),
                backend: ReporterBackend::Line(LineTarget::Buffer(Arc::clone(&sink))),
            }),
        };
        (reporter, sink)
    }

    /// Construct an interactive reporter backed by a hidden draw target for non-TTY unit tests.
    #[cfg(test)]
    #[must_use]
    pub fn interactive_hidden(config: UiConfig, geometry: (usize, usize)) -> Self {
        Self::new_interactive(config, ProgressDrawTarget::hidden(), Some(geometry))
    }

    fn new_interactive(
        config: UiConfig,
        draw_target: ProgressDrawTarget,
        fixed_geometry: Option<(usize, usize)>,
    ) -> Self {
        let palette = StylePalette::new(config.colors_enabled);
        let multi = MultiProgress::with_draw_target(draw_target);
        let overall_style = build_overall_progress_style(config.colors_enabled);
        let task_style = build_task_spinner_style(config.colors_enabled);
        let overall_bar = multi.add(ProgressBar::new(0));
        overall_bar.set_style(overall_style);
        overall_bar.enable_steady_tick(Duration::from_millis(STEADY_TICK_MS));

        Self {
            config,
            palette,
            started_at: Instant::now(),
            inner: Mutex::new(ReporterInner {
                active: BTreeMap::new(),
                backend: ReporterBackend::Interactive(Box::new(InteractiveState {
                    multi,
                    overall_bar,
                    task_style,
                    fixed_geometry,
                    total_tasks: 0,
                    completed_tasks: 0,
                    passed_tasks: 0,
                    failed_tasks: 0,
                    skipped_tasks: 0,
                    blocked_tasks: 0,
                })),
            }),
        }
    }

    /// Return whether periodic `WAIT` heartbeats should be emitted.
    #[must_use]
    pub const fn is_ci_heartbeat_enabled(&self) -> bool {
        matches!(self.config.mode, ResolvedOutputMode::Ci)
    }

    /// Announce the resolved execution plan before workers begin.
    pub fn on_plan(&self, total_tasks: usize, jobs: usize, scope: &str) {
        let mut guard = self.lock_inner();
        let active_len = guard.active.len();
        match &mut guard.backend {
            ReporterBackend::Interactive(interactive) => {
                interactive.total_tasks = total_tasks;
                interactive.overall_bar.set_length(total_tasks as u64);
                interactive.overall_bar.set_position(0);
                let msg = interactive.status_message(&self.palette, active_len);
                interactive.overall_bar.set_message(msg);
            }
            ReporterBackend::Line(target) => {
                let prefix = self.line_prefix(&self.palette.info, "PLAN ");
                target.write_line(&format!("{prefix} {total_tasks} task(s) (jobs={jobs}, scope={scope})"));
            }
        }
    }

    /// Record that `id` has started executing.
    pub fn on_task_start(&self, id: &str, label: &str, locks: &[String]) {
        let prefix = self.line_prefix(&self.palette.info, "START");
        let start_line = format!("{prefix} {id:<28} {label}");

        let mut guard = self.lock_inner();
        let bar = match &guard.backend {
            ReporterBackend::Interactive(interactive) => {
                let pb = interactive.multi.add(ProgressBar::new_spinner());
                pb.set_style(interactive.task_style.clone());
                pb.set_prefix(id.to_owned());
                pb.enable_steady_tick(Duration::from_millis(STEADY_TICK_MS));
                Some(pb)
            }
            ReporterBackend::Line(_) => None,
        };

        guard.active.insert(
            id.to_owned(),
            ActiveTask {
                locks: locks.to_vec(),
                started_at: Instant::now(),
                buffer: TaskLineBuffer::new(self.config.ring_buffer_capacity()),
                bar,
            },
        );
        self.refresh_or_emit_start(&mut guard, &start_line);
        drop(guard);
    }

    /// Append a sanitized live output line to `id`'s ring buffer and refresh interactive UI.
    pub fn on_task_output(&self, id: &str, line: &str) {
        let mut guard = self.lock_inner();
        let Some(task) = guard.active.get_mut(id) else {
            return;
        };
        task.buffer.push(line);
        self.refresh_single_interactive_task(&guard, id);
        drop(guard);
    }

    /// Emit a `--verbose` command trace safely without corrupting terminal state.
    pub fn on_task_command(&self, id: &str, display_cmd: &str) {
        let mut guard = self.lock_inner();
        match &mut guard.backend {
            ReporterBackend::Interactive(interactive) => {
                let badge = self.palette.dim.apply_to("CMD  ");
                let cmd_text = self.palette.dim.apply_to(format!("$ {display_cmd}"));
                let _ = interactive.multi.println(format!("  {badge} {id:<28} {cmd_text}"));
            }
            ReporterBackend::Line(target) => {
                let prefix = self.line_prefix(&self.palette.dim, "CMD  ");
                let cmd_text = self.palette.dim.apply_to(format!("$ {display_cmd}"));
                target.write_line(&format!("{prefix} {id:<28} {cmd_text}"));
            }
        }
    }

    /// Record task completion, emitting permanent status and any immediate mid-run failure alert.
    pub fn on_task_finish(&self, report: &TaskReport, repo_root: &Path) {
        let mut guard = self.lock_inner();
        let finished_task = guard.active.remove(&report.id);
        let (buffered_tail, finished_bar) = match finished_task {
            Some(task) => {
                let tail = task
                    .buffer
                    .tail(MID_RUN_FAILURE_ALERT_LINES)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                (tail, task.bar)
            }
            None => (Vec::new(), None),
        };

        let excerpt = if report.outcome.is_failure() {
            self.format_mid_run_failure_excerpt(&buffered_tail, report.log_path.as_deref(), repo_root)
        } else {
            Vec::new()
        };
        let duration_text = self.palette.style_paren_duration(report.duration);

        let is_interactive = matches!(guard.backend, ReporterBackend::Interactive(_));
        if is_interactive {
            let mut permanent_lines = Vec::new();
            match &report.outcome {
                TaskOutcome::Passed => {
                    let badge = self.palette.pass.apply_to("PASS ");
                    permanent_lines.push(format!("  {badge} {:<28} {duration_text}", report.id));
                }
                TaskOutcome::Skipped(reason) => {
                    let badge = self.palette.skip.apply_to("SKIP ");
                    permanent_lines.push(format!("  {badge} {:<28} {duration_text}  {reason}", report.id));
                }
                TaskOutcome::Failed(err) => {
                    let badge = self.palette.fail.apply_to("FAIL ");
                    permanent_lines.push(format!("  {badge} {:<28} {duration_text}  {err}", report.id));
                    permanent_lines.extend(excerpt);
                }
                TaskOutcome::Blocked(_) | TaskOutcome::DryRun => {}
            }

            if let ReporterBackend::Interactive(interactive) = &mut guard.backend {
                if let Some(pb) = finished_bar {
                    pb.finish_and_clear();
                    interactive.multi.remove(&pb);
                }
                interactive.record_outcome(&report.outcome);
                let (_, cols) = interactive.terminal_geometry();
                for line in &permanent_lines {
                    let _ = interactive.multi.println(truncate_to_terminal_width(line, cols));
                }
            }
            self.refresh_all_interactive(&guard);
            return;
        }

        let mut lines = Vec::new();
        match &report.outcome {
            TaskOutcome::Passed => {
                let prefix = self.line_prefix(&self.palette.pass, "PASS ");
                lines.push(format!("{prefix} {:<28} {duration_text}", report.id));
            }
            TaskOutcome::Skipped(reason) => {
                let prefix = self.line_prefix(&self.palette.skip, "SKIP ");
                lines.push(format!("{prefix} {:<28} {duration_text} - {reason}", report.id));
            }
            TaskOutcome::Failed(err) => {
                if self.config.github_actions {
                    lines.push(format!(
                        "::error title={}::{}",
                        escape_gha_property(&report.id),
                        escape_gha_data(err)
                    ));
                }
                let prefix = self.line_prefix(&self.palette.fail, "FAIL ");
                lines.push(format!("{prefix} {:<28} {duration_text} - {err}", report.id));
                lines.extend(excerpt);
            }
            TaskOutcome::Blocked(_) | TaskOutcome::DryRun => {}
        }
        guard.backend.emit_lines(&lines);
    }

    /// Record that `id` was blocked by a failed prerequisite or `--fail-fast`.
    pub fn on_task_blocked(&self, id: &str, _label: &str, reason: &str) {
        let mut guard = self.lock_inner();
        let active_len = guard.active.len();
        match &mut guard.backend {
            ReporterBackend::Interactive(interactive) => {
                interactive.completed_tasks = interactive.completed_tasks.saturating_add(1);
                interactive.blocked_tasks = interactive.blocked_tasks.saturating_add(1);
                interactive.overall_bar.set_position(interactive.completed_tasks as u64);
                let msg = interactive.status_message(&self.palette, active_len);
                interactive.overall_bar.set_message(msg);
                let (_, cols) = interactive.terminal_geometry();
                let badge = self.palette.block.apply_to("BLOCK");
                let line = format!("  {badge} {id:<28} {reason}");
                let _ = interactive.multi.println(truncate_to_terminal_width(&line, cols));
            }
            ReporterBackend::Line(target) => {
                let prefix = self.line_prefix(&self.palette.block, "BLOCK");
                target.write_line(&format!("{prefix} {id:<28} - {reason}"));
            }
        }
    }

    /// Emit a `--dryrun` command preview line for `id`.
    pub fn on_dry_run(&self, id: &str, summary: &str) {
        let mut guard = self.lock_inner();
        match &mut guard.backend {
            ReporterBackend::Interactive(interactive) => {
                let badge = self.palette.dry.apply_to("DRY  ");
                let _ = interactive.multi.println(format!("  {badge} {id:<28} {summary}"));
            }
            ReporterBackend::Line(target) => {
                let prefix = self.line_prefix(&self.palette.dry, "DRY  ");
                target.write_line(&format!("{prefix} {id:<28} {summary}"));
            }
        }
    }

    /// Emit a `WAIT` heartbeat listing currently running tasks and their elapsed times in `ci` mode.
    pub fn emit_heartbeat(&self) {
        if !self.is_ci_heartbeat_enabled() {
            return;
        }
        let mut guard = self.lock_inner();
        if guard.active.is_empty() {
            return;
        }
        let running_list: Vec<String> = guard
            .active
            .iter()
            .map(|(id, task)| format!("{id} ({:.1}s)", task.started_at.elapsed().as_secs_f64()))
            .collect();
        let prefix = self.line_prefix(&self.palette.info, "WAIT ");
        let line = format!(
            "{prefix} still running ({}): {}",
            running_list.len(),
            running_list.join(", ")
        );
        guard.backend.emit_line(&line);
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, ReporterInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn refresh_or_emit_start(&self, guard: &mut ReporterInner, start_line: &str) {
        if matches!(guard.backend, ReporterBackend::Interactive(_)) {
            self.refresh_all_interactive(guard);
        } else {
            guard.backend.emit_line(start_line);
        }
    }

    fn refresh_all_interactive(&self, guard: &ReporterInner) {
        let ReporterBackend::Interactive(interactive) = &guard.backend else {
            return;
        };
        let active_len = guard.active.len();
        let status_msg = interactive.status_message(&self.palette, active_len);
        interactive.overall_bar.set_message(status_msg);

        let (rows, cols) = interactive.terminal_geometry();
        let effective_tail = compute_effective_tail(active_len, rows, self.config.tail_lines);
        for (id, task) in &guard.active {
            if let Some(pb) = &task.bar {
                let msg = self.format_task_slot_message(id, task, effective_tail, cols);
                pb.set_message(msg);
            }
        }
    }

    fn refresh_single_interactive_task(&self, guard: &ReporterInner, id: &str) {
        let ReporterBackend::Interactive(interactive) = &guard.backend else {
            return;
        };
        let Some(task) = guard.active.get(id) else {
            return;
        };
        let Some(pb) = &task.bar else {
            return;
        };
        let (rows, cols) = interactive.terminal_geometry();
        let effective_tail = compute_effective_tail(guard.active.len(), rows, self.config.tail_lines);
        let msg = self.format_task_slot_message(id, task, effective_tail, cols);
        pb.set_message(msg);
    }

    fn format_task_slot_message(&self, id: &str, task: &ActiveTask, effective_tail: usize, cols: usize) -> String {
        // Header prefix is `"  ⠋ <id> (99.9s)"` (~14 columns plus id width).
        let header_used = id.len().saturating_add(16);
        let header_budget = cols.saturating_sub(header_used).saturating_sub(1);

        if effective_tail == 0 {
            if task.locks.is_empty() || header_budget < 4 {
                return String::new();
            }
            let badge = format!("[{}]", task.locks.join(", "));
            let truncated = truncate_str(&badge, header_budget, "…");
            return format!(" {}", self.palette.dim.apply_to(truncated));
        }

        if effective_tail == 1 {
            let Some(latest) = task.buffer.latest() else {
                return String::new();
            };
            if header_budget < 4 {
                return String::new();
            }
            let preview = format!("› {latest}");
            let truncated = truncate_str(&preview, header_budget, "…");
            return format!("  {}", self.palette.dim.apply_to(truncated));
        }

        let mut rendered = String::new();
        if !task.locks.is_empty() && header_budget >= 4 {
            let badge = format!("[{}]", task.locks.join(", "));
            let truncated = truncate_str(&badge, header_budget, "…");
            let _ = write!(rendered, " {}", self.palette.dim.apply_to(truncated));
        }

        let gutter = self.palette.dim.apply_to("│");
        let line_budget = cols.saturating_sub(7);
        if line_budget > 0 {
            for line in task.buffer.tail(effective_tail) {
                let truncated = truncate_str(line, line_budget, "…");
                let styled = self.palette.dim.apply_to(truncated);
                let _ = write!(rendered, "\n    {gutter} {styled}");
            }
        }
        rendered
    }

    fn line_prefix(&self, badge_style: &Style, badge: &str) -> String {
        let ts = self
            .palette
            .dim
            .apply_to(format_wall_timestamp(self.started_at.elapsed()));
        let styled_badge = badge_style.apply_to(badge);
        format!("{ts} {styled_badge}")
    }

    fn format_mid_run_failure_excerpt(
        &self,
        buffered_tail: &[String],
        log_path: Option<&Path>,
        repo_root: &Path,
    ) -> Vec<String> {
        let lines: Vec<String> = if buffered_tail.is_empty() {
            log_path
                .filter(|path| path.is_file())
                .map_or_default(|path| collect_failure_tail(path, MID_RUN_FAILURE_ALERT_LINES))
        } else {
            buffered_tail.to_vec()
        };

        let mut out = Vec::new();
        let gutter = self.palette.red_gutter.apply_to("│");
        for line in lines {
            out.push(format!("        {gutter} {line}"));
        }
        if let Some(path) = log_path.filter(|path| path.is_file()) {
            let rel = display_relative_path(path, repo_root);
            let branch = self.palette.red_gutter.apply_to("└─");
            let label = self.palette.dim.apply_to(format!("log: {rel}"));
            out.push(format!("        {branch} {label}"));
        }
        out
    }
}

impl ReporterBackend {
    fn emit_line(&mut self, line: &str) {
        match self {
            Self::Interactive(interactive) => {
                let _ = interactive.multi.println(line);
            }
            Self::Line(target) => target.write_line(line),
        }
    }

    fn emit_lines(&mut self, lines: &[String]) {
        match self {
            Self::Interactive(interactive) => {
                for line in lines {
                    let _ = interactive.multi.println(line);
                }
            }
            Self::Line(target) => {
                for line in lines {
                    target.write_line(line);
                }
            }
        }
    }
}

impl LineTarget {
    fn write_line(&self, line: &str) {
        match self {
            Self::Stdout => {
                let _ = writeln!(io::stdout(), "{line}");
            }
            #[cfg(test)]
            Self::Buffer(buffer) => {
                if let Ok(mut guard) = buffer.lock() {
                    let _ = writeln!(guard, "{line}");
                }
            }
            #[cfg(test)]
            Self::Silent => {}
        }
    }
}

/// Truncate `text` to `cols.saturating_sub(1)` visible columns using `"…"` without soft-wrapping.
#[must_use]
pub fn truncate_to_terminal_width(text: &str, cols: usize) -> String {
    let max_width = cols.saturating_sub(1);
    if max_width == 0 {
        return String::new();
    }
    truncate_str(text, max_width, "…").into_owned()
}

/// Format an elapsed duration as `[MM:SS.d]` for `ci` and `plain` log prefixes.
#[must_use]
pub fn format_wall_timestamp(elapsed: Duration) -> String {
    let total_tenths = elapsed.as_millis() / 100;
    let tenths = total_tenths % 10;
    let total_secs = total_tenths / 10;
    let secs = total_secs % 60;
    let mins = total_secs / 60;
    format!("[{mins:02}:{secs:02}.{tenths}]")
}

/// Escape message data for GitHub Actions workflow commands (`::error ...::<data>`).
#[must_use]
pub fn escape_gha_data(value: &str) -> String {
    value.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// Escape property values for GitHub Actions workflow commands (`title=<prop>`).
#[must_use]
pub fn escape_gha_property(value: &str) -> String {
    escape_gha_data(value).replace(':', "%3A").replace(',', "%2C")
}

fn display_relative_path(path: &Path, repo_root: &Path) -> String {
    path.strip_prefix(repo_root).unwrap_or(path).display().to_string()
}

fn collect_failure_tail(log_path: &Path, max_lines: usize) -> Vec<String> {
    let raw = read_log_tail(log_path, max_lines.saturating_mul(2));
    let cleaned: Vec<String> = raw
        .into_iter()
        .map(|line| sanitize_log_line(&line))
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = cleaned.len().saturating_sub(max_lines);
    cleaned.get(start..).unwrap_or_default().to_vec()
}

#[cfg(test)]
mod test_ui_config {
    use super::*;

    #[test]
    fn ui_config_scales_ring_buffer_capacity_with_tail_lines() {
        let default_cfg = UiConfig::from_options(&RunOptions::default(), &EnvSnapshot::default());
        assert_eq!(default_cfg.mode, ResolvedOutputMode::Plain);
        assert!(!default_cfg.colors_enabled);
        assert!(!default_cfg.github_actions);
        assert_eq!(default_cfg.ring_buffer_capacity(), 8);
        assert_eq!(default_cfg.failure_recap_lines(), 40);

        let tall_opts = RunOptions {
            tail_lines: Some(12),
            ..RunOptions::default()
        };
        let gha_env = EnvSnapshot {
            ci: Some("true".to_owned()),
            github_actions: Some("true".to_owned()),
            ..EnvSnapshot::default()
        };
        let tall_cfg = UiConfig::from_options(&tall_opts, &gha_env);
        assert_eq!(tall_cfg.ring_buffer_capacity(), 12);
        assert_eq!(tall_cfg.failure_recap_lines(), 100);
    }
}

#[cfg(test)]
mod test_task_line_buffer {
    use super::*;

    #[test]
    fn task_line_buffer_retains_latest_non_empty_lines() {
        let mut buf = TaskLineBuffer::new(3);
        buf.push("line 1");
        buf.push("   ");
        buf.push("line 2");
        buf.push("line 3");
        buf.push("line 4");

        assert_eq!(buf.latest(), Some("line 4"));
        assert_eq!(buf.tail(2), vec!["line 3", "line 4"]);
        assert_eq!(buf.tail(10), vec!["line 2", "line 3", "line 4"]);
    }
}

#[cfg(test)]
mod test_style_palette {
    use super::*;

    #[test]
    fn style_palette_forces_or_disables_ansi_independent_of_tty() {
        let colored = StylePalette::new(true);
        let plain = StylePalette::new(false);

        for style in [
            &colored.pass,
            &colored.fail,
            &colored.skip,
            &colored.block,
            &colored.dry,
            &colored.info,
            &colored.dim,
            &colored.slow,
            &colored.red_gutter,
        ] {
            assert!(style.apply_to("TAG").to_string().contains("\x1b["));
        }
        assert_eq!(plain.pass.apply_to("PASS").to_string(), "PASS");
        assert_eq!(plain.fail.apply_to("FAIL").to_string(), "FAIL");

        assert!(
            colored
                .style_paren_duration(Duration::from_millis(400))
                .contains("\x1b[")
        );
        assert_eq!(colored.style_paren_duration(Duration::from_secs(15)), "(15.0s)");
    }
}

#[cfg(test)]
mod test_formatting_helpers {
    use super::*;

    #[test]
    fn formats_timestamps_truncation_and_github_actions_escapes() {
        assert_eq!(format_wall_timestamp(Duration::from_millis(0)), "[00:00.0]");
        assert_eq!(format_wall_timestamp(Duration::from_millis(74_290)), "[01:14.2]");

        let truncated = truncate_to_terminal_width("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(console::measure_text_width(&truncated), 9);
        assert!(truncated.ends_with('…'));

        assert_eq!(escape_gha_data("100%\r\nboom"), "100%25%0D%0Aboom");
        assert_eq!(escape_gha_property("task:a,b"), "task%3Aa%2Cb");
    }
}

#[cfg(test)]
mod test_reporter_constructors {
    use super::*;

    #[test]
    fn silent_and_from_config_constructors_work_without_tty() {
        let silent = Reporter::silent();
        silent.on_plan(1, 1, "all");
        silent.on_dry_run("cargo-fmt", "cargo fmt --all --check");

        let plain_cfg = UiConfig::from_options(&RunOptions::default(), &EnvSnapshot::default());
        let reporter = Reporter::from_config(plain_cfg);
        assert!(!reporter.is_ci_heartbeat_enabled());
    }
}
