# cli-tools-vconvert

Convert video files to HEVC in an MP4 container.

Provides the `vconvert` binary.
See the [`vconvert` section](../../README.md#vconvert) of the main README for usage.

The library gathers and classifies the video files, runs ffmpeg, and keeps the pending files in a `SQLite` database.
It reads no command line arguments and prints nothing.
`VideoConvert` takes its `ConvertOptions` and reports every step to a `ConvertObserver`,
which the binary implements to print the progress and write the run log.
A run stops after the current file when the abort flag passed to it is set.
The binary's own dependencies, such as clap, indicatif, and ctrlc, are behind the default `cli` feature,
so depend on the crate with `default-features = false` to build only the library.

```shell
cargo run --bin vconvert -- -h
cargo test -p cli-tools-vconvert
cargo install --locked --path crates/vconvert
```
