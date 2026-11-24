//! End of run output: the failure recap followed by the result, log directory, and note lines.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use super::{Reporter, ReporterBackend, collect_failure_tail, display_relative_path};
use crate::scheduler::{RunSummary, TaskOutcome, TaskReport};

impl Reporter {
    /// Clear any active progress UI, then print the failure recap followed by the result lines.
    ///
    /// Each task's outcome was already printed live when it finished,
    /// so the end of the run only adds what the live lines do not show:
    /// the failure details, the totals, the log directory, and the notes.
    pub fn finish_and_print_summary(&self, summary: &RunSummary, repo_root: &Path, dry_run: bool) {
        let mut lines = self.render_failures_recap(summary, repo_root);
        lines.extend(self.render_result_lines(summary, repo_root, dry_run));

        let mut guard = self.lock_inner();
        for task in guard.active.values() {
            if let Some(pb) = &task.bar {
                pb.finish_and_clear();
            }
        }
        guard.active.clear();

        match &mut guard.backend {
            ReporterBackend::Interactive(interactive) => {
                interactive.overall_bar.finish_and_clear();
                let _ = interactive.multi.clear();
                let mut stdout = io::stdout().lock();
                for line in &lines {
                    let _ = writeln!(stdout, "{line}");
                }
            }
            ReporterBackend::Line(target) => {
                for line in &lines {
                    target.write_line(line);
                }
            }
        }
    }

    /// The `Result:`, `Logs:`, and `Note:` lines that end a run.
    fn render_result_lines(&self, summary: &RunSummary, repo_root: &Path, dry_run: bool) -> Vec<String> {
        let mut passed = 0_usize;
        let mut failed = 0_usize;
        let mut blocked = 0_usize;
        let mut skipped = 0_usize;
        let mut dry_count = 0_usize;
        let mut cumulative = Duration::ZERO;

        for report in &summary.reports {
            cumulative = cumulative.saturating_add(report.duration);
            match &report.outcome {
                TaskOutcome::Passed => passed = passed.saturating_add(1),
                TaskOutcome::Failed(_) => failed = failed.saturating_add(1),
                TaskOutcome::Blocked(_) => blocked = blocked.saturating_add(1),
                TaskOutcome::Skipped(_) => skipped = skipped.saturating_add(1),
                TaskOutcome::DryRun => dry_count = dry_count.saturating_add(1),
            }
        }

        let mut lines = vec![String::new()];
        if dry_run {
            lines.push(format!("Dry-run resolved {dry_count} task(s)."));
        } else {
            let wall = summary.wall_duration.as_secs_f64();
            let total = cumulative.as_secs_f64();
            let speedup = if wall > 0.01 { total / wall } else { 1.0 };
            let passed_str = if failed == 0 && passed > 0 {
                self.palette.pass.apply_to(format!("{passed} passed")).to_string()
            } else {
                format!("{passed} passed")
            };
            let failed_str = if failed > 0 {
                self.palette.fail.apply_to(format!("{failed} failed")).to_string()
            } else {
                format!("{failed} failed")
            };
            lines.push(format!(
                "Result: {passed_str}, {failed_str}, {blocked} blocked, {skipped} skipped \
                 (wall: {wall:.1}s, cumulative: {total:.1}s, speedup: {speedup:.2}x)"
            ));
            lines.push(format!(
                "Logs:   {}",
                display_relative_path(&summary.log_dir, repo_root)
            ));
        }

        for note in &summary.notes {
            lines.push(format!("Note:   {note}"));
        }

        lines
    }

    /// Details of every failed task: error, log path, re-run command, and the end of its log.
    fn render_failures_recap(&self, summary: &RunSummary, repo_root: &Path) -> Vec<String> {
        let failed_reports: Vec<&TaskReport> = summary
            .reports
            .iter()
            .filter(|report| report.outcome.is_failure())
            .collect();
        if failed_reports.is_empty() {
            return Vec::new();
        }

        let mut lines = vec![
            String::new(),
            self.palette.fail.apply_to("=== FAILURES ===").to_string(),
        ];
        let max_tail = self.config.failure_recap_lines();
        let dim_gutter = self.palette.dim.apply_to("│");

        for report in failed_reports {
            let TaskOutcome::Failed(err) = &report.outcome else {
                continue;
            };
            lines.push(String::new());
            if self.config.github_actions {
                lines.push(format!("::group::FAIL {} ({})", report.id, report.label));
            }
            let header = self.palette.fail.apply_to(format!(
                "FAIL {} — {} ({:.1}s)",
                report.id,
                report.label,
                report.duration.as_secs_f64()
            ));
            lines.push(header.to_string());
            lines.push(format!("  Error:  {err}"));
            if let Some(log_path) = report.log_path.as_deref().filter(|path| path.is_file()) {
                lines.push(format!("  Log:    {}", display_relative_path(log_path, repo_root)));
                lines.push(format!("  Re-run: {}", self.config.rerun_command(&report.id)));
                let tail = collect_failure_tail(log_path, max_tail);
                if !tail.is_empty() {
                    lines.push(format!("  Last {} line(s):", tail.len()));
                    for line in tail {
                        lines.push(format!("    {dim_gutter} {line}"));
                    }
                }
            } else {
                lines.push(format!("  Re-run: {}", self.config.rerun_command(&report.id)));
            }
            if self.config.github_actions {
                lines.push("::endgroup::".to_owned());
            }
        }

        lines
    }
}

