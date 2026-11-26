//! Subprocess execution and per-task log streaming helpers.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use console::strip_ansi_codes;

pub use cli_tools_common::utils::{command_exists, find_command};

/// Read buffer size for streaming child process output.
const STREAM_READ_BUFFER_BYTES: usize = 4096;
/// Longest partial line kept before it is flushed to the live preview.
const MAX_LINE_ACCUMULATOR_BYTES: usize = 16 * 1024;
/// Prefix of the environment variables the runner sets for every task command.
pub const RUNNER_ENV_PREFIX: &str = "XTASK_";
/// Package metadata variables `cargo run` sets for the runner itself.
///
/// They are removed from child processes so tools that check `CARGO_MANIFEST_DIR`,
/// such as `slb`, do not mistake a task for a Cargo test run.
const CARGO_RUN_ENV_VARS: &[&str] = &[
    "CARGO",
    "CARGO_BIN_NAME",
    "CARGO_CRATE_NAME",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_README",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_PRIMARY_PACKAGE",
];

/// Callback invoked with each sanitized non-empty output line streamed from a subprocess.
pub type OutputSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Specification for running an external command.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Positional arguments passed to the executable.
    pub args: Vec<String>,
    /// Working directory for the command.
    pub cwd: PathBuf,
    /// Environment variables to set or override.
    pub env_set: BTreeMap<String, String>,
    /// Environment variables to remove from the child environment.
    pub env_remove: Vec<String>,
    /// Command that combined stdout and stderr are piped through when it is available, empty for none.
    pub pipe_through: Vec<String>,
}

/// Captured stdout, stderr, and exit status from a short probe command.
#[derive(Debug)]
pub struct CommandOutput {
    /// Whether the process exited with status 0.
    pub success: bool,
    /// Captured UTF-8 stdout (lossy).
    pub stdout: String,
    /// Captured UTF-8 stderr (lossy).
    pub stderr: String,
}

/// Splits streamed bytes into sanitized lines for the live preview.
struct LineSplitter {
    buffer: Vec<u8>,
}

impl CommandSpec {
    /// Create a new command specification rooted in `cwd`.
    #[must_use]
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env_set: BTreeMap::new(),
            env_remove: Vec::new(),
            pipe_through: Vec::new(),
        }
    }

    /// Append multiple arguments to the command.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Append a single argument to the command.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Set an environment variable for this command.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_set.insert(key.into(), value.into());
        self
    }

    /// Remove an environment variable from the child process.
    #[must_use]
    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    /// Pipe combined output through another command while preserving the main exit status.
    #[must_use]
    pub fn with_pipe_through<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.pipe_through = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Format this command as a readable shell-like line for logs and `--dryrun`.
    ///
    /// The `XTASK_*` variables the runner sets for every command are left out to keep the line readable.
    #[must_use]
    pub fn display_line(&self) -> String {
        let mut parts = Vec::new();
        for (key, value) in &self.env_set {
            if key.starts_with(RUNNER_ENV_PREFIX) {
                continue;
            }
            parts.push(format!("{key}={}", shell_quote(value)));
        }
        parts.push(shell_quote(&self.program));
        for arg in &self.args {
            parts.push(shell_quote(arg));
        }
        if !self.pipe_through.is_empty() {
            let pipe: Vec<String> = self.pipe_through.iter().map(|arg| shell_quote(arg)).collect();
            parts.push(format!("2>&1 | {}", pipe.join(" ")));
        }
        parts.join(" ")
    }
}

impl LineSplitter {
    const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn feed_bytes<F: FnMut(&str)>(&mut self, chunk: &[u8], mut on_line: F) {
        for &byte in chunk {
            if byte == b'\n' || byte == b'\r' {
                self.flush_current(&mut on_line);
                continue;
            }
            if self.buffer.len() >= MAX_LINE_ACCUMULATOR_BYTES && !is_utf8_continuation(byte) {
                self.flush_current(&mut on_line);
            }
            self.buffer.push(byte);
        }
    }

    fn finish<F: FnMut(&str)>(&mut self, mut on_line: F) {
        self.flush_current(&mut on_line);
    }

