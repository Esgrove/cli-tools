//! Command line parsing, project flag resolution, and output mode detection.
//!
//! The generic arguments are defined with clap derive.
//! Project flags come from the `[flags]` table of the task file and are added to the clap command at runtime,
//! so the task file is located before the command line is parsed.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction, ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use serde::Deserialize;

use crate::config::{TaskFile, TaskFileLocation};
use crate::process::shell_quote;

/// Generic command line arguments shared by every project.
/// Built-in short options that take a value, so the rest of a group of short flags after one of them is that value.
const SHORT_OPTIONS_WITH_VALUES: &[char] = &['j', 'S', 'C', 'O', 'K', 'T'];

#[derive(Debug, Parser)]
#[command(
    name = "xtask",
    version,
    about = "Run a project's tasks from xtask.toml in parallel",
    long_about = "Run a project's tasks from xtask.toml in parallel.\n\n\
        The task file is searched for in the current directory and its parents up to the git repository root. \
        Project specific flags declared in the task file are listed below the generic options."
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<XtaskCommand>,

    /// Task IDs to run with their prerequisites, instead of every task enabled by the flags
    #[arg(value_name = "TASK")]
    pub task_ids: Vec<String>,

    /// Maximum number of tasks to run concurrently [default: min(CPUs, 6)]
    #[arg(short = 'j', long, value_name = "N", value_parser = parse_positive_jobs)]
    pub jobs: Option<usize>,

    /// Stop starting new tasks after the first failure
    #[arg(short = 'f', long)]
    pub fail_fast: bool,

    /// Only run tasks tagged with this scope or a scope it includes
    #[arg(short = 'S', long, value_name = "SCOPE", global = true)]
    pub scope: Option<String>,

    /// Path to the task file [default: xtask.toml in the current directory or a parent]
    #[arg(short = 'C', long, value_name = "PATH", value_hint = clap::ValueHint::FilePath, global = true)]
    pub config: Option<PathBuf>,

    /// Output style: interactive progress, CI log stream, or plain lines
    #[arg(short = 'O', long, value_enum)]
    pub output: Option<OutputMode>,

    /// When to use ANSI colors
    #[arg(short = 'K', long, value_enum)]
    pub color: Option<ColorMode>,

    /// Live output lines shown per running task in rich mode, 0 for a spinner only
    #[arg(short = 'T', long, value_name = "N")]
    pub tail_lines: Option<usize>,

    /// Print the resolved commands without running them
    #[arg(short = 'd', long)]
    pub dryrun: bool,

    /// Print commands as they run
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

/// Subcommands for `xtask`.
#[derive(Debug, Subcommand)]
pub enum XtaskCommand {
    /// List the resolved tasks, dependencies, and resource locks without running them
    #[command(name = "list")]
    List {
        /// Task IDs to list with their prerequisites, instead of every task enabled by the flags
        #[arg(value_name = "TASK")]
        task_ids: Vec<String>,
    },

    /// Generate shell completion script
    #[command(name = "completion")]
    Completion {
        /// Shell to generate completion for
        #[arg(value_enum)]
        shell: Shell,

        /// Install completion script to the shell's completion directory
        #[arg(short = 'I', long)]
        install: bool,
    },
}

/// Requested output rendering mode (`--output`).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    /// Detect `ci`, `rich`, or `plain` automatically from environment variables and TTY state.
    #[default]
    Auto,
    /// Interactive `indicatif` progress bar with live per-task console previews.
    Rich,
    /// Timestamped linear CI output with 30-second heartbeats and optional GitHub annotations.
    Ci,
    /// Plain timestamped linear output with no cursor movement or heartbeats.
    Plain,
}

/// Concrete output mode after resolving [`OutputMode::Auto`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolvedOutputMode {
    /// Interactive `indicatif` progress UI.
    Rich,
    /// Linear CI stream with periodic `WAIT` heartbeats.
    Ci,
    /// Linear plain stream.
    Plain,
}

/// Requested ANSI color policy (`--color`).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Respect `NO_COLOR`, `CLICOLOR_FORCE`, `GITHUB_ACTIONS`, `TERM`, and TTY detection.
    #[default]
    Auto,
    /// Always emit ANSI color sequences.
    Always,
    /// Never emit ANSI color sequences.
    Never,
}

