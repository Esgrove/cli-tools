#!/bin/bash
set -eo pipefail

# Copy the release binaries built by `cargo build --release --workspace --bins` into the Cargo bin directory.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Copy every workspace binary from target/release into the Cargo bin directory:
\$CARGO_INSTALL_ROOT/bin, else \$CARGO_HOME/bin, else ~/.cargo/bin.
Build the binaries first with: cargo build --release --locked --workspace --bins

OPTIONS: All options are optional
    -h | --help
        Display these instructions.

    -v | --verbose
        Display commands being executed.
"

while [ $# -gt 0 ]; do
    case "$1" in
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        -v | --verbose)
            set -x
            ;;
        *)
            print_usage_and_exit "Unknown option: $1"
            ;;
    esac
    shift
done

# Print the directory cargo install would put binaries in.
cargo_bin_directory() {
    if [ -n "$CARGO_INSTALL_ROOT" ]; then
        echo "$CARGO_INSTALL_ROOT/bin"
    elif [ -n "$CARGO_HOME" ]; then
        echo "$CARGO_HOME/bin"
    else
        echo "$HOME/.cargo/bin"
    fi
}

# Replace the target with the source through a temporary file in the target directory,
# so a running binary keeps its old file and nothing sees a partially written one.
# Windows cannot replace a running executable, but it can rename it out of the way first.
replace_binary() {
    local source="$1"
    local target="$2"
    local temporary
    temporary="$(dirname "$target")/.$(basename "$target").$$.tmp"
    cp "$source" "$temporary"
    if mv -f "$temporary" "$target" 2> /dev/null; then
        return
    fi
    rm -f "$target.old" 2> /dev/null || true
    if mv -f "$target" "$target.old" && mv -f "$temporary" "$target"; then
        return
    fi
    rm -f "$temporary"
    print_error_and_exit "Failed to replace $target"
}

BIN_DIRECTORY=$(cargo_bin_directory)
mkdir -p "$BIN_DIRECTORY"
RELEASE_DIRECTORY="$REPO_ROOT/target/release"

count=0
for executable in $(get_rust_executable_names); do
    source_path="$RELEASE_DIRECTORY/$executable"
    if [ ! -f "$source_path" ]; then
        print_error_and_exit "Missing $source_path. Build it first with: cargo build --release --locked --workspace --bins"
    fi
    replace_binary "$source_path" "$BIN_DIRECTORY/$executable"
    count=$((count + 1))
done

print_green "Copied $count binaries to $BIN_DIRECTORY"
