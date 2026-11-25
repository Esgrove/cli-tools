//! End to end tests for the `vconvert` binary with fake `ffmpeg` and `ffprobe` scripts.
//!
//! The scripts are put first on the child process `PATH` and `HOME` points at a temporary directory,
//! so the tests never touch real media, the user config, or the user database.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Fake `ffprobe` that reports HEVC for files containing "hevc" and fails for empty files.
const FAKE_FFPROBE: &str = r#"#!/bin/sh
for argument; do last="$argument"; done
[ -s "$last" ] || { echo "empty or missing file" >&2; exit 1; }
codec=h264; grep -q hevc "$last" && codec=hevc
case "$*" in *csv=p=0*) exit 0;; esac
echo "codec_name=$codec"; echo "width=1920"; echo "height=1080"; echo "r_frame_rate=24/1"; echo "pix_fmt=yuv420p"
echo "bit_rate=99000000"; echo "duration=100.0"
"#;

/// Fake `ffmpeg` that exits successfully but writes an empty output file.
const FAKE_FFMPEG_EMPTY_OUTPUT: &str = r#"#!/bin/sh
for argument; do last="$argument"; done
: > "$last"
"#;

/// Fake `ffmpeg` that writes a complete HEVC output.
const FAKE_FFMPEG: &str = r#"#!/bin/sh
for argument; do last="$argument"; done
echo "hevc converted video data" > "$last"
"#;

/// Temporary home, bin, and video directories for one run.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    /// Create the sandbox with the given fake `ffmpeg` script.
    fn new(ffmpeg: &str) -> Self {
        // A visible name, since the file walker skips hidden directories such as the default ".tmp" prefix.
        let root = tempfile::Builder::new()
            .prefix("vconvert-test")
            .tempdir()
            .expect("temporary directory");
        for directory in ["home", "bin", "videos"] {
            fs::create_dir(root.path().join(directory)).expect("create directory");
        }
        let sandbox = Self { root };
        sandbox.write_script("ffprobe", FAKE_FFPROBE);
        sandbox.write_script("ffmpeg", ffmpeg);
        sandbox
    }

    fn videos(&self) -> PathBuf {
        self.root.path().join("videos")
    }

    fn write_script(&self, name: &str, contents: &str) {
        let path = self.root.path().join("bin").join(name);
        fs::write(&path, contents).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make script executable");
    }

    /// Run `vconvert` with the fake tools first on `PATH`.
    fn run(&self, arguments: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_vconvert"))
            .args(arguments)
            .arg(self.videos())
            .env("PATH", path)
            .env("HOME", self.root.path().join("home"))
            // The shared config lookup treats a set manifest directory as a test run and reads a fixture config.
            .env_remove("CARGO_MANIFEST_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .output()
            .expect("vconvert should run")
    }
}

/// Names of the files in a directory, sorted.
fn file_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("directory should be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_remux_with_an_empty_output_keeps_the_original() {
    let sandbox = Sandbox::new(FAKE_FFMPEG_EMPTY_OUTPUT);
    let original = sandbox.videos().join("show.mkv");
    fs::write(&original, "hevc video data").expect("write video");

    let output = sandbox.run(&["-d"]);

    assert!(
        original.is_file(),
        "the original was deleted, files left: {:?}\nstdout:\n{}\nstderr:\n{}",
        file_names(&sandbox.videos()),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&original).expect("read original"), "hevc video data");
}

#[test]
fn a_valid_remux_replaces_the_original() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    let original = sandbox.videos().join("show.mkv");
    fs::write(&original, "hevc video data").expect("write video");

    sandbox.run(&["-d"]);

    assert_eq!(file_names(&sandbox.videos()), vec!["show.x265.mp4"]);
}