/// Parsed command line: the generic arguments plus the raw state of every project flag.
#[derive(Debug)]
pub struct ParsedCli {
    /// Generic arguments.
    pub args: Args,
    /// Project flags given on the command line, keyed by long name.
    pub flags: BTreeMap<String, bool>,
}

/// Resolved options for one run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunOptions {
    /// Explicit task IDs to run (plus their prerequisites).
    pub task_ids: Vec<String>,
    /// Maximum number of concurrent tasks.
    pub jobs: usize,
    /// Stop scheduling new tasks after the first failure.
    pub fail_fast: bool,
    /// List resolved tasks without running them.
    pub list_tasks: bool,
    /// Scope filter.
    pub scope: Option<String>,
    /// Print resolved commands without executing them.
    pub dry_run: bool,
    /// Echo executed commands.
    pub verbose: bool,
    /// Requested output mode.
    pub output: OutputMode,
    /// Requested ANSI color mode.
    pub color: ColorMode,
    /// Explicit live preview line count override.
    pub tail_lines: Option<usize>,
    /// Every project flag after applying `implies`, keyed by long name.
    pub flags: BTreeMap<String, bool>,
    /// Command that re-runs a single task with the same flags, without the task ID.
    pub rerun_prefix: String,
}

/// Defaults from the `[xtask]` section of the user config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// Default output mode.
    pub output: Option<OutputMode>,
    /// Default color mode.
    pub color: Option<ColorMode>,
    /// Default maximum number of concurrent tasks.
    pub jobs: Option<usize>,
    /// Default live preview line count.
    pub tail_lines: Option<usize>,
    /// Stop after the first failure by default.
    #[serde(default)]
    pub fail_fast: bool,
}

/// Snapshot of environment variables and stream TTY states used for UI auto-detection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvSnapshot {
    /// Value of `CI`, if set.
    pub ci: Option<String>,
    /// Value of `NO_COLOR`, if set.
    pub no_color: Option<String>,
    /// Value of `CLICOLOR_FORCE`, if set.
    pub clicolor_force: Option<String>,
    /// Value of `GITHUB_ACTIONS`, if set.
    pub github_actions: Option<String>,
    /// Value of `TERM`, if set.
    pub term: Option<String>,
    /// Whether standard output is an interactive terminal.
    pub stdout_is_tty: bool,
    /// Whether standard error is an interactive terminal.
    pub stderr_is_tty: bool,
}

/// Concrete output mode, color setting, and CI annotation policy resolved from options and environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedUiEnvironment {
    /// Concrete output renderer to use.
    pub mode: ResolvedOutputMode,
    /// Whether ANSI color styling is enabled.
    pub colors_enabled: bool,
    /// Whether GitHub Actions workflow commands (`::error`, `::group`) should be emitted.
    pub github_actions: bool,
}

/// Wrapper needed for parsing the user config section.
#[derive(Debug, Default, Deserialize)]
struct UserConfigFile {
    #[serde(default)]
    xtask: UserConfig,
}

impl ParsedCli {
    /// Combine the parsed arguments with task file settings and user defaults into [`RunOptions`].
    ///
    /// # Errors
    ///
    /// Returns an error if a flag's `requires` or `platforms` constraint is not met.
    pub fn into_options(self, task_file: &TaskFile, user_config: &UserConfig, current_os: &str) -> Result<RunOptions> {
        let flags = resolve_flags(task_file, &self.flags, current_os)?;
        let rerun_prefix = self.rerun_prefix(task_file);
        let args = self.args;
        let (task_ids, list_tasks) = match args.command {
            Some(XtaskCommand::List { task_ids }) => (task_ids, true),
            _ => (args.task_ids, false),
        };
        let task_ids = task_ids
            .into_iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        Ok(RunOptions {
            task_ids,
            jobs: args
                .jobs
                .or(task_file.settings.jobs)
                .or(user_config.jobs)
                .unwrap_or_else(default_jobs)
                .max(1),
            fail_fast: args.fail_fast || user_config.fail_fast,
            list_tasks,
            scope: args.scope,
            dry_run: args.dryrun,
            verbose: args.verbose,
            output: args.output.or(user_config.output).unwrap_or_default(),
            color: args.color.or(user_config.color).unwrap_or_default(),
            tail_lines: args.tail_lines.or(user_config.tail_lines),
            flags,
            rerun_prefix,
        })
    }

