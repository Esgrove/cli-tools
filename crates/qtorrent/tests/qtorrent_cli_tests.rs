//! Exit status of torrent add runs against a loopback-only `WebUI`.

#[path = "../src/test_server.rs"]
mod test_server;

use std::process::Output;

use cli_tools_common::test_support::TempTree;
use cli_tools_qtorrent::torrent::{Info, Torrent};
use serde_bytes::ByteBuf;
use test_server::{route, serve, serve_with};
use tokio::process::Command;

/// Run the CLI against the test server with no real user configuration or proxy.
async fn run(tree: &TempTree, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qtorrent"))
        .arg(tree.dir("torrents"))
        .args(arguments)
        .env("HOME", tree.dir("home"))
        .env("USERPROFILE", tree.join("home"))
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .await
        .expect("qtorrent should start and exit")
}

/// Write a distinct single-file torrent without registering a tracker.
fn write_torrent(tree: &TempTree, title: &str) {
    let mut torrent = Torrent::default();
    torrent.info = Info {
        name: Some(format!("{title}.mp4")),
        length: Some(1_000_000),
        piece_length: 262_144,
        pieces: ByteBuf::from(vec![0_u8; 20]),
        ..Info::default()
    };
    tree.file_with(
        &format!("torrents/{title}.torrent"),
        serde_bencode::to_bytes(&torrent).expect("test torrent should serialize"),
    );
}

/// Reply to the normal connection and add requests of the binary.
fn session_routes() -> Vec<test_server::Route> {
    vec![
        route("/api/v2/auth/login", 200, "Ok."),
        route("/api/v2/auth/logout", 200, ""),
        route("/api/v2/app/version", 200, "v5.0.0"),
        route("/api/v2/app/webapiVersion", 200, "2.11.2"),
        route("/api/v2/torrents/info", 200, "[]"),
        route("/api/v2/torrents/add", 415, ""),
    ]
}

/// Run in noninteractive mode with the supplied loopback port.
async fn run_connected(tree: &TempTree, port: u16) -> Output {
    run(
        tree,
        &[
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--username",
            "user",
            "--password",
            "pass",
            "--yes",
        ],
    )
    .await
}

#[tokio::test]
async fn a_rejected_add_exits_unsuccessfully() {
    let tree = TempTree::new();
    write_torrent(&tree, "Film");
    let port = serve(session_routes()).await;
    let output = run_connected(&tree, port).await;

    assert!(String::from_utf8_lossy(&output.stdout).contains("Failed:"));
    assert!(!output.status.success(), "{output:?}");
    assert!(tree.exists("torrents/Film.torrent"));
}

#[tokio::test]
async fn a_successful_add_keeps_a_mixed_run_successful() {
    let tree = TempTree::new();
    write_torrent(&tree, "Accepted");
    write_torrent(&tree, "Rejected");
    let port = serve_with(session_routes(), |request| {
        (request.starts_with("POST /api/v2/torrents/add ") && request.contains("Accepted.torrent"))
            .then(|| route("/api/v2/torrents/add", 200, "Ok."))
    })
    .await;
    let output = run_connected(&tree, port).await;

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Added:") && stdout.contains("Failed:"), "{stdout}");
}

#[tokio::test]
async fn an_offline_preview_with_only_malformed_inputs_exits_unsuccessfully() {
    let tree = TempTree::new();
    tree.file_with("torrents/broken.torrent", "not a torrent");

    let output = run(&tree, &["--offline", "--yes"]).await;

    assert!(String::from_utf8_lossy(&output.stdout).contains("No valid torrents to add"));
    assert!(!output.status.success(), "{output:?}");
}