#[cfg(test)]
mod test_summary {
    use super::*;
    use crate::cli::ResolvedOutputMode;
    use crate::ui::UiConfig;

    #[test]
    fn ci_line_reporter_emits_plan_heartbeat_github_annotations_and_failure_recap() {
        let repo_root = std::env::temp_dir().join(format!("cli-tools-xtask-ui-ci-{}", std::process::id()));
        let log_dir = repo_root.join("target/test-logs/20261124T000000Z");
        std::fs::create_dir_all(&log_dir).unwrap();
        let clippy_log = log_dir.join("cargo-clippy.log");
        std::fs::write(
            &clippy_log,
            "$ cargo clippy\n\x1b[31merror: unused variable `x`\x1b[0m\n  --> src/lib.rs:10:5\n",
        )
        .unwrap();

        let config = UiConfig {
            mode: ResolvedOutputMode::Ci,
            colors_enabled: false,
            github_actions: true,
            tail_lines: None,
            dry_run: false,
            rerun_prefix: "./test.sh --all".to_owned(),
        };
        let (reporter, buffer) = Reporter::line_to_buffer(config);

        reporter.on_plan(3, 4, "rust");
        reporter.on_task_start("cargo-fmt", "Rust formatting", &[]);
        reporter.on_task_start("cargo-clippy", "Rust lints", &["cargo-target".to_owned()]);
        reporter.emit_heartbeat();
        reporter.on_task_command("cargo-clippy", "cargo clippy --workspace");
        reporter.on_task_output("cargo-clippy", "error: unused variable `x`");
        reporter.on_task_output("cargo-clippy", "  --> src/lib.rs:10:5");

        let fmt_report = TaskReport {
            id: "cargo-fmt".to_owned(),
            label: "Rust formatting".to_owned(),
            outcome: TaskOutcome::Passed,
            duration: Duration::from_millis(400),
            log_path: None,
        };
        let clippy_report = TaskReport {
            id: "cargo-clippy".to_owned(),
            label: "Rust lints".to_owned(),
            outcome: TaskOutcome::Failed("Command exited with status 101\ndetails".to_owned()),
            duration: Duration::from_millis(14_200),
            log_path: Some(clippy_log),
        };
        let sqlx_report = TaskReport {
            id: "sqlx-check".to_owned(),
            label: "SQLx check".to_owned(),
            outcome: TaskOutcome::Blocked("prerequisite 'cargo-clippy' did not pass".to_owned()),
            duration: Duration::ZERO,
            log_path: None,
        };

        reporter.on_task_finish(&fmt_report, &repo_root);
        reporter.on_task_finish(&clippy_report, &repo_root);
        reporter.on_task_blocked("sqlx-check", "SQLx check", "prerequisite 'cargo-clippy' did not pass");

        let summary = RunSummary {
            reports: vec![fmt_report, clippy_report, sqlx_report],
            wall_duration: Duration::from_millis(14_200),
            log_dir,
            notes: vec!["Rust coverage: target/llvm-cov/html/index.html".to_owned()],
        };
        reporter.finish_and_print_summary(&summary, &repo_root, false);

        let rendered = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(rendered.contains("PLAN  3 task(s) (jobs=4, scope=rust)"));
        assert!(rendered.contains("WAIT  still running (2): cargo-clippy"));
        assert!(rendered.contains("CMD   cargo-clippy                 $ cargo clippy --workspace"));
        assert!(rendered.contains("::error title=cargo-clippy::Command exited with status 101%0Adetails"));
        assert!(rendered.contains("│ error: unused variable `x`"));
        assert!(rendered.contains("└─ log: target/test-logs/20261124T000000Z/cargo-clippy.log"));
        assert!(rendered.contains("BLOCK sqlx-check                   - prerequisite 'cargo-clippy' did not pass"));
        assert!(!rendered.contains("Summary"));
        assert_eq!(
            rendered.matches("cargo-fmt").count(),
            3,
            "only the start, heartbeat, and pass lines name cargo-fmt, the summary does not repeat it"
        );
        assert!(rendered.contains("Logs:   target/test-logs/20261124T000000Z"));
        assert!(rendered.contains("Note:   Rust coverage: target/llvm-cov/html/index.html"));
        assert!(rendered.contains("=== FAILURES ==="));
        assert!(rendered.contains("::group::FAIL cargo-clippy (Rust lints)"));
        assert!(rendered.contains("FAIL cargo-clippy — Rust lints (14.2s)"));
        assert!(rendered.contains("Re-run: ./test.sh --all cargo-clippy"));
        assert!(rendered.contains("::endgroup::"));

        let tail: Vec<&str> = rendered.lines().rev().take(4).collect();
        assert!(tail[0].starts_with("Note:   "), "the run ends with the notes: {tail:?}");
        assert!(tail[1].starts_with("Logs:   "));
        assert!(tail[2].starts_with("Result: 1 passed, 1 failed, 1 blocked, 0 skipped"));
        assert_eq!(tail[3], "");

        let _ = std::fs::remove_dir_all(&repo_root);
    }
}
