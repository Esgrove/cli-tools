# cli-tools-dirmove

Move files to matching directories.

Provides the `dirmove` binary.
The library exposes the core logic so other projects can reuse and extend it without the rest of `cli-tools`.
It reads no command line arguments and prints nothing.
Long running steps report what they do through the `MoveObserver`, `MergeObserver`, `UnpackObserver`,
and `GroupingProgress` traits, which the binary implements to print progress.
See the [`dirmove` section](../../README.md#dirmove) of the main README for usage.

| Module          | Contents                                                                    |
| --------------- | --------------------------------------------------------------------------- |
| `roots`         | Input and output roots and the display paths shown relative to them         |
| `scanner`       | Collecting input files, input directories, and destination directories      |
| `name_filter`   | Ignored prefixes, ignored group names, and unwanted directories              |
| `matcher`       | Matching files and directories to destinations, and custom mappings         |
| `candidates`    | File name parts and the prefix candidates that file names share              |
| `grouping`      | Grouping files by shared name prefixes for create mode                      |
| `prefix_index`  | Sorted prefix index used to find prefix candidates                           |
| `unpack`        | Deleting unwanted directories and unpacking configured directory names      |
| `merge`         | Merging directory contents and finding prefixed directory merge pairs       |
| `mover`         | File moves that fall back to a verified copy across devices                 |

```shell
cargo run --bin dirmove -- -h
cargo test -p cli-tools-dirmove
cargo install --locked --path crates/dirmove
```
