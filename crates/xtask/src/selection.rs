//! Choosing the tasks of one run.
//!
//! Positional names are split into task groups and task IDs,
//! and the run takes the tasks of the selected groups that match the scope,
//! the explicitly named tasks, and the prerequisites of both.
//! Without names, the run takes the ungrouped tasks and the tasks of the default group.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::config::TaskFile;
use crate::scheduler::TaskSpec;

/// Positional names split into the task groups and the task IDs they refer to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SelectedNames {
    /// Names of known task groups, in the order given.
    pub groups: Vec<String>,
    /// Every other name, treated as a task ID.
    pub task_ids: Vec<String>,
}

/// What a run selects from the enabled tasks before adding prerequisites.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    /// Groups whose tasks are selected.
    pub groups: BTreeSet<String>,
    /// Whether tasks without any group are selected.
    pub include_ungrouped: bool,
    /// Explicitly named task IDs, selected whatever their groups and scopes.
    pub task_ids: Vec<String>,
    /// The selected scope and every scope it includes, or `None` to keep every scope.
    pub scopes: Option<BTreeSet<String>>,
    /// Every group the task file knows, listed when a name is unknown.
    pub known_groups: Vec<String>,
}

impl SelectedNames {
    /// Split positional names into known group names and task IDs.
    ///
    /// Names are trimmed and empty names dropped.
    #[must_use]
    pub fn from_names(task_file: &TaskFile, names: Vec<String>) -> Self {
        let known_groups = task_file.known_groups();
        let mut selected = Self::default();
        for name in names {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if known_groups.contains(name) {
                selected.groups.push(name.to_owned());
            } else {
                selected.task_ids.push(name.to_owned());
            }
        }
        selected
    }
}

impl Selection {
    /// Build the selection for the named groups, task IDs, and scope.
    ///
    /// Without groups and task IDs the ungrouped tasks and the default group are selected.
    ///
    /// # Errors
    ///
    /// Returns an error if the scope is unknown.
    pub fn new(task_file: &TaskFile, groups: &[String], task_ids: &[String], scope: Option<&str>) -> Result<Self> {
        let nothing_named = groups.is_empty() && task_ids.is_empty();
        let groups = if nothing_named {
            task_file.settings.default_group.iter().cloned().collect()
        } else {
            groups.iter().cloned().collect()
        };
        Ok(Self {
            groups,
            include_ungrouped: nothing_named,
            task_ids: task_ids.to_vec(),
            scopes: resolve_scope(task_file, scope)?,
            known_groups: task_file.known_groups().into_iter().map(str::to_owned).collect(),
        })
    }

    /// Label describing the selection in the run header.
    ///
    /// This is the scope when one is given, otherwise the selected groups and task IDs, or "all" when there are none.
    #[must_use]
    pub fn label(&self, scope: Option<&str>) -> String {
        if let Some(scope) = scope {
            return scope.to_owned();
        }
        let names: Vec<&str> = self.groups.iter().chain(&self.task_ids).map(String::as_str).collect();
        if names.is_empty() {
            "all".to_owned()
        } else {
            names.join(", ")
        }
    }

    /// Whether `task` is selected through its groups and scopes.
    fn selects(&self, task: &TaskSpec) -> bool {
        let in_group = if task.groups.is_empty() {
            self.include_ungrouped
        } else {
            task.groups.iter().any(|group| self.groups.contains(group))
        };
        in_group
            && self
                .scopes
                .as_ref()
                .is_none_or(|selected| task.scopes.iter().any(|scope| selected.contains(scope)))
    }
}

/// Keep the selected tasks and their transitive prerequisites, in their original order.
///
/// # Errors
///
/// Returns an error if a task ID is not one of the enabled tasks.
pub fn filter_tasks(tasks: Vec<TaskSpec>, selection: &Selection) -> Result<Vec<TaskSpec>> {
    let by_id: BTreeMap<String, Vec<String>> = tasks.iter().map(|task| (task.id.clone(), task.deps.clone())).collect();

    let mut keep = BTreeSet::new();
    for id in &selection.task_ids {
        if !by_id.contains_key(id) {
            let available: Vec<&str> = by_id.keys().map(String::as_str).collect();
            let groups = if selection.known_groups.is_empty() {
                String::new()
            } else {
                format!("Groups: {}. ", selection.known_groups.join(", "))
            };
            bail!(
                "Unknown task or group '{id}'. {groups}Tasks for the current flags: {}",
                available.join(", ")
            );
        }
        collect_with_deps(id, &by_id, &mut keep);
    }
    for task in &tasks {
        if selection.selects(task) {
            collect_with_deps(&task.id, &by_id, &mut keep);
        }
    }

    Ok(tasks.into_iter().filter(|task| keep.contains(&task.id)).collect())
}

