# cli-tools-qtorrent

Add torrents to qBittorrent with automatic file renaming, show torrent stats.

Provides the `qtorrent` binary.
See the [`qtorrent` section](../../README.md#qtorrent) of the main README for usage.

The library parses torrent files, filters their files, suggests output names, resolves tags,
and talks to the qBittorrent `WebUI` API.
It reads no command line arguments, prompts, or prints.
`TorrentAdder` adds a torrent and renames its content,
reporting every step after the add request as an `AddEvent` to an `AddObserver`,
which the binary implements to print it.
The binary's own dependencies are behind the default `cli` feature,
so depend on the crate with `default-features = false` to build only the library.

```shell
cargo run --bin qtorrent -- -h
cargo test -p cli-tools-qtorrent
cargo install --locked --path crates/qtorrent
```