    /// Command that re-runs one task with the flags given on this command line.
    fn rerun_prefix(&self, task_file: &TaskFile) -> String {
        let mut parts = vec![
            task_file
                .settings
                .rerun_prefix
                .clone()
                .unwrap_or_else(|| "xtask".to_owned()),
        ];
        for (name, enabled) in &self.flags {
            if *enabled {
                parts.push(format!("--{name}"));
            }
        }
        if let Some(config) = &self.args.config {
            parts.push("--config".to_owned());
            parts.push(shell_quote(&config.to_string_lossy()));
        }
        parts.join(" ")
    }
}

impl RunOptions {
    /// Resolve the concrete UI mode, color policy, and CI integration for `env`.
    #[must_use]
    pub fn resolve_ui(&self, env: &EnvSnapshot) -> ResolvedUiEnvironment {
        let mode = self.output.resolve(env);
        let colors_enabled = self.color.resolve(mode, env);
        let github_actions = mode == ResolvedOutputMode::Ci && env.is_github_actions();
        ResolvedUiEnvironment {
            mode,
            colors_enabled,
            github_actions,
        }
    }

    /// Whether the named project flag is on.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.get(name).copied().unwrap_or(false)
    }
}

impl UserConfig {
    /// Read the `[xtask]` section from the shared `cli-tools` user config file.
    ///
    /// A missing file or section gives the defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load() -> Result<Self> {
        let Some(path) = cli_tools_common::config_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(path) {
            Ok(content) => Self::from_toml_str(&content)
                .with_context(|| format!("Invalid [xtask] section in user config {}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("Failed to read user config {}", path.display())),
        }
    }

    /// Parse the `[xtask]` section from a full user config TOML string.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid TOML or unknown keys in the section.
    pub fn from_toml_str(content: &str) -> Result<Self> {
        let file: UserConfigFile = toml::from_str(content)?;
        Ok(file.xtask)
    }
}

impl EnvSnapshot {
    /// Capture environment variables and terminal status from the current process.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            ci: std::env::var("CI").ok(),
            no_color: std::env::var("NO_COLOR").ok(),
            clicolor_force: std::env::var("CLICOLOR_FORCE").ok(),
            github_actions: std::env::var("GITHUB_ACTIONS").ok(),
            term: std::env::var("TERM").ok(),
            stdout_is_tty: io::stdout().is_terminal(),
            stderr_is_tty: io::stderr().is_terminal(),
        }
    }

    /// Whether the `CI` variable is set to a truthy value.
    #[must_use]
    pub fn is_ci(&self) -> bool {
        self.ci
            .as_deref()
            .is_some_and(|val| !val.is_empty() && val != "0" && !val.eq_ignore_ascii_case("false"))
    }

    fn has_no_color(&self) -> bool {
        self.no_color.as_deref().is_some_and(|val| !val.is_empty())
    }

    fn has_force_color(&self) -> bool {
        self.clicolor_force
            .as_deref()
            .is_some_and(|val| !val.is_empty() && val != "0" && !val.eq_ignore_ascii_case("false"))
    }

    fn is_github_actions(&self) -> bool {
        self.github_actions
            .as_deref()
            .is_some_and(|val| val.eq_ignore_ascii_case("true"))
    }

    fn is_dumb_term(&self) -> bool {
        self.term.as_deref().is_some_and(|val| val.eq_ignore_ascii_case("dumb"))
    }
}

impl OutputMode {
    /// Resolve `Auto` into a concrete [`ResolvedOutputMode`] using `env`.
    #[must_use]
    pub fn resolve(self, env: &EnvSnapshot) -> ResolvedOutputMode {
        match self {
            Self::Rich => ResolvedOutputMode::Rich,
            Self::Ci => ResolvedOutputMode::Ci,
            Self::Plain => ResolvedOutputMode::Plain,
            Self::Auto => {
                if env.is_ci() {
                    ResolvedOutputMode::Ci
                } else if env.stdout_is_tty && env.stderr_is_tty && !env.is_dumb_term() {
                    ResolvedOutputMode::Rich
                } else {
                    ResolvedOutputMode::Plain
                }
            }
        }
    }
}

