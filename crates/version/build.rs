//! Build script that records the version and build metadata of the binaries.
//!
//! It reads the git commit and branch, the build time, and the Rust compiler version,
//! and passes them to the library with `cargo:rustc-env` directives.
//! A value that cannot be read becomes an empty string,
//! so the build also works without git, for example from a git dependency checkout.

#[path = "src/format.rs"]
mod format;

use std::env;
use std::path::Path;
use std::process::Command;

/// Format of the build time, a date and a time without spaces so the version line splits cleanly on spaces.
const BUILD_TIME_FORMAT: &str = "%Y-%m-%d_%H:%M:%S";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/format.rs");
    emit_git_rerun_triggers();

    let build_time = jiff::Timestamp::now().strftime(BUILD_TIME_FORMAT).to_string();
    let git_commit = git_output(&["rev-parse", "--short=8", "HEAD"]);
    let git_branch = git_branch();
    let rust_version = rust_version();
    let package_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version_info = format::join_present(&[&package_version, &build_time, &git_branch, &git_commit]);

    println!("cargo:rustc-env=BUILD_TIME={build_time}");
    println!("cargo:rustc-env=GIT_BRANCH={git_branch}");
    println!("cargo:rustc-env=GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=RUST_VERSION={rust_version}");
    println!("cargo:rustc-env=VERSION_INFO={version_info}");
}

/// Rerun the build script when the checked out commit changes.
///
/// Watches `HEAD`, the loose ref of the current branch, and `packed-refs`.
/// A file that does not exist is skipped, since Cargo would rerun the script on every build for it.
fn emit_git_rerun_triggers() {
    let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
    let symbolic_ref = git_output(&["symbolic-ref", "--quiet", "HEAD"]);
    if !symbolic_ref.is_empty() {
        watched.push(symbolic_ref);
    }
    for name in watched {
        let path = git_output(&["rev-parse", "--path-format=absolute", "--git-path", &name]);
        if !path.is_empty() && Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// Current branch name, or an empty string for a detached `HEAD`.
fn git_branch() -> String {
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]);
    if branch == "HEAD" { String::new() } else { branch }
}

/// Rust compiler version line, such as "rustc 1.100.0 (4d91de4e4 2026-11-20)".
fn rust_version() -> String {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    command_output(Command::new(rustc).arg("--version"))
}

/// Trimmed standard output of a git command run in the package directory, or an empty string on failure.
fn git_output(arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    command.args(arguments);
    if let Ok(directory) = env::var("CARGO_MANIFEST_DIR") {
        command.current_dir(directory);
    }
    command_output(&mut command)
}

/// Trimmed standard output of a successful command, or an empty string on failure.
fn command_output(command: &mut Command) -> String {
    command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map_or_default(|text| text.trim().to_string())
}
