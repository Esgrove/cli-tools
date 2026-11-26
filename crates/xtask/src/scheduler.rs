//! Parallel task scheduler over the dependency graph with exclusive resource locks.
//!
//! Also defines the [`TaskContext`] passed to every task action and the state tasks share during a run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::process::{CommandSpec, OutputSink, append_log_line, run_logged};
use crate::ui::Reporter;

const CI_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Named shared resource lock that cannot be held concurrently by two tasks.
///
/// Resource names are free-form strings defined in the task file
/// (for example, `"cargo-target"` or `"android-emulator"`).
pub type Resource = String;

/// Action closure executed by a task.
pub type TaskAction = Box<dyn FnOnce(&TaskContext, &Path) -> Result<TaskActionResult> + Send>;

/// Result returned by an individual task action callback.
#[derive(Debug)]
pub enum TaskActionResult {
    /// Task completed successfully.
    Passed,
    /// Task was intentionally skipped (for example, an optional tool was not installed).
    Skipped(String),
    /// Task ran in `--dryrun` mode.
    DryRun,
}

/// Final recorded outcome of a task in a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskOutcome {
    /// Task succeeded.
    Passed,
    /// Task failed with an error message.
    Failed(String),
    /// Task was not started because a prerequisite failed or `--fail-fast` stopped scheduling.
    Blocked(String),
    /// Task was skipped with a reason.
    Skipped(String),
    /// Task ran in `--dryrun` mode.
    DryRun,
}

/// Runtime values discovered by setup tasks and shared with downstream tasks.
#[derive(Debug, Default)]
pub struct SharedState {
    /// Variables available for `${NAME}` expansion, each holding one or more values.
    pub variables: BTreeMap<String, Vec<String>>,
    /// Summary notes (such as coverage report paths) printed at the end of the run.
    pub notes: Vec<String>,
}

/// Context passed to every task action.
#[derive(Clone, Debug)]
pub struct TaskContext {
    /// Absolute path to the project root, the directory containing the task file.
    pub repo_root: PathBuf,
    /// Directory where per-task log files are written.
    pub log_dir: PathBuf,
    /// Whether `--dryrun` is enabled.
    pub dry_run: bool,
    /// Whether `--verbose` is enabled.
    pub verbose: bool,
    /// Whether `--fail-fast` is enabled.
    pub fail_fast: bool,
    /// Every project flag after applying `implies`, keyed by long name.
    pub flags: Arc<BTreeMap<String, bool>>,
    /// Active task ID when running inside a worker slot.
    pub task_id: Option<String>,
    /// Shared progress and CI reporter.
    pub reporter: Arc<Reporter>,
    /// Thread-safe shared state populated by prerequisite tasks.
    pub shared: Arc<Mutex<SharedState>>,
}

/// Static specification of one node in the test DAG.
pub struct TaskSpec {
    /// Stable unique identifier (used on the command line, in logs, and in dependency references).
    pub id: String,
    /// Human-readable description shown in status output.
    pub label: String,
    /// Lower values start first when resources or job slots are contended (matching `prek`).
    pub priority: u32,
    /// IDs of prerequisite tasks that must pass before this task starts.
    pub deps: Vec<String>,
    /// IDs of tasks that must finish first when they run too, whatever their outcome.
    ///
    /// Unlike `deps`, these never add a task to the run and never block this one when they fail.
    pub after: Vec<String>,
    /// Shared resources exclusively locked while this task runs.
    pub resources: Vec<Resource>,
    /// Scope tags this task belongs to.
    pub scopes: Vec<String>,
    /// Concise command summary printed in `--dryrun` and `list`.
    pub dry_run_summary: String,
    /// Action executed when the task is scheduled.
    pub action: TaskAction,
}

/// Execution report for one task.
#[derive(Clone, Debug)]
pub struct TaskReport {
    /// Task identifier.
    pub id: String,
    /// Human-readable label.
    pub label: String,
    /// Final outcome.
    pub outcome: TaskOutcome,
    /// Elapsed wall-clock time for this task.
    pub duration: Duration,
    /// Path to the task's log file (when executed).
    pub log_path: Option<PathBuf>,
}