impl ColorMode {
    /// Resolve `Auto` into whether ANSI colors should be emitted for `mode` and `env`.
    #[must_use]
    pub fn resolve(self, mode: ResolvedOutputMode, env: &EnvSnapshot) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => {
                if env.has_no_color() {
                    false
                } else if env.has_force_color() {
                    true
                } else if env.is_dumb_term() {
                    false
                } else {
                    match mode {
                        ResolvedOutputMode::Rich => env.stderr_is_tty || env.stdout_is_tty,
                        ResolvedOutputMode::Ci => env.is_github_actions() || env.stdout_is_tty,
                        ResolvedOutputMode::Plain => env.stdout_is_tty,
                    }
                }
            }
        }
    }
}

/// Build the clap command with the generic arguments and the project flags from `task_file`.
///
/// `location` is shown in the help text when a task file was found.
#[must_use]
pub fn build_command(task_file: Option<&TaskFile>, location: Option<&TaskFileLocation>) -> clap::Command {
    let mut command = Args::command();
    if let Some(task_file) = task_file {
        for (name, flag) in &task_file.flags {
            let mut arg = Arg::new(name.clone())
                .long(name.clone())
                .action(ArgAction::SetTrue)
                .help(flag.help.clone())
                .help_heading("Project flags")
                .global(true);
            if let Some(short) = flag.short {
                arg = arg.short(short);
            }
            command = command.arg(arg);
        }
    }
    if let Some(location) = location {
        command = command.after_help(format!("Task file: {}", location.path.display()));
    }
    command
}

/// Parse `argv` against [`build_command`].
///
/// # Errors
///
/// Returns the clap error for invalid arguments, `--help`, and `--version`.
pub fn parse_args<I, T>(
    argv: I,
    task_file: Option<&TaskFile>,
    location: Option<&TaskFileLocation>,
) -> clap::error::Result<ParsedCli>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut command = build_command(task_file, location);
    let arg_matches = command.try_get_matches_from_mut(argv)?;
    let generic_args = Args::from_arg_matches(&arg_matches)?;
    let flags = task_file.map_or_else(BTreeMap::new, |task_file| project_flag_values(task_file, &arg_matches));
    Ok(ParsedCli {
        args: generic_args,
        flags,
    })
}

/// Find the value of `-C`/`--config` in raw arguments before the full command line can be parsed.
///
/// Short flags can be grouped, as in `-dC path` or `-dCpath`.
/// Within a group, an option that takes a value takes the rest of the group,
/// or the next argument when it ends the group.
#[must_use]
pub fn find_config_argument(argv: &[OsString]) -> Option<PathBuf> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        if text == "--" {
            break;
        }
        if text == "--config" {
            return arguments.next().map(PathBuf::from);
        }
        if let Some(value) = text.strip_prefix("--config=") {
            return Some(PathBuf::from(value));
        }
        let Some(group) = text
            .strip_prefix('-')
            .filter(|group| !group.is_empty() && !group.starts_with('-'))
        else {
            continue;
        };
        for (index, character) in group.char_indices() {
            if !SHORT_OPTIONS_WITH_VALUES.contains(&character) {
                continue;
            }
            let rest = group.get(index + character.len_utf8()..).unwrap_or_default();
            if character == 'C' {
                return if rest.is_empty() {
                    arguments.next().map(PathBuf::from)
                } else {
                    Some(PathBuf::from(rest.strip_prefix('=').unwrap_or(rest)))
                };
            }
            if rest.is_empty() {
                arguments.next();
            }
            break;
        }
    }
    None
}

/// Apply `implies` and check `requires` and `platforms` for the given project flags.
///
/// # Errors
///
/// Returns an error if an enabled flag requires a flag that is off or is not supported on `current_os`.
pub fn resolve_flags(
    task_file: &TaskFile,
    given: &BTreeMap<String, bool>,
    current_os: &str,
) -> Result<BTreeMap<String, bool>> {
    let mut resolved: BTreeMap<String, bool> = task_file
        .flags
        .keys()
        .map(|name| (name.clone(), given.get(name).copied().unwrap_or(false)))
        .collect();

    let mut pending: Vec<String> = resolved
        .iter()
        .filter(|(_, enabled)| **enabled)
        .map(|(name, _)| name.clone())
        .collect();
    while let Some(name) = pending.pop() {
        if let Some(flag) = task_file.flags.get(&name) {
            for implied in &flag.implies {
                if let Some(enabled) = resolved.get_mut(implied)
                    && !*enabled
                {
                    *enabled = true;
                    pending.push(implied.clone());
                }
            }
        }
    }

    for (name, flag) in &task_file.flags {
        if !resolved.get(name).copied().unwrap_or(false) {
            continue;
        }
        for required in &flag.requires {
            if !resolved.get(required).copied().unwrap_or(false) {
                bail!("--{name} requires --{required}");
            }
        }
        if !flag.platforms.is_empty() && !flag.platforms.iter().any(|platform| platform == current_os) {
            bail!("--{name} is only supported on {}", flag.platforms.join(", "));
        }
    }
    Ok(resolved)
}

