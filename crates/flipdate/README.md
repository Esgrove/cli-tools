# cli-tools-flipdate

Flip dates in file and directory names.

Provides the `flipdate` binary.
See the [`flipdate` section](../../README.md#flipdate) of the main README for usage.
With `--force`, an existing file is replaced,
but two files that map to the same new name in one run are never allowed to overwrite each other.

The end to end tests in `tests/` run the binary on nested temporary trees.

```shell
cargo run --bin flipdate -- -h
cargo test -p cli-tools-flipdate
cargo install --locked --path crates/flipdate
```