/// Aggregate result of a full scheduler run.
#[derive(Debug)]
pub struct RunSummary {
    /// Reports for every task in deterministic order.
    pub reports: Vec<TaskReport>,
    /// Total wall-clock time for the run.
    pub wall_duration: Duration,
    /// Directory containing per-task log files.
    pub log_dir: PathBuf,
    /// Extra notes collected during the run (for example, coverage paths).
    pub notes: Vec<String>,
}

struct SchedulerState {
    pending: BTreeMap<String, TaskSpec>,
    running: BTreeSet<String>,
    completed: BTreeMap<String, TaskReport>,
    held_resources: BTreeSet<Resource>,
    any_failed: bool,
    stop_scheduling: bool,
}

impl TaskContext {
    /// Return a task-scoped clone bound to `task_id` for live output streaming.
    #[must_use]
    pub fn with_task_id(&self, task_id: impl Into<String>) -> Self {
        let mut scoped = self.clone();
        scoped.task_id = Some(task_id.into());
        scoped
    }

    /// Build a live line-streaming callback routing subprocess output to [`Reporter::on_task_output`].
    #[must_use]
    pub fn task_sink(&self) -> Option<OutputSink> {
        let id = self.task_id.clone()?;
        let reporter = Arc::clone(&self.reporter);
        Some(Arc::new(move |line: &str| {
            reporter.on_task_output(&id, line);
        }))
    }

    /// Execute `spec`, logging to `log_path` and streaming sanitized lines to the active task slot.
    ///
    /// # Errors
    ///
    /// Returns an error if the process fails to spawn, cannot write logs, or exits non-zero.
    pub fn run_logged(&self, spec: &CommandSpec, log_path: &Path) -> Result<()> {
        if self.verbose {
            let display = spec.display_line();
            let id = self.task_id.as_deref().unwrap_or("task");
            self.reporter.on_task_command(id, &display);
        }
        let sink = self.task_sink();
        run_logged(spec, log_path, sink.as_ref())
    }

    /// Whether the named project flag is on.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.get(name).copied().unwrap_or(false)
    }

    /// Values of a shared variable set by an earlier task, a builtin, or a variable provider.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<Vec<String>> {
        self.lock_shared().variables.get(name).cloned()
    }

    /// Set a shared variable for `${NAME}` expansion in later tasks, replacing any earlier values.
    pub fn set_variable(&self, name: impl Into<String>, values: Vec<String>) {
        self.lock_shared().variables.insert(name.into(), values);
    }

    /// Add a line to the notes printed at the end of the run.
    pub fn add_note(&self, note: impl Into<String>) {
        self.lock_shared().notes.push(note.into());
    }

    /// Append a status note to `log_path` and push it to the task's live preview.
    ///
    /// # Errors
    ///
    /// Returns an error if `log_path` cannot be opened or written.
    pub fn log_note(&self, log_path: &Path, message: &str) -> Result<()> {
        append_log_line(log_path, message)?;
        if let Some(id) = &self.task_id {
            self.reporter.on_task_output(id, message);
        }
        Ok(())
    }

    fn lock_shared(&self) -> std::sync::MutexGuard<'_, SharedState> {
        self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RunSummary {
    /// Return `true` if no task failed.
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self.reports.iter().any(|r| r.outcome.is_failure())
    }
}

impl TaskOutcome {
    /// Return whether this outcome counts as a failure for the overall exit status.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// Return whether dependents are allowed to proceed after this outcome.
    #[must_use]
    pub const fn unblocks_dependents(&self) -> bool {
        matches!(self, Self::Passed | Self::Skipped(_) | Self::DryRun)
    }
}

