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

`tests/slb_property_tests.rs` formats generated documents in every file kind with every rule combination.
It checks that formatting settles within two runs, leaves no fixable violation,
keeps every word, line ending, and byte order mark, and agrees with checking.
The default run uses a fixed seed.
Set `PROPTEST_RNG_SEED` and `PROPTEST_CASES` to explore more documents:

```shell
PROPTEST_RNG_SEED=1 PROPTEST_CASES=20000 cargo test -p cli-tools-slb --release --test slb_property_tests
```