/// Compute a safe default concurrency cap so parallel builds do not exhaust host memory.
#[must_use]
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(1, 6)
}

/// Read every project flag's value from parsed matches.
fn project_flag_values(task_file: &TaskFile, matches: &ArgMatches) -> BTreeMap<String, bool> {
    task_file
        .flags
        .keys()
        .map(|name| (name.clone(), matches.get_flag(name)))
        .collect()
}

fn parse_positive_jobs(value: &str) -> Result<usize, String> {
    let parsed: usize = value
        .parse()
        .map_err(|_| format!("Invalid --jobs value '{value}': expected a positive integer."))?;
    if parsed == 0 {
        return Err("--jobs must be at least 1.".to_owned());
    }
    Ok(parsed)
}

#[cfg(test)]
mod test_helpers {
    use super::*;

    pub const MOBILE_PROJECT_FLAGS: &str = r#"
        [settings]
        rerun_prefix = "./test.sh"

        [flags.all]
        short = "a"
        help = "Run everything"
        implies = ["web"]

        [flags.web]
        short = "w"

        [flags.sign-in]
        short = "s"

        [flags.android]
        short = "n"
        requires = ["sign-in"]

        [flags.show-ui]
        short = "u"
        requires = ["all"]
        platforms = ["macos"]
    "#;

    pub fn task_file() -> TaskFile {
        TaskFile::from_toml_str(MOBILE_PROJECT_FLAGS).expect("flags should parse")
    }

    pub fn options(argv: &[&str], current_os: &str) -> Result<RunOptions> {
        let task_file = task_file();
        let parsed = parse_args(argv, Some(&task_file), None)?;
        parsed.into_options(&task_file, &UserConfig::default(), current_os)
    }
}

#[cfg(test)]
mod test_argument_parsing {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn parses_generic_defaults() {
        let parsed = parse_args(["xtask"], None, None).expect("should parse");
        assert_eq!(parsed.args.task_ids, Vec::<String>::new());
        assert!(parsed.args.command.is_none());
        assert!(parsed.flags.is_empty());
        assert!(!parsed.args.dryrun);
    }

    #[test]
    fn parses_combined_short_project_flags_and_task_ids() {
        let options = options(&["xtask", "-awu", "-j", "3", "fmt", "clippy"], "macos").expect("should parse");
        assert!(options.flag("all"));
        assert!(options.flag("web"));
        assert!(options.flag("show-ui"));
        assert!(!options.flag("sign-in"));
        assert_eq!(options.jobs, 3);
        assert_eq!(options.task_ids, ["fmt", "clippy"]);
    }

    #[test]
    fn parses_long_generic_options() {
        let parsed = parse_args(
            [
                "xtask",
                "--scope",
                "mobile",
                "--output",
                "plain",
                "--color",
                "never",
                "--tail-lines",
                "0",
                "--fail-fast",
                "--dryrun",
                "--verbose",
            ],
            None,
            None,
        )
        .expect("should parse");
        assert_eq!(parsed.args.scope.as_deref(), Some("mobile"));
        assert_eq!(parsed.args.output, Some(OutputMode::Plain));
        assert_eq!(parsed.args.color, Some(ColorMode::Never));
        assert_eq!(parsed.args.tail_lines, Some(0));
        assert!(parsed.args.fail_fast && parsed.args.dryrun && parsed.args.verbose);
    }

    #[test]
    fn rejects_unknown_project_flag_without_task_file() {
        assert!(parse_args(["xtask", "--all"], None, None).is_err());
        assert!(parse_args(["xtask", "--jobs", "0"], None, None).is_err());
    }