/// Validate that a task list has unique IDs, valid dependency references, and no cycles.
///
/// # Errors
///
/// Returns an error if duplicate IDs, missing dependencies, or dependency cycles exist.
pub fn validate_graph(tasks: &[TaskSpec]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for task in tasks {
        if !ids.insert(task.id.as_str()) {
            bail!("Duplicate task ID '{}'.", task.id);
        }
    }

    let mut in_degree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for task in tasks {
        for dep in &task.deps {
            if !ids.contains(dep.as_str()) {
                bail!("Task '{}' depends on unknown task '{dep}'.", task.id);
            }
        }
        // An ordering only applies to tasks in the run, so one naming a task outside it is left out.
        // Deduplicate predecessors so a repeated entry in `deps` or `after` is counted once.
        let predecessors: BTreeSet<&str> = task
            .deps
            .iter()
            .chain(task.after.iter().filter(|id| ids.contains(id.as_str())))
            .map(String::as_str)
            .collect();
        in_degree.insert(task.id.as_str(), predecessors.len());
        for predecessor in predecessors {
            dependents.entry(predecessor).or_default().push(task.id.as_str());
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter_map(|(&id, &deg)| (deg == 0).then_some(id))
        .collect();
    let mut visited = 0_usize;

    while let Some(node) = queue.pop_front() {
        visited = visited.saturating_add(1);
        if let Some(children) = dependents.get(node) {
            for &child in children {
                if let Some(deg) = in_degree.get_mut(child) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        queue.push_back(child);
                    }
                }
            }
        }
    }

    if visited != tasks.len() {
        bail!("Task dependency graph contains a cycle.");
    }
    Ok(())
}

/// Filter `tasks` by scope and optional explicit task IDs (including transitive prerequisites).
///
/// `scopes` holds the selected scope and every scope it includes, or `None` to keep every task.
///
/// # Errors
///
/// Returns an error if `only` references a task ID that does not exist in the resolved graph.
pub fn filter_tasks(tasks: Vec<TaskSpec>, scopes: Option<&BTreeSet<String>>, only: &[String]) -> Result<Vec<TaskSpec>> {
    let by_id: BTreeMap<String, Vec<String>> = tasks.iter().map(|task| (task.id.clone(), task.deps.clone())).collect();

    let mut keep = BTreeSet::new();
    if only.is_empty() {
        for task in &tasks {
            if scopes.is_none_or(|selected| task.scopes.iter().any(|scope| selected.contains(scope))) {
                collect_with_deps(&task.id, &by_id, &mut keep);
            }
        }
    } else {
        for id in only {
            if !by_id.contains_key(id) {
                let available: Vec<&str> = by_id.keys().map(String::as_str).collect();
                bail!(
                    "Unknown task ID '{id}'. Available tasks for the current flags: {}",
                    available.join(", ")
                );
            }
            collect_with_deps(id, &by_id, &mut keep);
        }
    }

    Ok(tasks.into_iter().filter(|task| keep.contains(&task.id)).collect())
}

/// Print the resolved task graph without running it.
pub fn print_task_list(tasks: &[TaskSpec]) {
    let _ = writeln!(io::stdout(), "Resolved tasks ({}):", tasks.len());
    for task in tasks {
        let deps = if task.deps.is_empty() {
            "none".to_owned()
        } else {
            task.deps.join(", ")
        };
        let resources = if task.resources.is_empty() {
            "none".to_owned()
        } else {
            task.resources
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            io::stdout(),
            "  {:<28} prio={:<3} deps=[{deps}] locks=[{resources}]",
            task.id,
            task.priority
        );
    }
}

