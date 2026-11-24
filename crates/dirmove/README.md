# cli-tools-dirmove

Move files to matching directories.

Provides the `dirmove` binary.
The library exposes the core logic so other projects can reuse and extend it without the rest of `cli-tools`.
See the [`dirmove` section](../../README.md#dirmove) of the main README for usage.

```shell
cargo run --bin dirmove -- -h
cargo test -p cli-tools-dirmove
cargo install --locked --path crates/dirmove
```
