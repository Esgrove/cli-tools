# filefind-cli agent notes

The `filefind` command line search tool.
See `crates/filefind-common/AGENTS.md` for the database and configuration,
and the root `AGENTS.md` for the general workspace rules.

## Modules

- `main.rs` - CLI entry point and search logic
- `cli.rs` - Search execution, result display, and move dispatch
- `config.rs` - CLI configuration merging the user config with the CLI arguments
- `hyperlink.rs` - OSC 8 terminal hyperlinks for result paths
- `mover.rs` - Move planning, progress, database reconciliation, and abort handling
- `mover/transfer.rs` - File system rename, copy, and verification
- `utils.rs` - Match highlighting, path accessibility checks, and directory sizes

## Search

- **Pattern expansion**: dot separated patterns like "some.name" also search "some name" and "somename".
- **Glob patterns**: `*` and `?` wildcards.
- **Regex search**: full regex support with `-r`.
- **Exact matching**: `-e` disables pattern expansion.
- **Duplicates**: `filefind duplicates` finds files sharing the same case-insensitive stem,
  the file name without its extension, and shows them as groups.
  It scans the indexed files in two linear passes with an `indicatif` progress bar.
  The first pass counts stems, and the second collects the duplicate groups.
  It supports `--drive` filtering, `--limit` to cap the number of groups shown, and `--verbose` for stats.

## Moving files

- `--move <DIR>` (`-m`) moves the matching files to the directory after displaying the search results.
- `--force` (`-F`) overwrites existing files at the destination. Without it, conflicts are skipped.
- A move tries `fs::rename` first, which is atomic and instant on the same device.
  It falls back to a chunked copy, verify, and delete for cross device moves,
  such as from a local drive to a network share.
- The disk space check only counts cross device files, since same device renames need no free space.
  Volumes are compared with `get_volume_prefix()` from the shared library,
  by drive letter or by UNC server and share root.
- An `indicatif` progress bar shows progress.
  Ctrl+C aborts gracefully after the current file, and a second Ctrl+C quits immediately.
- The confirmation prompt shows the file count, total size, skipped files, and a warning in force mode.
- Files already in the destination directory or any of its subdirectories are counted,
  but neither moved nor reported as skips.
- Duplicate file names across the search results are skipped, and the first occurrence wins.
- The database is updated after each successful move.
  The MFT reference is cleared, since it may be invalid on the new volume.
- The `ctrlc` handler can only be registered once per process.
  The function documents this and returns an error on a second call.

## Terminal hyperlinks

- `--links <MODE>` (`-L`) sets the hyperlink mode: auto, always, or never.
- Result paths are wrapped in OSC 8 escape sequences, so terminals render them as links.
  Ctrl+Click in Windows Terminal and Warp, or Cmd+Click in iTerm2,
  opens the path with the application the operating system associates with it.
- `Hyperlinker` owns the mode and URI scheme and is stored on `CliConfig`.
  `wrap()` returns the text borrowed and unchanged when links are disabled,
  so display code can print its result unconditionally.
- Link text may already contain ANSI color codes.
  They nest inside the link and the sequences are zero width, so `info` format columns stay aligned.
- `auto` mode, the default, requires stdout to be a terminal that is known to support OSC 8.
  The known terminals are detected from `WT_SESSION`, `TERM_PROGRAM`, `KONSOLE_VERSION`, `DOMTERM`,
  kitty through `TERM`, or VTE 0.50 and newer.
  This keeps piped and redirected output free of escape sequences.
  `TerminalEnvironment` holds the relevant variables,
  so detection can be tested without changing the process environment.
- Paths become URIs.
  Backslashes become forward slashes and unsafe bytes are percent encoded.
  UNC paths become `file://server/share/...` and drive paths become `file:///C:/...`.
  Links use the mapped display path, so `path_mappings` are respected.
- Missing directories are never linked, since they no longer exist on disk.
- A `hyperlink_scheme` other than "file" emits `<scheme>://<path>`
  and requires a protocol handler registered with the operating system.