/// Execute `tasks` concurrently while respecting DAG dependencies, resource locks, and `max_jobs`.
///
/// # Errors
///
/// Returns an error if the task graph is invalid or the log directory cannot be created.
pub fn run_schedule(
    tasks: Vec<TaskSpec>,
    ctx: &TaskContext,
    max_jobs: usize,
    fail_fast: bool,
    scope_label: &str,
) -> Result<RunSummary> {
    validate_graph(&tasks)?;
    if !ctx.dry_run {
        std::fs::create_dir_all(&ctx.log_dir)?;
    }

    let max_workers = max_jobs.max(1);
    let wall_start = Instant::now();
    ctx.reporter.on_plan(tasks.len(), max_workers, scope_label);

    let scheduled: BTreeSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
    let mut order = Vec::with_capacity(tasks.len());
    let mut pending = BTreeMap::new();
    for mut task in tasks {
        task.after.retain(|id| scheduled.contains(id));
        order.push(task.id.clone());
        pending.insert(task.id.clone(), task);
    }

    let state = Arc::new((
        Mutex::new(SchedulerState {
            pending,
            running: BTreeSet::new(),
            completed: BTreeMap::new(),
            held_resources: BTreeSet::new(),
            any_failed: false,
            stop_scheduling: false,
        }),
        Condvar::new(),
    ));

    std::thread::scope(|thread_scope| {
        let heartbeat_stop = Arc::new((Mutex::new(false), Condvar::new()));
        if ctx.reporter.is_ci_heartbeat_enabled() && !ctx.dry_run {
            let stop_pair = Arc::clone(&heartbeat_stop);
            let reporter = Arc::clone(&ctx.reporter);
            thread_scope.spawn(move || {
                heartbeat_loop(&stop_pair, &reporter);
            });
        }

        let mut worker_handles = Vec::with_capacity(max_workers);
        for _ in 0..max_workers {
            let state = Arc::clone(&state);
            let ctx = ctx.clone();

            worker_handles.push(thread_scope.spawn(move || {
                worker_loop(&state, &ctx, fail_fast);
            }));
        }

        for handle in worker_handles {
            let _ = handle.join();
        }

        let (stop_lock, stop_cvar) = &*heartbeat_stop;
        let mut stopped = stop_lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *stopped = true;
        drop(stopped);
        stop_cvar.notify_all();
    });

    let wall_duration = wall_start.elapsed();
    let reports = {
        let mut guard = state.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        order.into_iter().filter_map(|id| guard.completed.remove(&id)).collect()
    };
    let notes = ctx
        .shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .notes
        .clone();

    Ok(RunSummary {
        reports,
        wall_duration,
        log_dir: ctx.log_dir.clone(),
        notes,
    })
}

fn heartbeat_loop(stop_pair: &(Mutex<bool>, Condvar), reporter: &Reporter) {
    let (lock, cvar) = stop_pair;
    let mut guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    while !*guard {
        let (next_guard, timeout) = cvar
            .wait_timeout(guard, CI_HEARTBEAT_INTERVAL)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard = next_guard;
        if !*guard && timeout.timed_out() {
            reporter.emit_heartbeat();
        }
    }
}

fn worker_loop(state: &Arc<(Mutex<SchedulerState>, Condvar)>, ctx: &TaskContext, fail_fast: bool) {
    let (lock, cvar) = &**state;

    loop {
        let mut blocked_notifications: Vec<(String, String, String)> = Vec::new();
        let next_task = {
            let mut guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

            loop {
                blocked_notifications.extend(mark_blocked_tasks(&mut guard, fail_fast));

                if guard.pending.is_empty() {
                    cvar.notify_all();
                    drop(guard);
                    emit_blocked_notifications(&ctx.reporter, &blocked_notifications);
                    return;
                }

                if let Some(task_id) = pick_ready_task(&guard)
                    && let Some(task) = guard.pending.remove(&task_id)
                {
                    for resource in &task.resources {
                        guard.held_resources.insert(resource.clone());
                    }
                    guard.running.insert(task.id.clone());
                    break task;
                }

                if guard.running.is_empty() {
                    // Nothing running and nothing ready means remaining tasks are unrunnable.
                    cvar.notify_all();
                    drop(guard);
                    emit_blocked_notifications(&ctx.reporter, &blocked_notifications);
                    return;
                }

                if blocked_notifications.is_empty() {
                    // Waiting on the held guard releases the lock and sleeps in one step,
                    // so a task that finishes in between cannot slip its notification past this worker.
                    guard = cvar.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner);
                } else {
                    // Notifications are emitted without the lock,
                    // so the state is checked again before waiting since it may have changed meanwhile.
                    drop(guard);
                    emit_blocked_notifications(&ctx.reporter, &blocked_notifications);
                    blocked_notifications.clear();
                    guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        };

        emit_blocked_notifications(&ctx.reporter, &blocked_notifications);

        let task_id = next_task.id.clone();
        let task_label = next_task.label.clone();
        let resources = next_task.resources.clone();
        let dry_summary = next_task.dry_run_summary.clone();
        let log_path = ctx.log_dir.join(format!("{task_id}.log"));
        let task_ctx = ctx.with_task_id(&task_id);

        if ctx.dry_run {
            ctx.reporter.on_dry_run(&task_id, &dry_summary);
        } else {
            ctx.reporter.on_task_start(&task_id, &task_label, &resources);
        }

        let started = Instant::now();
        let outcome = if ctx.dry_run {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (next_task.action)(&task_ctx, &log_path)
            }));
            TaskOutcome::DryRun
        } else {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (next_task.action)(&task_ctx, &log_path)
            })) {
                Ok(Ok(TaskActionResult::Passed)) => TaskOutcome::Passed,
                Ok(Ok(TaskActionResult::Skipped(reason))) => TaskOutcome::Skipped(reason),
                Ok(Ok(TaskActionResult::DryRun)) => TaskOutcome::DryRun,
                Ok(Err(err)) => TaskOutcome::Failed(format!("{err:#}")),
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic");
                    TaskOutcome::Failed(format!("Task panicked: {message}"))
                }
            }
        };
        let duration = started.elapsed();

        let report = TaskReport {
            id: task_id.clone(),
            label: task_label,
            outcome: outcome.clone(),
            duration,
            log_path: (!ctx.dry_run).then_some(log_path),
        };

        if !ctx.dry_run {
            ctx.reporter.on_task_finish(&report, &ctx.repo_root);
        }

        let mut guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.running.remove(&task_id);
        for resource in &resources {
            guard.held_resources.remove(resource);
        }
        if outcome.is_failure() {
            guard.any_failed = true;
            if fail_fast {
                guard.stop_scheduling = true;
            }
        }
        guard.completed.insert(task_id, report);
        drop(guard);
        cvar.notify_all();
    }
}

