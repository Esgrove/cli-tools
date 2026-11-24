# cli-tools-slb

Check and format prose with semantic line breaks.

Provides the `slb` binary.
The library exposes the core logic so other projects can reuse and extend it without the rest of `cli-tools`.
See the [`slb` section](../../README.md#slb) of the main README for usage.

```shell
cargo run --bin slb -- -h
cargo test -p cli-tools-slb
cargo install --locked --path crates/slb
```