    fn flush_current<F: FnMut(&str)>(&mut self, on_line: &mut F) {
        if self.buffer.is_empty() {
            return;
        }
        let decoded = String::from_utf8_lossy(&self.buffer);
        let stripped = strip_ansi_codes(&decoded);
        let cleaned = stripped.trim_end();
        if !cleaned.trim().is_empty() {
            on_line(cleaned);
        }
        self.buffer.clear();
    }
}

/// Ensure `name` is available on `PATH`, returning a descriptive error with `hint` if missing.
///
/// # Errors
///
/// Returns an error if the command is not found on `PATH`.
pub fn require_command(name: &str, hint: Option<&str>) -> Result<()> {
    if command_exists(name) {
        return Ok(());
    }
    if let Some(message) = hint {
        bail!("{message}");
    }
    bail!("Required command '{name}' not found.");
}

/// Append a formatted message line to a task's log file.
///
/// # Errors
///
/// Returns an error if the log file cannot be opened or written.
pub fn append_log_line(log_path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("Failed to open log file {}", log_path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("Failed to write log file {}", log_path.display()))
}

/// Execute `spec`, writing verbatim stdout/stderr to `log_path` while streaming sanitized lines to `sink`.
///
/// # Errors
///
/// Returns an error if the process fails to spawn, cannot open `log_path`, or exits non-zero.
pub fn run_logged(spec: &CommandSpec, log_path: &Path, sink: Option<&OutputSink>) -> Result<()> {
    let display = spec.display_line();
    append_log_line(log_path, &format!("$ {display}"))?;

    if let Some(pipe_program) = spec.pipe_through.first()
        && command_exists(pipe_program)
    {
        return run_piped(spec, log_path, sink);
    }

    let log_file = Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("Failed to open log file {}", log_path.display()))?,
    );

    let mut cmd = build_command(spec);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute '{}'", spec.program))?;
    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();

    let status = std::thread::scope(|scope| -> Result<std::process::ExitStatus> {
        let log_ref = &log_file;
        scope.spawn(move || {
            if let Some(reader) = child_stdout {
                tee_stream(reader, log_ref, sink);
            }
        });
        scope.spawn(move || {
            if let Some(reader) = child_stderr {
                tee_stream(reader, log_ref, sink);
            }
        });
        child
            .wait()
            .with_context(|| format!("Failed to wait for '{}'", spec.program))
    })?;

    if !status.success() {
        let code = status.code().map_or_else(|| "signal".to_owned(), |c| c.to_string());
        bail!("Command exited with status {code}: {display}");
    }
    Ok(())
}