fn emit_blocked_notifications(reporter: &Reporter, notifications: &[(String, String, String)]) {
    for (id, label, reason) in notifications {
        reporter.on_task_blocked(id, label, reason);
    }
}

fn mark_blocked_tasks(guard: &mut SchedulerState, fail_fast: bool) -> Vec<(String, String, String)> {
    let mut all_blocked = Vec::new();
    loop {
        let mut newly_blocked: Vec<(String, String)> = Vec::new();
        for (id, task) in &guard.pending {
            if fail_fast && guard.stop_scheduling {
                newly_blocked.push((id.clone(), "cancelled by --fail-fast".to_owned()));
                continue;
            }
            for dep in &task.deps {
                if let Some(dep_report) = guard.completed.get(dep)
                    && !dep_report.outcome.unblocks_dependents()
                {
                    newly_blocked.push((id.clone(), format!("prerequisite '{dep}' did not pass")));
                    break;
                }
            }
        }

        if newly_blocked.is_empty() {
            break;
        }

        for (id, reason) in newly_blocked {
            if let Some(task) = guard.pending.remove(&id) {
                all_blocked.push((task.id.clone(), task.label.clone(), reason.clone()));
                guard.completed.insert(
                    id,
                    TaskReport {
                        id: task.id,
                        label: task.label,
                        outcome: TaskOutcome::Blocked(reason),
                        duration: Duration::ZERO,
                        log_path: None,
                    },
                );
            }
        }
    }
    all_blocked
}

fn pick_ready_task(guard: &SchedulerState) -> Option<String> {
    let mut best: Option<(&str, u32)> = None;
    for task in guard.pending.values() {
        let deps_ready = task.deps.iter().all(|dep| {
            guard
                .completed
                .get(dep)
                .is_some_and(|report| report.outcome.unblocks_dependents())
        });
        let earlier_tasks_finished = task.after.iter().all(|id| guard.completed.contains_key(id));
        if !deps_ready || !earlier_tasks_finished {
            continue;
        }
        let resources_free = task.resources.iter().all(|res| !guard.held_resources.contains(res));
        if !resources_free {
            continue;
        }
        match best {
            None => best = Some((task.id.as_str(), task.priority)),
            Some((_, best_priority)) if task.priority < best_priority => {
                best = Some((task.id.as_str(), task.priority));
            }
            _ => {}
        }
    }
    best.map(|(id, _)| id.to_owned())
}

