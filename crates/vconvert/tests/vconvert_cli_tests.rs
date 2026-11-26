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
codec=h264; /usr/bin/grep -q hevc "$last" && codec=hevc
case "$*" in *csv=p=0*) exit 0;; esac
echo "codec_name=$codec"; echo "width=1920"; echo "height=1080"; echo "r_frame_rate=24/1"; echo "pix_fmt=yuv420p"
echo "bit_rate=99000000"
duration=$(/usr/bin/awk -F= '/^duration=/ {print $2; exit}' "$last")
echo "duration=${duration:-100.0}"
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
        self.command(arguments).output().expect("vconvert should run")
    }

    /// Build a command whose media tools, config, and database are isolated from the host.
    fn command(&self, arguments: &[&str]) -> Command {
        let path = self.root.path().join("bin");
        let mut command = Command::new(env!("CARGO_BIN_EXE_vconvert"));
        command.args(arguments);
        if !arguments.contains(&"--from-db") {
            command.arg(self.videos());
        }
        command
            .env("PATH", path)
            .env("HOME", self.root.path().join("home"))
            // The shared config lookup treats a set manifest directory as a test run and reads a fixture config.
            .env_remove("CARGO_MANIFEST_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        command
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
fn all_conversion_attempts_fail_in_scan_and_database_modes() {
    let sandbox = Sandbox::new("#!/bin/sh\necho 'test conversion failure' >&2\nexit 1\n");
    let original = sandbox.videos().join("show.mkv");
    fs::write(&original, "h264 video data").unwrap();

    let skipped = sandbox.run(&["--skip-convert"]);
    assert!(skipped.status.success(), "{skipped:?}");
    let scan = sandbox.run(&["--delete"]);
    let replay = sandbox.run(&["--delete", "--from-db"]);
    for output in [scan, replay] {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("test conversion failure"),
            "{output:?}"
        );
        assert!(!output.status.success(), "{output:?}");
    }
    assert!(original.is_file());
}

#[test]
fn all_analysis_attempts_fail() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    fs::write(sandbox.videos().join("empty.mkv"), []).unwrap();

    let output = sandbox.run(&[]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("empty or missing file"));
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn a_failed_probe_does_not_fail_a_partially_successful_scan() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    fs::write(sandbox.videos().join("empty.mkv"), []).unwrap();
    fs::write(
        sandbox.videos().join("show.mkv"),
        "h264 original video payload larger than fake output",
    )
    .unwrap();

    let output = sandbox.run(&["--delete"]);
    assert!(output.status.success(), "{output:?}");
    assert!(sandbox.videos().join("show.x265.mp4").is_file());
}

#[test]
fn a_reconversion_still_larger_than_input_keeps_the_original() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    let original = sandbox.videos().join("tiny.mkv");
    fs::write(&original, "h264 tiny").unwrap();

    let output = sandbox.run(&["--delete"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(original.is_file(), "original must be kept when reconversion is larger");
    assert!(!sandbox.videos().join("tiny.x265.mp4").exists());
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
fn a_forced_remux_that_cannot_start_preserves_the_existing_output() {
    let sandbox = Sandbox::new("#!/no-such-vconvert-test-interpreter\n");
    let original = sandbox.videos().join("show.mkv");
    let prior_output = sandbox.videos().join("show.x265.mp4");
    fs::write(&original, b"hevc new source").unwrap();
    fs::write(&prior_output, b"hevc prior output").unwrap();

    sandbox.run(&["--force", "--delete"]);
    assert_eq!(fs::read(&original).unwrap(), b"hevc new source");
    assert_eq!(
        fs::read(&prior_output).ok().as_deref(),
        Some(b"hevc prior output".as_slice())
    );
    assert_eq!(file_names(&sandbox.videos()), ["show.mkv", "show.x265.mp4"]);
}

#[test]
fn a_valid_remux_replaces_the_original() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    let original = sandbox.videos().join("show.mkv");
    fs::write(&original, "hevc video data").expect("write video");

    sandbox.run(&["-d"]);

    assert_eq!(file_names(&sandbox.videos()), vec!["show.x265.mp4"]);
}

/// Fake tool fixture that represents subtitle tracks as `subtitle=LANG` lines in the video file.
const MOVIE_FFPROBE: &str = r#"#!/bin/sh
for argument; do last="$argument"; done
[ -s "$last" ] || exit 1
case "$*" in
    *'-select_streams s '*)
        /usr/bin/awk -F= '/^subtitle=/ {printf "%d,%s\n", NR, $2}' "$last"
        exit 0;;
    *csv=p=0*) exit 0;;
