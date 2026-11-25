//! A local HTTP server with canned replies, standing in for the qBittorrent `WebUI` in tests.
//!
//! The library and the binary tests both include this file,
//! and each uses a different part of it.

#![allow(dead_code)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A canned reply for every request whose path starts with the given prefix.
pub struct Route {
    path: &'static str,
    status: u16,
    body: String,
}

impl Route {
    /// The path prefix this route answers.
    pub const fn path(&self) -> &'static str {
        self.path
    }
}

/// Build a route replying with the given status and body.
pub fn route(path: &'static str, status: u16, body: impl Into<String>) -> Route {
    Route {
        path,
        status,
        body: body.into(),
    }
}

/// Serve the routes on a local port for the rest of the test and return the port.
///
/// Routes are tried in order, and paths without a route get a 404 reply.
pub async fn serve(routes: Vec<Route>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind a local port");
    let port = listener.local_addr().expect("should have a local address").port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let request = read_request(&mut stream).await;
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            let (status, body) = routes
                .iter()
                .find(|route| path.starts_with(route.path))
                .map_or((404, ""), |route| (route.status, route.body.as_str()));
            let response = format!(
                "HTTP/1.1 {status} Reply\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.ok();
            stream.shutdown().await.ok();
        }
    });
    port
}

/// Read one full HTTP request, headers and body, so the connection closes cleanly.
async fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
        if let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(buffer.get(..header_end).unwrap_or_default()).to_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}
