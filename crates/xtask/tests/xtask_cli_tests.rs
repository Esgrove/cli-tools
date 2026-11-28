//! End to end tests that run the `xtask` binary against task files in temporary git repositories.
//!
//! They cover task file discovery from nested directories, project flags,
//! variables and notes exported between tasks, and the end of run output.

use std::path::Path;
use std::process::{Command, Output};

use cli_tools_common::test_support::TempTree;

const TASK_FILE: &str = r#"
[settings]
rerun_prefix = "./run.sh"

[flags.all]
short = "a"
help = "Run every check"

[[tasks]]
id = "prepare"
label = "Prepare"
priority = 1
command = ["sh", "-c", "echo DEVICE=dev-42 >> \"$XTASK_ENV\"; echo 'Report: out/index.html' >> \"$XTASK_NOTES\""]

[[tasks]]
id = "use-device"
label = "Use device"
deps = ["prepare"]
command = ["sh", "-c", "test \"$0\" = dev-42", "${DEVICE}"]

[[tasks]]
id = "failing"
label = "Failing check"
when = "all"
command = ["sh", "-c", "echo boom >&2; exit 3"]

[[tasks]]
id = "after-failing"
label = "After failing"
when = "all"
deps = ["failing"]
command = ["true"]
"#;

/// Temporary git repository with the task file at its root and a nested working directory.
fn repository() -> TempTree {
    let tree = repository_with_tasks(TASK_FILE);
    tree.dir("nested/dir");
    tree
}

/// Temporary git repository with the given task file at its root.
fn repository_with_tasks(task_file: &str) -> TempTree {
    let tree = TempTree::with_entries(&[".git/"]);
    tree.file_with("xtask.toml", task_file);
    tree
}

fn run_xtask(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["--output", "plain", "--color", "never"])
        .args(args)
        .current_dir(directory)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env("HOME", directory)
        .output()
        .expect("xtask should start")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn runs_tasks_found_from_a_nested_directory() {
    let repository = repository();
    let output = run_xtask(&repository.path().join("nested").join("dir"), &[]);
    let text = stdout(&output);
    assert!(
        output.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("PASS  prepare"));
    assert!(
        text.contains("PASS  use-device"),
        "the exported variable reaches the dependent task"
    );
    assert!(!text.contains("failing"), "tasks behind a flag do not run without it");
}

#[test]
fn ends_with_failure_recap_and_result_lines_without_a_summary_table() {
    let repository = repository();
    let output = run_xtask(repository.path(), &["-a"]);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");

    let failures = text.find("=== FAILURES ===").expect("failure recap should be printed");
    let result = text
        .find("Result: 2 passed, 1 failed, 1 blocked, 0 skipped")
        .expect("result line");
    assert!(failures < result, "the recap comes before the result line");
    assert!(text.contains("Re-run: ./run.sh --all failing"));
    assert!(!text.contains("Summary"));
    assert_eq!(text.matches("PASS  prepare").count(), 1, "passed tasks are listed once");

    let tail: Vec<&str> = text.lines().rev().take(3).collect();
    assert_eq!(tail[0], "Note:   Report: out/index.html");
    let logs_line = tail[1].replace('\\', "/");
    assert!(logs_line.starts_with("Logs:   target/xtask-logs/"), "{logs_line}");
    assert!(tail[2].starts_with("Result: "));
}