fn collect_with_deps(id: &str, by_id: &BTreeMap<String, Vec<String>>, keep: &mut BTreeSet<String>) {
    if !keep.insert(id.to_owned()) {
        return;
    }
    if let Some(deps) = by_id.get(id) {
        for dep in deps {
            collect_with_deps(dep, by_id, keep);
        }
    }
}

#[cfg(test)]
mod test_scheduler {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn test_context(name: &str) -> TaskContext {
        let dir = std::env::temp_dir().join(format!("cli-tools-xtask-test-{name}-{}", std::process::id()));
        TaskContext {
            repo_root: dir.clone(),
            log_dir: dir,
            dry_run: false,
            verbose: false,
            fail_fast: false,
            flags: Arc::new(BTreeMap::new()),
            task_id: None,
            reporter: Arc::new(Reporter::silent()),
            shared: Arc::new(Mutex::new(SharedState::default())),
        }
    }

    fn noop_task(id: &str, priority: u32, deps: &[&str], resources: &[&str]) -> TaskSpec {
        TaskSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            priority,
            deps: deps.iter().map(|d| (*d).to_owned()).collect(),
            after: Vec::new(),
            resources: resources.iter().map(|r| (*r).to_owned()).collect(),
            scopes: Vec::new(),
            dry_run_summary: id.to_owned(),
            action: Box::new(|_, _| Ok(TaskActionResult::Passed)),
        }
    }

    #[test]
    fn an_after_task_runs_once_the_earlier_task_finished_even_when_it_failed() {
        let ctx = test_context("after-order");
        let earlier_finished = Arc::new(AtomicUsize::new(0));
        let seen_when_started = Arc::new(AtomicUsize::new(usize::MAX));
        let finished = Arc::clone(&earlier_finished);
        let mut earlier = noop_task("earlier", 1, &[], &[]);
        earlier.action = Box::new(move |_, _| {
            std::thread::sleep(Duration::from_millis(50));
            finished.store(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("earlier fails"))
        });
        let finished = Arc::clone(&earlier_finished);
        let seen = Arc::clone(&seen_when_started);
        let mut later = noop_task("later", 1, &[], &[]);
        later.after = vec!["earlier".to_owned(), "not-in-this-run".to_owned()];
        later.action = Box::new(move |_, _| {
            seen.store(finished.load(Ordering::SeqCst), Ordering::SeqCst);
            Ok(TaskActionResult::Passed)
        });

        let summary = run_schedule(vec![later, earlier], &ctx, 4, false, "all").expect("schedule should run");

        assert_eq!(
            seen_when_started.load(Ordering::SeqCst),
            1,
            "later started before earlier finished"
        );
        let later_report = summary
            .reports
            .iter()
            .find(|report| report.id == "later")
            .expect("later report");
        assert!(
            matches!(later_report.outcome, TaskOutcome::Passed),
            "{:?}",
            later_report.outcome
        );
    }

    #[test]
    fn a_long_dry_run_chain_never_hangs() {
        const RUNS: usize = 300;
        const CHAIN_LENGTH: usize = 40;
        for run in 0..RUNS {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut ctx = test_context("chain");
                ctx.dry_run = true;
                let ids: Vec<String> = (0..CHAIN_LENGTH).map(|index| format!("task{index:02}")).collect();
                let tasks: Vec<TaskSpec> = ids
                    .iter()
                    .enumerate()
                    .map(|(index, id)| {
                        let deps: Vec<&str> = index
                            .checked_sub(1)
                            .and_then(|previous| ids.get(previous))
                            .map(String::as_str)
                            .into_iter()
                            .collect();
                        noop_task(id, 1, &deps, &[])
                    })
                    .collect();
                let summary = run_schedule(tasks, &ctx, 8, false, "all");
                let _ = sender.send(summary.map(|summary| summary.reports.len()));
            });
            let finished = receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("run {run} of the scheduler hung"));
            assert_eq!(finished.expect("the schedule should succeed"), CHAIN_LENGTH);
        }
    }

    #[test]
    fn detects_cycles_and_unknown_deps() {
        let cyclic = vec![noop_task("a", 1, &["b"], &[]), noop_task("b", 1, &["a"], &[])];
        assert!(validate_graph(&cyclic).is_err());

        let missing = vec![noop_task("a", 1, &["does-not-exist"], &[])];
        assert!(validate_graph(&missing).is_err());

        let dup = vec![noop_task("a", 1, &[], &[]), noop_task("a", 1, &[], &[])];
        assert!(validate_graph(&dup).is_err());
    }

    #[test]
    fn enforces_multi_resource_exclusion_while_running_independent_tasks_in_parallel() {
        let ctx = test_context("resources");
        let active_cargo = Arc::new(AtomicUsize::new(0));
        let max_cargo = Arc::new(AtomicUsize::new(0));
        let active_total = Arc::new(AtomicUsize::new(0));
        let max_total = Arc::new(AtomicUsize::new(0));

        let make_timed = |id: &str, resources: Vec<Resource>, track_cargo: bool| {
            let active_cargo = Arc::clone(&active_cargo);
            let max_cargo = Arc::clone(&max_cargo);
            let active_total = Arc::clone(&active_total);
            let max_total = Arc::clone(&max_total);
            TaskSpec {
                id: id.to_owned(),
                label: id.to_owned(),
                priority: 10,
                deps: Vec::new(),
                after: Vec::new(),
                resources,
                scopes: Vec::new(),
                dry_run_summary: id.to_owned(),
                action: Box::new(move |_, _| {
                    let total = active_total.fetch_add(1, Ordering::SeqCst) + 1;
                    max_total.fetch_max(total, Ordering::SeqCst);
                    if track_cargo {
                        let cargo = active_cargo.fetch_add(1, Ordering::SeqCst) + 1;
                        max_cargo.fetch_max(cargo, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(60));
                    if track_cargo {
                        active_cargo.fetch_sub(1, Ordering::SeqCst);
                    }
                    active_total.fetch_sub(1, Ordering::SeqCst);
                    Ok(TaskActionResult::Passed)
                }),
            }
        };

        let tasks = vec![
            make_timed("cargo-1", vec!["cargo-target".to_owned()], true),
            make_timed(
                "cargo-2",
                vec!["cargo-target".to_owned(), "ios-project".to_owned()],
                true,
            ),
            make_timed("android-1", vec!["android-emulator".to_owned()], false),
            make_timed("web-1", vec!["web-node-modules".to_owned()], false),
        ];

        let summary = run_schedule(tasks, &ctx, 4, false, "all").unwrap();
        assert!(summary.is_success());
        assert_eq!(max_cargo.load(Ordering::SeqCst), 1);
        assert!(max_total.load(Ordering::SeqCst) >= 2);
        let _ = std::fs::remove_dir_all(&ctx.log_dir);
    }

    #[test]
    fn failure_blocks_dependents_and_streams_notes_to_reporter() {
        let dir = std::env::temp_dir().join(format!("cli-tools-xtask-test-failure-prop-{}", std::process::id()));
        let cfg =
            crate::ui::UiConfig::from_options(&crate::cli::RunOptions::default(), &crate::cli::EnvSnapshot::default());
        let (reporter, output_buf) = Reporter::line_to_buffer(cfg);
        let ctx = TaskContext {
            repo_root: dir.clone(),
            log_dir: dir,
            dry_run: false,
            verbose: false,
            fail_fast: false,
            flags: Arc::new(BTreeMap::new()),
            task_id: None,
            reporter: Arc::new(reporter),
            shared: Arc::new(Mutex::new(SharedState::default())),
        };

        let tasks = vec![
            TaskSpec {
                id: "fail-root".to_owned(),
                label: "fail".to_owned(),
                priority: 10,
                deps: Vec::new(),
                after: Vec::new(),
                resources: Vec::new(),
                scopes: Vec::new(),
                dry_run_summary: "fail".to_owned(),
                action: Box::new(|task_ctx, log_path| {
                    task_ctx.log_note(log_path, "about to fail")?;
                    bail!("boom")
                }),
            },
            noop_task("child", 5, &["fail-root"], &[]),
            noop_task("grandchild", 5, &["child"], &[]),
            noop_task("unrelated", 5, &[], &[]),
        ];

        let summary = run_schedule(tasks, &ctx, 2, false, "all").unwrap();
        assert!(!summary.is_success());
        let outcomes: BTreeMap<&str, &TaskOutcome> =
            summary.reports.iter().map(|r| (r.id.as_str(), &r.outcome)).collect();
        assert!(matches!(outcomes["fail-root"], TaskOutcome::Failed(_)));
        assert!(matches!(outcomes["child"], TaskOutcome::Blocked(_)));
        assert!(matches!(outcomes["grandchild"], TaskOutcome::Blocked(_)));
        assert_eq!(outcomes["unrelated"], &TaskOutcome::Passed);

        let rendered = String::from_utf8(output_buf.lock().unwrap().clone()).unwrap();
        assert!(rendered.contains("│ about to fail"));
        assert!(rendered.contains("BLOCK child"));
        let _ = std::fs::remove_dir_all(&ctx.log_dir);
    }

    #[test]
    fn filter_only_pulls_in_transitive_dependencies() {
        let tasks = vec![
            noop_task("a", 1, &[], &[]),
            noop_task("b", 1, &["a"], &[]),
            noop_task("c", 1, &["b"], &[]),
            noop_task("d", 1, &[], &[]),
        ];
        let filtered = filter_tasks(tasks, None, &["c".to_owned()]).unwrap();
        let ids: Vec<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn lower_priority_number_runs_first() {
        let ctx = test_context("priority-order");
        let order = Arc::new(Mutex::new(Vec::new()));
        let make = |id: &str, priority: u32| {
            let order = Arc::clone(&order);
            let id_owned = id.to_owned();
            TaskSpec {
                id: id_owned.clone(),
                label: id_owned.clone(),
                priority,
                deps: Vec::new(),
                after: Vec::new(),
                resources: vec!["single-lock".to_owned()],
                scopes: Vec::new(),
                dry_run_summary: id_owned.clone(),
                action: Box::new(move |_, _| {
                    order.lock().unwrap().push(id_owned);
                    Ok(TaskActionResult::Passed)
                }),
            }
        };

        let tasks = vec![make("low", 8), make("high", 1), make("mid", 5)];
        let summary = run_schedule(tasks, &ctx, 1, false, "all").unwrap();
        assert!(summary.is_success());
        assert_eq!(*order.lock().unwrap(), vec!["high", "mid", "low"]);
        let _ = std::fs::remove_dir_all(&ctx.log_dir);
    }

    #[test]
    fn panicking_task_fails_cleanly_and_blocks_dependents_without_deadlock() {
        let ctx = test_context("panic-no-deadlock");
        let panicker = TaskSpec {
            id: "boom".to_owned(),
            label: "Boom".to_owned(),
            priority: 1,
            deps: Vec::new(),
            after: Vec::new(),
            resources: vec!["shared-lock".to_owned()],
            scopes: Vec::new(),
            dry_run_summary: "boom".to_owned(),
            action: Box::new(|_, _| panic!("builtin exploded")),
        };
        let dependent = noop_task("child", 2, &["boom"], &[]);
        let independent = noop_task("other", 3, &[], &["shared-lock"]);

        let summary = run_schedule(vec![panicker, dependent, independent], &ctx, 2, false, "all").unwrap();
        assert!(!summary.is_success());
        assert!(matches!(
            summary.reports.iter().find(|r| r.id == "boom").unwrap().outcome,
            TaskOutcome::Failed(ref msg) if msg.contains("builtin exploded")
        ));
        assert!(matches!(
            summary.reports.iter().find(|r| r.id == "child").unwrap().outcome,
            TaskOutcome::Blocked(_)
        ));
        assert_eq!(
            summary.reports.iter().find(|r| r.id == "other").unwrap().outcome,
            TaskOutcome::Passed
        );
        let _ = std::fs::remove_dir_all(&ctx.log_dir);
    }
}
