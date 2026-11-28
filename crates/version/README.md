# cli-tools-version

Compile-time version and build metadata for the cli-tools binaries.

`build.rs` records the git commit and branch, the UTC build time, and the Rust compiler version.
A value that cannot be read, for example without git, is an empty string.
The build script reruns only when the checked out commit changes,
so the build time is the time of the first build after that change.

Binaries pass `VERSION_INFO` to clap,
so `-V` prints `<name> <version> <build_time> <branch> <commit>`:

```rust
#[derive(clap::Parser)]
#[command(version = cli_tools_version::VERSION_INFO)]
struct Args {}
```

`build_details()` gives the same metadata as aligned, labelled lines for a user interface.

```shell
cargo test -p cli-tools-version
```
