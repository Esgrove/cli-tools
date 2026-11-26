#!/bin/bash
set -euo pipefail

# Exercise build.sh with temporary workspaces and a fake Cargo.
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

fixture=$(mktemp -d)
trap 'rm -rf -- "$fixture"' EXIT
unset MISSING_REPLACEMENT

suffix=""
if [ "$BASH_PLATFORM" = "windows" ]; then
    suffix=".exe"
fi

# Write a fixture binary that identifies the version installed by build.sh.
write_binary() {
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$2" > "$1"
    chmod +x "$1"
}

# Set up a workspace that lists two binaries and already has runnable copies of both.
create_workspace() {
    workspace="$fixture/$1"
    mkdir -p "$workspace/crates/sample"
    cp "$DIR/build.sh" "$DIR/common.sh" "$workspace/"
    cat > "$workspace/crates/sample/Cargo.toml" << 'MANIFEST'
[[bin]]
name = "first"

[[bin]]
name = "second"
MANIFEST
    write_binary "$workspace/first$suffix" "existing first"
    write_binary "$workspace/second$suffix" "existing second"
}

# Cargo writes replacements into its configured target directory and reports their full paths.
mkdir "$fixture/bin"
cat > "$fixture/bin/cargo" << 'CARGO'
#!/bin/bash
set -euo pipefail

[ "$1" = "build" ]
json=false
for argument in "$@"; do
    case "$argument" in
        --message-format=json*) json=true ;;
    esac
done

suffix=""
if [ "$BASH_PLATFORM" = "windows" ]; then
    suffix=".exe"
fi
release="$PWD/${CARGO_TARGET_DIR:-target}/release"
mkdir -p "$release"
for name in first second; do
    if [ "$name" = "${MISSING_REPLACEMENT:-}" ]; then
        continue
    fi
    artifact="$release/$name$suffix"
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "built $name" > "$artifact"
    chmod +x "$artifact"
    if [ "$json" = true ]; then
        jq -cn --arg executable "$artifact" --arg name "$name" \
            '{reason: "compiler-artifact", target: {kind: ["bin"], name: $name}, executable: $executable}'
    fi
done
CARGO
chmod +x "$fixture/bin/cargo"
export PATH="$fixture/bin:$PATH"

# A relative target directory with spaces must take precedence over stale artifacts.
test_redirected_build() {
    create_workspace redirected
    mkdir -p "$workspace/target/release"
    write_binary "$workspace/target/release/first$suffix" "stale first"
    write_binary "$workspace/target/release/second$suffix" "stale second"
    if ! CARGO_TARGET_DIR="redirected target" bash "$workspace/build.sh" > "$workspace/log" 2>&1; then
        printf 'FAIL: redirected build did not succeed\n' >&2
        cat "$workspace/log" >&2
        return 1
    fi
    if [ "$("$workspace/first$suffix" --version)" != "built first" ] ||
        [ "$("$workspace/second$suffix" --version)" != "built second" ]; then
        printf 'FAIL: redirected build did not install the new binaries\n' >&2
        return 1
    fi
}

# One missing replacement must fail the build step without changing any installed binary.
test_missing_replacement() {
    create_workspace missing
    if CARGO_TARGET_DIR='' MISSING_REPLACEMENT=second bash "$workspace/build.sh" > "$workspace/log" 2>&1; then
        printf 'FAIL: build unexpectedly succeeded without a replacement\n' >&2
        return 1
    fi
    if [ ! -x "$workspace/first$suffix" ] || [ ! -x "$workspace/second$suffix" ] ||
        [ "$("$workspace/first$suffix" --version)" != "existing first" ] ||
        [ "$("$workspace/second$suffix" --version)" != "existing second" ]; then
        printf 'FAIL: missing replacement changed an existing binary\n' >&2
        return 1
    fi
}

status=0
test_redirected_build || status=1
test_missing_replacement || status=1
if [ "$status" -eq 0 ]; then
    printf 'Build script regression tests passed.\n'
fi
exit "$status"