    #[test]
    fn parses_list_subcommand_with_project_flags_and_scope() {
        let trailing = options(&["xtask", "list", "-a", "--scope", "mobile", "fmt"], "macos").expect("should parse");
        assert!(trailing.list_tasks);
        assert!(trailing.flag("all"));
        assert_eq!(trailing.scope.as_deref(), Some("mobile"));
        assert_eq!(trailing.task_ids, ["fmt"]);

        let leading = options(&["xtask", "-a", "list"], "macos").expect("should parse");
        assert!(leading.list_tasks);
        assert!(leading.flag("all"));
        assert_eq!(leading.task_ids, [] as [std::string::String; 0]);
    }

    #[test]
    fn run_without_subcommand_does_not_list() {
        let options = options(&["xtask", "fmt"], "macos").expect("should parse");
        assert!(!options.list_tasks);
    }

    #[test]
    fn parses_completion_subcommand() {
        let parsed = parse_args(["xtask", "completion", "zsh", "-I"], None, None).expect("should parse");
        assert!(matches!(
            parsed.args.command,
            Some(XtaskCommand::Completion {
                shell: Shell::Zsh,
                install: true
            })
        ));
    }

    #[test]
    fn command_definition_is_valid_with_project_flags() {
        build_command(Some(&task_file()), None).debug_assert();
    }

    #[test]
    fn finds_config_argument_in_raw_arguments() {
        let argv = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            find_config_argument(&argv(&["xtask", "-a", "-C", "a.toml"])),
            Some(PathBuf::from("a.toml"))
        );
        assert_eq!(
            find_config_argument(&argv(&["xtask", "--config=b.toml"])),
            Some(PathBuf::from("b.toml"))
        );
        assert_eq!(
            find_config_argument(&argv(&["xtask", "-Cc.toml"])),
            Some(PathBuf::from("c.toml"))
        );
        assert_eq!(find_config_argument(&argv(&["xtask", "--", "-C", "x"])), None);
        assert_eq!(find_config_argument(&argv(&["xtask", "fmt"])), None);
    }

    #[test]
    fn short_options_with_values_match_the_command_definition() {
        let mut with_values: Vec<char> = build_command(None, None)
            .get_arguments()
            .filter(|argument| argument.get_action().takes_values())
            .filter_map(clap::Arg::get_short)
            .collect();
        with_values.sort_unstable();
        let mut listed = SHORT_OPTIONS_WITH_VALUES.to_vec();
        listed.sort_unstable();
        assert_eq!(with_values, listed);
    }

    #[test]
    fn finds_config_argument_grouped_with_other_short_flags() {
        let argv = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            find_config_argument(&argv(&["xtask", "-dC", "sub/alt.toml"])),
            Some(PathBuf::from("sub/alt.toml"))
        );
        assert_eq!(
            find_config_argument(&argv(&["xtask", "-vdCsub/alt.toml"])),
            Some(PathBuf::from("sub/alt.toml"))
        );
        // The value of another option is not a config path, even when it contains a C.
        assert_eq!(find_config_argument(&argv(&["xtask", "-SC", "fmt"])), None);
        assert_eq!(find_config_argument(&argv(&["xtask", "-j", "4", "fmt"])), None);
    }
}

#[cfg(test)]
mod test_flag_resolution {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn applies_implied_flags() {
        let options = options(&["xtask", "--all"], "linux").expect("should parse");
        assert!(options.flag("all") && options.flag("web"));
    }

    #[test]
    fn enforces_requires() {
        let error = options(&["xtask", "-n"], "linux").expect_err("android needs sign-in");
        assert_eq!(error.to_string(), "--android requires --sign-in");
        assert!(options(&["xtask", "-sn"], "linux").is_ok());
        assert!(options(&["xtask", "-u"], "macos").is_err());
    }

    #[test]
    fn enforces_platforms() {
        let error = options(&["xtask", "-au"], "linux").expect_err("show-ui is macOS only");
        assert_eq!(error.to_string(), "--show-ui is only supported on macos");
    }

    #[test]
    fn builds_rerun_prefix_from_given_flags_only() {
        let options = options(&["xtask", "-a", "-s"], "linux").expect("should parse");
        assert_eq!(options.rerun_prefix, "./test.sh --all --sign-in");

        let task_file = task_file();
        let parsed = parse_args(["xtask", "-C", "my tasks.toml"], Some(&task_file), None).expect("should parse");
        let options = parsed
            .into_options(&task_file, &UserConfig::default(), "linux")
            .expect("should resolve");
        assert_eq!(options.rerun_prefix, "./test.sh --config 'my tasks.toml'");
    }

