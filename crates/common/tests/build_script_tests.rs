//! Exercises release binary installation with small Cargo workspaces.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Result, ensure};

/// Create two binary targets. The second can be disabled without removing its manifest entry.
fn write_workspace(workspace: &Path, disable_second: bool) -> Result<()> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for script in ["build.sh", "common.sh"] {
        fs::copy(repository.join(script), workspace.join(script))?;
    }
    let package = workspace.join("crates/sample");
    fs::create_dir_all(&package)?;
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"crates/sample\"]\n",
    )?;
    let mut manifest = String::from(
        "[package]\nname = \"sample\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
         [features]\nsecond = []\n\
         [[bin]]\nname = \"first\"\npath = \"first.rs\"\n\
         [[bin]]\nname = \"second\"\npath = \"second.rs\"\n",
    );
    if disable_second {
        manifest.push_str("required-features = [\"second\"]\n");
    }
    fs::write(package.join("Cargo.toml"), manifest)?;
    for name in ["first", "second"] {
        fs::write(
            package.join(format!("{name}.rs")),
            format!("fn main() {{ println!(\"built {name}\"); }}\n"),
        )?;
    }
    Ok(())
}

/// Run the real build script against an isolated fixture without network access.
fn build(workspace: &Path, target: &str) -> std::io::Result<Output> {
    Command::new("bash")
        .arg(workspace.join("build.sh"))
        .current_dir(workspace)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("CARGO_BUILD_TARGET")
        .output()
}

/// Whether the build script's optional local test prerequisites are available.
fn has_build_tools() -> bool {
    let available = ["bash", "jq"].into_iter().all(cli_tools_common::command_exists);
    if !available {
        eprintln!("Skipping build script regression: bash and jq are required");
    }
    available
}

#[test]
fn redirected_build_installs_new_binaries_instead_of_stale_defaults() -> Result<()> {
    if !has_build_tools() {
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    write_workspace(workspace, false)?;
    let stale = workspace.join("target/release");
    fs::create_dir_all(&stale)?;
    for name in ["first", "second"] {
        fs::write(stale.join(name), b"stale artifact")?;
    }

    let result = build(workspace, "redirected target")?;
    ensure!(result.status.success(), "redirected build failed: {result:?}");
    for (name, version) in [("first", b"built first\n".as_slice()), ("second", b"built second\n")] {
        let installed = Command::new(workspace.join(name)).arg("--version").output()?;
        ensure!(
            installed.status.success() && installed.stdout == version,
            "unexpected installed version for {name}: {installed:?}"
        );
        ensure!(fs::read(stale.join(name))? == b"stale artifact", "stale {name} changed");
    }
    Ok(())
}

#[test]
fn missing_replacement_leaves_every_existing_binary_untouched() -> Result<()> {
    if !has_build_tools() {
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    write_workspace(workspace, true)?;
    let existing_binaries = [("first", b"existing first".as_slice()), ("second", b"existing second")];
    for (name, contents) in existing_binaries {
        fs::write(workspace.join(name), contents)?;
    }

    let result = build(workspace, "target")?;
    ensure!(
        !result.status.success(),
        "build succeeded without a replacement: {result:?}"
    );
    for (name, contents) in existing_binaries {
        let installed = fs::read(workspace.join(name));
        ensure!(
            installed.is_ok_and(|bytes| bytes == contents),
            "{name} changed: {result:?}"
        );
    }
    ensure!(
        String::from_utf8_lossy(&result.stdout).contains("Expected exactly one Cargo replacement for second"),
        "build failed for an unexpected reason: {result:?}"
    );
    Ok(())
}