esac
codec=h264; /usr/bin/grep -q hevc "$last" && codec=hevc
printf 'codec_name=%s\nwidth=1920\nheight=1080\nr_frame_rate=24/1\npix_fmt=yuv420p\nbit_rate=99000000\nduration=100.0\n' "$codec"
"#;

/// Fake ffmpeg that only writes subtitle tracks actually mapped by the command.
const MOVIE_FFMPEG: &str = r#"#!/bin/sh
previous=; input=; internal=; external=0; languages=; disable=0
for argument; do
    case "$previous" in
        -i) [ -n "$input" ] || input=$argument;;
        -map) case "$argument" in
            0:s:m:language:*) internal="$internal ${argument##*:}";;
            [1-9]*:s*) external=$((external+1));;
        esac;;
        -metadata:s:s:*) languages="$languages ${argument#language=}";;
    esac
    [ "$argument" != -sn ] || disable=1
    previous=$argument
done
printf 'hevc\n' > "$previous"
if [ "$disable" = 0 ]; then
    for language in $internal; do
        /usr/bin/grep "^subtitle=$language$" "$input" >> "$previous"
    done
    set -- $languages
    while [ "$external" -gt 0 ]; do
        printf 'subtitle=%s\n' "${1-}" >> "$previous"
        [ "$#" = 0 ] || shift
        external=$((external-1))
    done
fi
"#;

#[test]
fn a_movie_sidecar_remains_embedded_after_a_second_run() {
    let sandbox = Sandbox::new(MOVIE_FFMPEG);
    sandbox.write_script("ffprobe", MOVIE_FFPROBE);
    let original = sandbox.videos().join("Film.mkv");
    let sidecar = sandbox.videos().join("Film.English.srt");
    let final_output = sandbox.videos().join("Film.x265.mkv");
    fs::write(&original, b"hevc original video").unwrap();
    fs::write(&sidecar, b"English subtitles").unwrap();

    sandbox.run(&["--movie", "--delete"]);
    assert_eq!(file_names(&sandbox.videos()), ["Film.x265.mkv"]);
    sandbox.run(&["--movie", "--delete"]);
    assert_eq!(fs::read_to_string(final_output).unwrap(), "hevc\nsubtitle=eng\n");
}

#[test]
fn a_movie_mux_that_omits_a_mapped_sidecar_keeps_the_inputs() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    sandbox.write_script("ffprobe", MOVIE_FFPROBE);
    let original = sandbox.videos().join("Film.mkv");
    let sidecar = sandbox.videos().join("Film.English.srt");
    fs::write(&original, b"hevc original video").unwrap();
    fs::write(&sidecar, b"English subtitles").unwrap();

    sandbox.run(&["--movie", "--delete"]);
    assert_eq!(file_names(&sandbox.videos()), ["Film.English.srt", "Film.mkv"]);
}

