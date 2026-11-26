#!/bin/bash
set -eo pipefail

# Build release binaries and put them in the repository root.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Build all Rust binaries.

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
            print_error_and_exit "Unknown option: $1"
            ;;
    esac
    shift
done

if [ -z "$(command -v cargo)" ]; then
    print_error_and_exit "Cargo not found in path. Maybe install rustup?"
fi
if [ -z "$(command -v jq)" ]; then
    print_error_and_exit "jq not found in path. Install jq to read Cargo's build output."
fi

cd "$REPO_ROOT"

print_magenta "Building binaries..."
# Cargo reports the actual executable paths, including for redirected targets.
artifacts=$(cargo build --release --workspace --bins --message-format=json-render-diagnostics |
    jq -Rc 'fromjson? | objects | select(.reason == "compiler-artifact" and .executable != null and
        (.target.kind | index("bin"))) | {name: .target.name, executable}')

# Check all replacements before moving any existing binary.
executables=()
replacements=()
executable_names=$(get_rust_executable_names)
while IFS= read -r executable; do
    [ -n "$executable" ] || continue
    if ! replacement=$(jq -esj --arg name "${executable%.exe}" \
        'map(select(.name == $name) | .executable) | select(length == 1) | first' <<< "$artifacts"); then
        print_error_and_exit "Expected exactly one Cargo replacement for $executable."
    fi
    if [ "$BASH_PLATFORM" = "windows" ]; then
        replacement=$(cygpath -u -- "$replacement")
    fi
    if [ ! -f "$replacement" ] || [ ! -x "$replacement" ]; then
        print_error_and_exit "Replacement for $executable is not an executable file: $replacement"
    fi
    executables+=("$executable")
    replacements+=("$replacement")
done <<< "$executable_names"

for index in "${!executables[@]}"; do
    executable=${executables[index]}
    print_green "${executable%.exe}"
    mv -f -- "${replacements[index]}" "./$executable"
    ./"$executable" --version
    ./"$executable" -h || :
    echo ""
done
