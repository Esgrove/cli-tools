# cli-tools-dupefind

Find duplicate files.

Provides the `dupefind` binary.
The library exposes the core logic so other projects can reuse and extend it without the rest of `cli-tools`.
See the [`dupefind` section](../../README.md#dupefind) of the main README for usage.

```shell
cargo run --bin dupefind -- -h
cargo test -p cli-tools-dupefind
cargo install --locked --path crates/dupefind
```
