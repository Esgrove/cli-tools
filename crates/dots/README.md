# cli-tools-dots

Rename files to use dot formatting.

Provides the `dots` binary.
The library exposes the core logic so other projects can reuse and extend it without the rest of `cli-tools`.
See the [`dots` section](../../README.md#dots) of the main README for usage.

```shell
cargo run --bin dots -- -h
cargo test -p cli-tools-dots
cargo install --locked --path crates/dots
```