    #[test]
    fn layers_cli_settings_and_user_defaults() {
        let task_file = TaskFile::from_toml_str("[settings]\njobs = 2").expect("should parse");
        let user =
            UserConfig::from_toml_str("[xtask]\njobs = 9\noutput = \"ci\"\nfail_fast = true").expect("should parse");
        let options = parse_args(["xtask"], Some(&task_file), None)
            .expect("should parse")
            .into_options(&task_file, &user, "linux")
            .expect("should resolve");
        assert_eq!(options.jobs, 2);
        assert_eq!(options.output, OutputMode::Ci);
        assert!(options.fail_fast);

        let options = parse_args(["xtask", "-j", "4", "-O", "plain"], Some(&task_file), None)
            .expect("should parse")
            .into_options(&task_file, &user, "linux")
            .expect("should resolve");
        assert_eq!(options.jobs, 4);
        assert_eq!(options.output, OutputMode::Plain);
    }

    #[test]
    fn user_config_ignores_other_sections_and_rejects_unknown_keys() {
        let user = UserConfig::from_toml_str("[slb]\nwidth = 100\n").expect("should parse");
        assert!(user.jobs.is_none());
        assert!(UserConfig::from_toml_str("[xtask]\nunknown = 1\n").is_err());
    }
}

#[cfg(test)]
mod test_ui_resolution {
    use super::*;

    #[test]
    fn auto_output_prefers_ci_then_rich_then_plain() {
        let ci = EnvSnapshot {
            ci: Some("true".to_owned()),
            ..EnvSnapshot::default()
        };
        assert_eq!(OutputMode::Auto.resolve(&ci), ResolvedOutputMode::Ci);
        let tty = EnvSnapshot {
            stdout_is_tty: true,
            stderr_is_tty: true,
            ..EnvSnapshot::default()
        };
        assert_eq!(OutputMode::Auto.resolve(&tty), ResolvedOutputMode::Rich);
        let dumb = EnvSnapshot {
            term: Some("dumb".to_owned()),
            ..tty
        };
        assert_eq!(OutputMode::Auto.resolve(&dumb), ResolvedOutputMode::Plain);
        assert_eq!(
            OutputMode::Auto.resolve(&EnvSnapshot::default()),
            ResolvedOutputMode::Plain
        );
    }

    #[test]
    fn auto_color_honors_no_color_and_force() {
        let tty = EnvSnapshot {
            stdout_is_tty: true,
            stderr_is_tty: true,
            ..EnvSnapshot::default()
        };
        assert!(ColorMode::Auto.resolve(ResolvedOutputMode::Rich, &tty));
        let no_color = EnvSnapshot {
            no_color: Some("1".to_owned()),
            ..tty
        };
        assert!(!ColorMode::Auto.resolve(ResolvedOutputMode::Rich, &no_color));
        let forced = EnvSnapshot {
            clicolor_force: Some("1".to_owned()),
            ..EnvSnapshot::default()
        };
        assert!(ColorMode::Auto.resolve(ResolvedOutputMode::Plain, &forced));
        let github = EnvSnapshot {
            github_actions: Some("true".to_owned()),
            ..EnvSnapshot::default()
        };
        assert!(ColorMode::Auto.resolve(ResolvedOutputMode::Ci, &github));
        assert!(ColorMode::Always.resolve(ResolvedOutputMode::Plain, &EnvSnapshot::default()));
        assert!(!ColorMode::Never.resolve(ResolvedOutputMode::Rich, &tty_snapshot()));
    }

    fn tty_snapshot() -> EnvSnapshot {
        EnvSnapshot {
            stdout_is_tty: true,
            stderr_is_tty: true,
            ..EnvSnapshot::default()
        }
    }

    #[test]
    fn github_annotations_only_in_ci_mode() {
        let github = EnvSnapshot {
            ci: Some("true".to_owned()),
            github_actions: Some("true".to_owned()),
            ..EnvSnapshot::default()
        };
        let options = RunOptions::default();
        assert!(options.resolve_ui(&github).github_actions);
        let plain = RunOptions {
            output: OutputMode::Plain,
            ..RunOptions::default()
        };
        assert!(!plain.resolve_ui(&github).github_actions);
    }
}
