# cli-tools-rsbump

Bump Rust projects to the local Rust toolchain version.

Sets `rust-version` in `Cargo.toml` and pins `rust` Docker images to the matching version,
and can also upgrade dependencies, clean, verify, and commit the changes.

Provides the `rsbump` binary.
See the [`rsbump` section](../../README.md#rsbump) of the main README for usage.

```shell
cargo run --bin rsbump -- -h
cargo test -p cli-tools-rsbump
cargo install --locked --path crates/rsbump
```