#[test]
fn non_movie_conversion_in_mixed_database_queue_keeps_its_sidecars() {
    let sandbox = Sandbox::new(MOVIE_FFMPEG);
    sandbox.write_script("ffprobe", MOVIE_FFPROBE);
    let movie = sandbox.videos().join("Film.mkv");
    let movie_sidecar = sandbox.videos().join("Film.English.srt");
    let conversion = sandbox.videos().join("Show.mkv");
    let english = sandbox.videos().join("Show.English.srt");
    let finnish = sandbox.videos().join("Show.Finnish.srt");
    fs::write(&movie, b"hevc original video").unwrap();
    fs::write(&conversion, b"h264 original video").unwrap();
    for sidecar in [&movie_sidecar, &english, &finnish] {
        fs::write(sidecar, b"unembedded subtitle data").unwrap();
    }
    let scan = sandbox.run(&["--movie", "--skip-remux", "--skip-convert"]);
    assert!(scan.status.success());

    let replay = sandbox.run(&["--from-db", "--delete"]);
    assert!(replay.status.success());
    assert!(sandbox.videos().join("Show.x265.mp4").exists());
    assert_eq!(
        fs::read(english).ok().as_deref(),
        Some(b"unembedded subtitle data".as_slice())
    );
    assert_eq!(
        fs::read(finnish).ok().as_deref(),
        Some(b"unembedded subtitle data".as_slice())
    );
    assert!(!movie_sidecar.exists());
    assert_eq!(
        fs::read_to_string(sandbox.videos().join("Film.x265.mkv")).unwrap(),
        "hevc\nsubtitle=eng\n"
    );
}

#[test]
fn movie_remux_keeps_compatible_embedded_subtitles() {
    let sandbox = Sandbox::new(MOVIE_FFMPEG);
    sandbox.write_script("ffprobe", MOVIE_FFPROBE);
    let original = sandbox.videos().join("Film.x265.m4v");
    let final_output = sandbox.videos().join("Film.x265.mp4");
    fs::write(&original, b"hevc original video\nsubtitle=eng\n").unwrap();

    sandbox.run(&["--all", "--movie", "--delete"]);
    assert!(final_output.exists());
    assert!(!original.exists());
    assert_eq!(fs::read_to_string(final_output).unwrap(), "hevc\nsubtitle=eng\n");
}

#[test]
fn opt_in_duplicate_deletion_accepts_a_closely_matching_duration_and_refuses_truncated() {
    let sandbox = Sandbox::new(FAKE_FFMPEG);
    let close_original = sandbox.videos().join("Interview.mkv");
    let close_output = sandbox.videos().join("Interview.x265.mp4");
    fs::write(&close_original, b"h264\nduration=3600\n").unwrap();
    fs::write(&close_output, b"hevc\nduration=3550\n").unwrap();

    let truncated_original = sandbox.videos().join("Truncated.mkv");
    let truncated_output = sandbox.videos().join("Truncated.x265.mp4");
    fs::write(&truncated_original, b"h264\nduration=3600\n").unwrap();
    fs::write(&truncated_output, b"hevc\nduration=3300\n").unwrap();

    sandbox.run(&["--delete-duplicates", "--delete"]);
    assert!(!close_original.exists());
    assert_eq!(fs::read(close_output).unwrap(), b"hevc\nduration=3550\n");
    assert!(
        truncated_original.exists(),
        "duplicate differing by more than 2% must not delete the original"
    );
}

#[test]
fn forced_abort_during_retry_leaves_pending_work_retryable() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let sandbox = Sandbox::new(
        r#"#!/bin/sh
for argument; do last=$argument; done
case $* in *hwupload_cuda*) exit 1;; esac
printf 'partial hevc header' > "$last"
echo 'retry wrote header'
exec /bin/sleep 15
"#,
    );
    let original = sandbox.videos().join("show.mkv");
    let final_output = sandbox.videos().join("show.x265.mp4");
    fs::write(&original, b"h264 source and plenty of video data").unwrap();
    let mut child = sandbox
        .command(&["--delete"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("retry wrote header") || line.contains("Received Ctrl+C") {
                let _ = sender.send(line);
            }
        }
    });
    let abort = || {
        Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    };
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
        "retry wrote header"
    );
    assert!(abort());
    assert!(
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .contains("Received Ctrl+C")
    );
    assert!(abort());
    child.wait().unwrap();
    reader.join().unwrap();

    assert!(original.exists());
    assert!(!final_output.exists(), "retry published an unvalidated header");

    sandbox.write_script("ffmpeg", FAKE_FFMPEG);
    let replay = sandbox.run(&["--from-db", "--delete"]);
    assert!(replay.status.success());
    assert!(!original.exists(), "pending conversion was not retried");
    assert_eq!(fs::read_to_string(final_output).unwrap(), "hevc converted video data\n");
}