/// Execute `spec` and capture stdout/stderr in memory.
///
/// # Errors
///
/// Returns an error if the command fails to spawn.
pub fn run_capture(spec: &CommandSpec) -> Result<CommandOutput> {
    let mut cmd = build_command(spec);
    let output = cmd
        .output()
        .with_context(|| format!("Failed to execute '{}'", spec.program))?;
    Ok(CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Read up to the last `max_lines` lines from `log_path`.
///
/// Tool output is not always valid UTF-8, so invalid bytes are replaced instead of losing the whole tail.
#[must_use]
pub fn read_log_tail(log_path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(bytes) = std::fs::read(log_path) else {
        return Vec::new();
    };
    let contents = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines
        .get(start..)
        .unwrap_or_default()
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

/// Strip ANSI sequences and carriage-return overwrites from a raw log line for terminal display.
#[must_use]
pub fn sanitize_log_line(raw: &str) -> String {
    let last_segment = raw
        .rsplit('\r')
        .find(|seg| !strip_ansi_codes(seg).trim().is_empty())
        .unwrap_or(raw);
    strip_ansi_codes(last_segment).trim_end().to_owned()
}

/// Build a `Command` from `spec` with its working directory and environment overrides applied.
fn build_command(spec: &CommandSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);
    for &key in CARGO_RUN_ENV_VARS {
        cmd.env_remove(key);
    }
    for key in &spec.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        cmd.env(key, value);
    }
    cmd
}

/// Build the `pipe_through` child command inheriting `spec`'s working directory and environment overrides.
fn build_pipe_command(pipe_program: &str, pipe_args: &[String], spec: &CommandSpec) -> Command {
    let mut pipe_cmd = Command::new(pipe_program);
    pipe_cmd
        .args(pipe_args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &key in CARGO_RUN_ENV_VARS {
        pipe_cmd.env_remove(key);
    }
    for key in &spec.env_remove {
        pipe_cmd.env_remove(key);
    }
    for (key, value) in &spec.env_set {
        pipe_cmd.env(key, value);
    }
    pipe_cmd
}

/// Run `spec` with combined output piped through `spec.pipe_through`, keeping the main command's exit status.
fn run_piped(spec: &CommandSpec, log_path: &Path, sink: Option<&OutputSink>) -> Result<()> {
    let log_file = Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("Failed to open log file {}", log_path.display()))?,
    );

    let Some((pipe_program, pipe_args)) = spec.pipe_through.split_first() else {
        bail!("Pipe command is empty");
    };
    let mut pipe_child = build_pipe_command(pipe_program, pipe_args, spec)
        .spawn()
        .with_context(|| format!("Failed to spawn '{pipe_program}'"))?;

    let Some(pipe_stdin) = pipe_child.stdin.take() else {
        let _ = pipe_child.kill();
        let _ = pipe_child.wait();
        bail!("Standard input of '{pipe_program}' is unavailable");
    };
    let pipe_stdout = pipe_child.stdout.take();
    let pipe_stderr = pipe_child.stderr.take();

    let mut main_cmd = build_command(spec);
    main_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match main_cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(pipe_stdin);
            let _ = pipe_child.kill();
            let _ = pipe_child.wait();
            return Err(error).with_context(|| format!("Failed to spawn '{}'", spec.program));
        }
    };

    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();

    let (main_status, pipe_status) = std::thread::scope(
        |scope| -> Result<(std::process::ExitStatus, std::process::ExitStatus)> {
            let log_ref = &log_file;
            scope.spawn(move || {
                if let Some(reader) = pipe_stdout {
                    tee_stream(reader, log_ref, sink);
                }
            });
            scope.spawn(move || {
                if let Some(reader) = pipe_stderr {
                    tee_stream(reader, log_ref, sink);
                }
            });

            let stdin_pipe = Arc::new(Mutex::new(pipe_stdin));
            let stdin_out = Arc::clone(&stdin_pipe);
            let out_handle = scope.spawn(move || {
                if let Some(reader) = child_stdout {
                    forward_stream(reader, &stdin_out);
                }
            });
            let stdin_err = Arc::clone(&stdin_pipe);
            let err_handle = scope.spawn(move || {
                if let Some(reader) = child_stderr {
                    forward_stream(reader, &stdin_err);
                }
            });
            drop(stdin_pipe);

            let status = child
                .wait()
                .with_context(|| format!("Failed to wait for '{}'", spec.program))?;
            let _ = out_handle.join();
            let _ = err_handle.join();
            let pipe_status = pipe_child
                .wait()
                .with_context(|| format!("Failed to wait for '{pipe_program}'"))?;
            Ok((status, pipe_status))
        },
    )?;

    if !main_status.success() {
        let code = main_status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        bail!("Command exited with status {code}: {}", spec.display_line());
    }
    if !pipe_status.success() {
        let code = pipe_status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        bail!(
            "Pipe command '{pipe_program}' exited with status {code}: {}",
            spec.display_line()
        );
    }
    Ok(())
}

/// Read a child output stream into the log file and forward complete lines to `sink`.
fn tee_stream<R: io::Read>(mut reader: R, log_file: &Mutex<File>, sink: Option<&OutputSink>) {
    let mut buf = [0_u8; STREAM_READ_BUFFER_BYTES];
    let mut splitter = LineSplitter::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(bytes_read) => {
                if let Some(chunk) = buf.get(..bytes_read) {
                    if let Ok(mut guard) = log_file.lock() {
                        let _ = guard.write_all(chunk);
                    }
                    if let Some(callback) = sink {
                        splitter.feed_bytes(chunk, |line| callback(line));
                    }
                }
            }
        }
    }
    if let Some(callback) = sink {
        splitter.finish(|line| callback(line));
    }
}

