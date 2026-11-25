# cli-tools-common

Shared helpers for the cli-tools binaries: path handling, formatting, printing, config, and shell completion.

Library crate shared by the `cli-tools` binaries.

The `test-support` feature adds `test_support::TempTree` for tests.
It builds nested files, directories, symlinks, and non-UTF-8 names in a temporary directory with a visible name,
and snapshots the tree as sorted relative paths so a test can compare the whole result at once.
Enable it from a dev-dependency:

```toml
[dev-dependencies]
cli-tools-common = { workspace = true, features = ["test-support"] }
```

```shell
cargo test -p cli-tools-common
```