/// Validate the requested scope and return it together with every scope it includes.
fn resolve_scope(task_file: &TaskFile, scope: Option<&str>) -> Result<Option<BTreeSet<String>>> {
    let Some(scope) = scope.filter(|scope| *scope != "all") else {
        return Ok(None);
    };
    let known = task_file.known_scopes();
    if !known.contains(scope) {
        bail!(
            "Unknown scope '{scope}'. Available: all, {}",
            known.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(Some(task_file.expand_scope(scope)))
}

/// Recursively collect `id` and all of its transitive dependencies into `keep`.
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
mod test_task_selection {
    use super::*;
    use crate::scheduler::TaskActionResult;

    const TASK_FILE: &str = r#"
        [settings]
        default_group = "test"

        [scopes.checks]
        includes = ["rust"]

        [[tasks]]
        id = "ungrouped"
        label = "Ungrouped"
        scopes = ["rust"]
        command = ["true"]

        [[tasks]]
        id = "unit-tests"
        label = "Unit tests"
        groups = ["test"]
        scopes = ["rust"]
        command = ["true"]

        [[tasks]]
        id = "web-tests"
        label = "Web tests"
        groups = ["test"]
        scopes = ["web"]
        command = ["true"]

        [[tasks]]
        id = "build"
        label = "Build"
        groups = ["release"]
        command = ["true"]

        [[tasks]]
        id = "copy"
        label = "Copy"
        groups = ["install"]
        deps = ["build"]
        command = ["true"]
    "#;

    fn task(id: &str, deps: &[&str], scopes: &[&str], groups: &[&str]) -> TaskSpec {
        TaskSpec {
            id: id.to_owned(),
            label: id.to_owned(),
            priority: 5,
            deps: deps.iter().map(|dep| (*dep).to_owned()).collect(),
            after: Vec::new(),
            resources: Vec::new(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            dry_run_summary: id.to_owned(),
            action: Box::new(|_, _| Ok(TaskActionResult::Passed)),
        }
    }

    fn tasks() -> Vec<TaskSpec> {
        vec![
            task("ungrouped", &[], &["rust"], &[]),
            task("unit-tests", &[], &["rust"], &["test"]),
            task("web-tests", &[], &["web"], &["test"]),
            task("build", &[], &[], &["release"]),
            task("copy", &["build"], &[], &["install"]),
        ]
    }

    fn selected_ids(task_file: &str, names: &[&str], scope: Option<&str>) -> Result<Vec<String>> {
        let task_file = TaskFile::from_toml_str(task_file).unwrap();
        let names = SelectedNames::from_names(&task_file, names.iter().map(|name| (*name).to_owned()).collect());
        let selection = Selection::new(&task_file, &names.groups, &names.task_ids, scope)?;
        Ok(filter_tasks(tasks(), &selection)?
            .into_iter()
            .map(|task| task.id)
            .collect())
    }

    #[test]
    fn without_groups_selects_every_task() {
        let ungrouped = "[[tasks]]\nid = \"a\"\nlabel = \"A\"\ncommand = [\"true\"]";
        let task_file = TaskFile::from_toml_str(ungrouped).unwrap();
        let selection = Selection::new(&task_file, &[], &[], None).unwrap();
        let tasks = vec![task("a", &[], &[], &[]), task("b", &["a"], &[], &[])];
        let ids: Vec<String> = filter_tasks(tasks, &selection)
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(selection.label(None), "all");
    }

    #[test]
    fn plain_run_selects_ungrouped_tasks_and_the_default_group() {
        assert_eq!(
            selected_ids(TASK_FILE, &[], None).unwrap(),
            ["ungrouped", "unit-tests", "web-tests"]
        );
    }

    #[test]
    fn named_group_excludes_other_groups_and_pulls_in_prerequisites() {
        assert_eq!(selected_ids(TASK_FILE, &["install"], None).unwrap(), ["build", "copy"]);
        assert_eq!(
            selected_ids(TASK_FILE, &["test"], None).unwrap(),
            ["unit-tests", "web-tests"]
        );
    }

    #[test]
    fn groups_and_task_ids_combine() {
        assert_eq!(
            selected_ids(TASK_FILE, &["install", "ungrouped"], None).unwrap(),
            ["ungrouped", "build", "copy"]
        );
    }

    #[test]
    fn task_ids_pull_in_transitive_prerequisites_only() {
        let task_file = TaskFile::default();
        let selection = Selection::new(&task_file, &[], &["c".to_owned()], None).unwrap();
        let tasks = vec![
            task("a", &[], &[], &[]),
            task("b", &["a"], &[], &[]),
            task("c", &["b"], &[], &[]),
            task("d", &[], &[], &[]),
        ];
        let ids: Vec<String> = filter_tasks(tasks, &selection)
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn scope_filters_within_the_selected_groups() {
        assert_eq!(
            selected_ids(TASK_FILE, &[], Some("checks")).unwrap(),
            ["ungrouped", "unit-tests"]
        );
        assert_eq!(selected_ids(TASK_FILE, &["test"], Some("web")).unwrap(), ["web-tests"]);
    }

    #[test]
    fn unknown_name_lists_groups_and_tasks() {
        let error = selected_ids(TASK_FILE, &["deploy"], None).unwrap_err().to_string();
        assert!(
            error.starts_with("Unknown task or group 'deploy'. Groups: install, release, test. "),
            "{error}"
        );
        assert!(error.contains("Tasks for the current flags: build, copy"), "{error}");
    }

    #[test]
    fn labels_the_selection() {
        let task_file = TaskFile::from_toml_str(TASK_FILE).unwrap();
        let plain = Selection::new(&task_file, &[], &[], None).unwrap();
        assert_eq!(plain.label(None), "test");
        assert_eq!(plain.label(Some("checks")), "checks");
        let named = Selection::new(&task_file, &["install".to_owned()], &["ungrouped".to_owned()], None).unwrap();
        assert_eq!(named.label(None), "install, ungrouped");
    }

    #[test]
    fn resolves_scopes_with_includes() {
        let task_file = TaskFile::from_toml_str(TASK_FILE).unwrap();
        assert!(resolve_scope(&task_file, None).unwrap().is_none());
        assert!(resolve_scope(&task_file, Some("all")).unwrap().is_none());
        let checks = resolve_scope(&task_file, Some("checks")).unwrap().unwrap();
        assert!(checks.contains("rust"));
        let error = resolve_scope(&task_file, Some("backend")).unwrap_err();
        assert!(
            error.to_string().contains("Available: all, checks, rust, web"),
            "{error}"
        );
    }
}