/// Copy all bytes from `reader` into a shared writer until EOF or write error.
fn forward_stream<R: io::Read, W: Write>(mut reader: R, sink: &Mutex<W>) {
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(bytes_read) => {
                if let Some(chunk) = buf.get(..bytes_read)
                    && let Ok(mut guard) = sink.lock()
                    && guard.write_all(chunk).is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Return whether `byte` is a UTF-8 continuation byte (`10xxxxxx`).
const fn is_utf8_continuation(byte: u8) -> bool {
    (byte & 0b1100_0000) == 0b1000_0000
}

/// Quote a value for display in a shell-like command line when it contains special characters.
#[must_use]
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b':' | b'=' | b'@' | b'+'))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod test_process {
    use super::*;

    #[test]
    fn line_splitter_handles_carriage_returns_ansi_and_split_utf8() {
        let mut splitter = LineSplitter::new();
        let mut lines = Vec::new();

        // Feed progress updates separated by \r, ANSI colors, blank lines,
        // and a split 3-byte UTF-8 '›' (E2 80 BA).
        splitter.feed_bytes(b"\x1b[32mDownloading\x1b[0m 10%\rDownloading 50%\r\n   \n", |line| {
            lines.push(line.to_owned());
        });
        splitter.feed_bytes(&[b' ', b' ', 0xE2, 0x80], |line| lines.push(line.to_owned()));
        splitter.feed_bytes(&[0xBA, b' ', b'd', b'o', b'n', b'e', b' ', b' '], |line| {
            lines.push(line.to_owned());
        });
        splitter.finish(|line| lines.push(line.to_owned()));

        assert_eq!(lines, vec!["Downloading 10%", "Downloading 50%", "  › done"]);
    }

    #[test]
    fn display_line_hides_runner_variables_and_quotes_values() {
        let spec = CommandSpec::new("tool", "/tmp")
            .args(["a b", "c"])
            .env("XTASK_ROOT", "/project")
            .env("MODE", "fast mode")
            .with_pipe_through(["xcbeautify"]);
        assert_eq!(spec.display_line(), "MODE='fast mode' tool 'a b' c 2>&1 | xcbeautify");
    }

    #[test]
    fn sanitize_log_line_keeps_latest_carriage_return_segment_and_strips_ansi() {
        assert_eq!(
            sanitize_log_line("step 1/4\r\x1b[1;31merror: boom\x1b[0m   \r"),
            "error: boom"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_logged_preserves_raw_file_and_streams_clean_lines() {
        let temp_dir = std::env::temp_dir().join(format!("cli-tools-xtask-proc-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let log_path = temp_dir.join("stream.log");

        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_sink = Arc::clone(&captured);
        let sink: OutputSink = Arc::new(move |line: &str| {
            captured_sink.lock().unwrap().push(line.to_owned());
        });

        let spec = CommandSpec::new("sh", &temp_dir).args([
            "-c",
            "printf '\\033[36mout-1\\033[0m\\rout-2\\n'; printf '\\033[31merr-1\\033[0m\\n' >&2",
        ]);
        run_logged(&spec, &log_path, Some(&sink)).unwrap();

        let streamed = captured.lock().unwrap().clone();
        assert!(streamed.contains(&"out-1".to_owned()));
        assert!(streamed.contains(&"out-2".to_owned()));
        assert!(streamed.contains(&"err-1".to_owned()));

        let raw_log = std::fs::read_to_string(&log_path).unwrap();
        assert!(raw_log.starts_with("$ sh -c "));
        assert!(raw_log.contains("\x1b[36mout-1\x1b[0m\rout-2\n"));
        assert!(raw_log.contains("\x1b[31merr-1\x1b[0m\n"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[cfg(unix)]
    #[test]
    fn build_command_strips_cargo_run_env_vars() {
        let probe = CommandSpec::new("sh", std::env::temp_dir())
            .args(["-c", "printf '%s|%s' \"$CARGO_MANIFEST_DIR\" \"$CARGO_PKG_NAME\""]);
        let output = run_capture(&probe).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, "|");
    }
}

#[cfg(test)]
mod test_read_log_tail {
    use super::*;

    #[test]
    fn a_log_with_invalid_utf8_still_gives_its_last_lines() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("task.log");
        std::fs::write(&path, b"first\nbad \xff byte\nlast line\n").expect("write log");

        let tail = read_log_tail(&path, 2);

        assert_eq!(tail, vec!["bad \u{fffd} byte".to_string(), "last line".to_string()]);
    }
}