#[test]
fn runs_selected_task_ids_with_prerequisites() {
    let repository = repository();
    let output = run_xtask(repository.path(), &["list", "use-device"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("Resolved tasks (2) "));
    assert!(text.contains("prepare"));

    let unknown = run_xtask(repository.path(), &["missing-task"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("Unknown task or group 'missing-task'"));
}

#[test]
fn does_not_search_past_the_repository_root() {
    let outer = tempfile::tempdir().expect("should create temp dir");
    std::fs::write(outer.path().join("xtask.toml"), TASK_FILE).expect("should write task file");
    let inner = outer.path().join("inner");
    std::fs::create_dir_all(inner.join(".git")).expect("should create inner repository");

    let output = run_xtask(&inner, &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("No xtask.toml found"));

    let explicit = run_xtask(&inner, &["--config", "../xtask.toml", "list"]);
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
}

#[test]
fn help_lists_project_flags_and_the_task_file() {
    let repository = repository();
    let output = run_xtask(repository.path(), &["--help"]);
    let text = stdout(&output);
    assert!(output.status.success());
    assert!(text.contains("Project flags:"));
    assert!(text.contains("-a, --all"));
    assert!(text.contains("Task file: "));
}

#[test]
fn help_without_a_task_file_has_no_task_file_note() {
    let directory = tempfile::tempdir().expect("should create temp dir");
    std::fs::create_dir_all(directory.path().join(".git")).expect("should create repository");
    let output = run_xtask(directory.path(), &["-h"]);
    let text = stdout(&output);
    assert!(output.status.success());
    assert!(text.contains("list"));
    assert!(!text.contains("xtask.toml found"));
    assert!(!text.contains("Task file: "));
}

#[test]
fn rejects_builtins_without_a_project_runner() {
    let repository = repository();
    std::fs::write(
        repository.path().join("xtask.toml"),
        "[[tasks]]\nid = \"setup\"\nlabel = \"Setup\"\nbuiltin = \"prepare-device\"\n",
    )
    .expect("should write task file");
    let output = run_xtask(repository.path(), &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs the project's own runner"));
}

#[cfg(unix)]
#[test]
fn hands_the_run_over_to_the_project_runner() {
    let repository = repository();
    std::fs::write(
        repository.path().join("xtask.toml"),
        "[settings]\nrunner = [\"sh\", \"-c\", \"echo delegated $XTASK_DELEGATED \\\"$*\\\"; exit 7\", \"runner\"]\n",
    )
    .expect("should write task file");
    let output = run_xtask(repository.path(), &["-a", "fmt"]);
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        stdout(&output).trim(),
        "delegated 1 --output plain --color never -a fmt"
    );
}

/// Task file where `docs` should only run after `lint` when both run, and `lint` always fails.
const AFTER_TASK_FILE: &str = r#"
[[tasks]]
id = "lint"
label = "Lint"
command = ["sh", "-c", "sleep 0.2; exit 1"]

[[tasks]]
id = "docs"
label = "Docs"
after = ["lint"]
command = ["sh", "-c", "test ! -e started-docs && touch started-docs"]
"#;

#[test]
fn selecting_a_task_does_not_pull_in_its_after_tasks() {
    let repository = repository_with_tasks(AFTER_TASK_FILE);
    let output = run_xtask(repository.path(), &["docs"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("PASS  docs"), "{text}");
    assert!(!text.contains("lint"), "{text}");
}

#[test]
fn a_failed_after_task_does_not_block_the_task_waiting_for_it() {
    let repository = repository_with_tasks(AFTER_TASK_FILE);
    let output = run_xtask(repository.path(), &[]);
    let text = stdout(&output);
    assert!(text.contains("FAIL  lint"), "{text}");
    assert!(text.contains("PASS  docs"), "{text}");
}

/// Hard prerequisites and soft ordering after a task that skips itself.
const SKIPPED_PREREQUISITE_TASK_FILE: &str = r#"
[[tasks]]
id = "prepare"
label = "Prepare"
priority = 10
skip_if_missing = "cli-tools-xtask-test-missing-prerequisite-8pkw"
command = ["sh", "-c", "exit 1"]

[[tasks]]
id = "hard"
label = "Hard dependent"
priority = 1
deps = ["prepare"]
command = ["sh", "-c", "exit 0"]

[[tasks]]
id = "transitive"
label = "Transitive dependent"
deps = ["hard"]
command = ["sh", "-c", "exit 0"]

[[tasks]]
id = "soft"
label = "Soft dependent"
priority = 1
after = ["prepare"]
command = ["sh", "-c", "exit 0"]
"#;

/// Skipping a hard prerequisite blocks its dependents but releases soft ordering.
#[cfg(unix)]
#[test]
fn skipped_prerequisite_blocks_hard_dependents_but_not_soft_ordering() {
    let repository = repository_with_tasks(SKIPPED_PREREQUISITE_TASK_FILE);
    let output = run_xtask(repository.path(), &["--jobs", "1"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("BLOCK hard"), "{text}");
    assert!(text.contains("BLOCK transitive"), "{text}");
    let skip = text
        .find("SKIP  prepare")
        .expect("the optional prerequisite should skip");
    let soft_start = text
        .find("START soft")
        .expect("soft ordering should still allow the task to run");
    assert!(skip < soft_start, "{text}");
    assert!(text.contains("PASS  soft"), "{text}");
}

/// An explicitly requested task blocked by a skip produces a nonzero exit status.
#[test]
fn explicitly_requested_blocked_task_fails_the_run() {
    let repository = repository_with_tasks(SKIPPED_PREREQUISITE_TASK_FILE);
    let output = run_xtask(repository.path(), &["transitive"]);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("BLOCK transitive"), "{text}");
    assert!(
        text.contains("Result: 0 passed, 0 failed, 2 blocked, 1 skipped"),
        "{text}"
    );

    let skipped = run_xtask(repository.path(), &["prepare"]);
    assert!(skipped.status.success(), "{}", stdout(&skipped));
    let dry_run = run_xtask(repository.path(), &["--dryrun", "transitive"]);
    assert!(dry_run.status.success(), "{}", stdout(&dry_run));
    assert!(stdout(&dry_run).contains("Dry-run resolved 3 task(s)."));
}

/// Scoped and explicit selections ignore an unrelated task's disabled prerequisite.
#[test]
fn selecting_fmt_ignores_an_unrelated_disabled_prerequisite() {
    let repository = repository_with_tasks(
        r#"
[flags.web]

[[tasks]]
id = "setup"
label = "Setup"
when = "web"
command = ["true"]

[[tasks]]
id = "web-tests"
label = "Web tests"
deps = ["setup"]
command = ["true"]

[[tasks]]
id = "fmt"
label = "Format"
scopes = ["rust"]
after = ["setup", "web-tests"]
command = ["true"]
"#,
    );
    for args in [&["list", "fmt"][..], &["--scope", "rust", "list"]] {
        let listed = run_xtask(repository.path(), args);
        assert!(listed.status.success(), "{listed:?}");
        assert!(stdout(&listed).contains("Resolved tasks (1) "));
        assert!(stdout(&listed).contains("  fmt"));
    }
    #[cfg(unix)]
    {
        let run = run_xtask(repository.path(), &["fmt"]);
        assert!(run.status.success(), "{run:?}");
        assert!(stdout(&run).contains("PASS  fmt"));
    }

    for args in [&["list", "web-tests"][..], &["web-tests"]] {
        let rejected = run_xtask(repository.path(), args);
        assert!(!rejected.status.success(), "{rejected:?}");
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("'setup', which is not enabled"));
    }
}

/// An unknown ordering ID is an error rather than an ordering silently ignored.
#[test]
fn list_and_run_reject_unknown_after_ids() {
    let repository = repository_with_tasks(
        r#"
[[tasks]]
id = "fmt"
label = "Format"
after = ["typo"]
command = ["true"]
"#,
    );
    for args in [&["list", "fmt"][..], &["--dryrun", "fmt"]] {
        let output = run_xtask(repository.path(), args);
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("orders after unknown task 'typo'"));
    }
}

/// Listing reports broken hard prerequisites in the selected transitive closure.
#[test]
fn list_rejects_unknown_hard_prerequisites_in_the_selected_closure() {
    let repository = repository_with_tasks(
        r#"
[[tasks]]
id = "fmt"
label = "Format"
deps = ["setup"]
command = ["true"]

[[tasks]]
id = "setup"
label = "Setup"
deps = ["missing"]
command = ["true"]
"#,
    );
    let output = run_xtask(repository.path(), &["list", "fmt"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Task 'setup' depends on unknown task 'missing'"));
}

/// Mixed hard and soft cycles are rejected only when all their edges are selected.
#[test]
fn list_and_run_reject_selected_cycles() {
    let repository = repository_with_tasks(
        r#"
[[tasks]]
id = "lint"
label = "Lint"
deps = ["fmt"]
command = ["true"]

[[tasks]]
id = "fmt"
label = "Format"
after = ["lint"]
command = ["true"]
"#,
    );
    for args in [&["list", "lint"][..], &["--dryrun", "lint"]] {
        let output = run_xtask(repository.path(), args);
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("dependency graph contains a cycle"));
    }
    let output = run_xtask(repository.path(), &["list", "fmt"]);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("Resolved tasks (1) "));
}

const GROUPED_TASK_FILE: &str = r#"
[settings]
default_group = "test"

[groups.install]
help = "Build and install the binaries"

[[tasks]]
id = "ungrouped"
label = "Ungrouped"
command = ["true"]

[[tasks]]
id = "unit-tests"
label = "Unit tests"
groups = ["test"]
command = ["true"]

[[tasks]]
id = "build"
label = "Build"
groups = ["test", "install"]
command = ["true"]

[[tasks]]
id = "copy"
label = "Copy"
groups = ["install"]
deps = ["build"]
command = ["true"]
"#;

/// Task IDs listed by `xtask list` with the given extra arguments.
fn listed_task_ids(directory: &Path, args: &[&str]) -> Vec<String> {
    let output = run_xtask(directory, &[&["list"], args].concat());
    let text = stdout(&output);
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn plain_run_takes_ungrouped_tasks_and_the_default_group() {
    let repository = repository_with_tasks(GROUPED_TASK_FILE);
    assert_eq!(
        listed_task_ids(repository.path(), &[]),
        ["ungrouped", "unit-tests", "build"]
    );
}

#[test]
fn named_group_runs_only_its_tasks() {
    let repository = repository_with_tasks(GROUPED_TASK_FILE);
    assert_eq!(listed_task_ids(repository.path(), &["install"]), ["build", "copy"]);

    let output = run_xtask(repository.path(), &["--dryrun", "install"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("copy"));
    assert!(!text.contains("unit-tests"));
}

#[test]
fn groups_and_task_ids_combine() {
    let repository = repository_with_tasks(GROUPED_TASK_FILE);
    assert_eq!(
        listed_task_ids(repository.path(), &["install", "ungrouped"]),
        ["ungrouped", "build", "copy"]
    );
}

#[test]
fn help_lists_the_task_groups() {
    let repository = repository_with_tasks(GROUPED_TASK_FILE);
    let output = run_xtask(repository.path(), &["--help"]);
    let text = stdout(&output);
    assert!(output.status.success());
    assert!(text.contains("Task groups:"), "{text}");
    assert!(text.contains("install  Build and install the binaries"), "{text}");
    assert!(text.contains("test     (default)"), "{text}");
}

#[test]
fn unknown_name_lists_groups_and_tasks() {
    let repository = repository_with_tasks(GROUPED_TASK_FILE);
    let output = run_xtask(repository.path(), &["deploy"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Unknown task or group 'deploy'"), "{error}");
    assert!(error.contains("Groups: install, test"), "{error}");
}
