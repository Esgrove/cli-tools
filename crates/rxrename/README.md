# cli-tools-rxrename

Remove a trailing "_1" from file names.

Provides the `rxrename` binary.
See the [`rxrename` section](../../README.md#rxrename) of the main README for usage.
A name that is only the suffix, such as "_1.txt", is left as it is.

The end to end tests in `tests/` run the binary on nested temporary trees.

```shell
cargo run --bin rxrename -- -h
cargo test -p cli-tools-rxrename
cargo install --locked --path crates/rxrename
```
