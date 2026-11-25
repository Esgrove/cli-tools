# cli-tools-fuzz

Coverage guided fuzz targets for the cli-tools libraries,
run with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) on the nightly toolchain.
The crate is its own workspace, so the stable workspace builds never compile it.

| Target             | What it checks                                                                                       |
|--------------------|------------------------------------------------------------------------------------------------------|
| `slb_format`       | Formatting never panics, is idempotent, leaves no fixable violation, and agrees with checking        |
| `dots_format_name` | Name formatting never panics and never produces repeated, leading, or trailing dots                  |
| `common_date`      | Date reordering never panics, and separator collapsing matches a plain loop and is idempotent        |
| `common_simd`      | Every SIMD byte scanning kernel returns what the plain iterator code returns                         |

The proptest suites in each crate check the same properties on every `cargo test`.
Fuzzing explores much further from the same starting point.

```shell
rustup toolchain install nightly
cargo install --locked cargo-fuzz

cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run slb_format corpus/slb_format ../crates/slb/tests/fixtures -- -max_total_time=300
cargo +nightly fuzz run common_simd -- -max_total_time=60
```

`./test.sh` at the repo root runs every target for 30 seconds after the tests, see `./test.sh -h`.

A crash is saved under `artifacts/<target>/`.
Replay it with `cargo +nightly fuzz run <target> artifacts/<target>/<file>`,
then turn the input into a unit test in the owning crate and fix it there.
The `corpus`, `artifacts`, and `coverage` directories are not committed.
