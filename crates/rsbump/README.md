# cli-tools-rsbump

Bump Rust projects to the local Rust toolchain version.

Sets `rust-version` in `Cargo.toml` and pins `rust` Docker images to the matching version,
and can also upgrade dependencies, clean, verify, and commit the changes.
If the active compiler is nightly or beta, `rsbump` uses the stable compiler installed through `rustup`.
It stops without editing files if the stable compiler is unavailable.

Provides the `rsbump` binary.
See the [`rsbump` section](../../README.md#rsbump) of the main README for usage.

The library holds the update steps and reads no command line arguments and prints nothing.
`bump_projects` processes projects in parallel and records every step of a project as a `ReportEntry`,
handing each finished `Report` to a `BumpObserver`, which the binary implements to print it.
The binary's own dependencies are behind the default `cli` feature,
so depend on the crate with `default-features = false` to build only the library.

```shell
cargo run --bin rsbump -- -h
cargo test -p cli-tools-rsbump
cargo install --locked --path crates/rsbump
```
