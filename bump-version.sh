#!/bin/bash
set -eo pipefail

# Bump the shared workspace version in Cargo.toml and refresh the workspace packages in both lock files.
# The xtask bump group runs this, choosing the version part from its major and minor project flags.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS]

Bump the [workspace.package] version in Cargo.toml with cargo set-version from cargo-edit,
then update the workspace packages in Cargo.lock and fuzz/Cargo.lock without touching other dependencies.
The xtask flags XTASK_FLAG_MAJOR, XTASK_FLAG_MINOR, and XTASK_FLAG_PATCH select the version part as well.

OPTIONS: All options are optional
    -h | --help
        Display these instructions.

    -m | --major
        Bump the major version.

    -n | --minor
        Bump the minor version.

    -p | --patch
        Bump the patch version. This is the default.

    -v | --verbose
        Display commands being executed.
"

LEVEL=""

# Select the version part to bump, failing when another part was already selected.
select_level() {
    if [ -n "$LEVEL" ] && [ "$LEVEL" != "$1" ]; then
        print_error_and_exit "Choose only one of major, minor, and patch, got: $LEVEL and $1"
    fi
    LEVEL="$1"
}

[ "${XTASK_FLAG_MAJOR:-0}" = 1 ] && select_level major
[ "${XTASK_FLAG_MINOR:-0}" = 1 ] && select_level minor
[ "${XTASK_FLAG_PATCH:-0}" = 1 ] && select_level patch

while [ $# -gt 0 ]; do
    case "$1" in
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        -m | --major)
            select_level major
            ;;
        -n | --minor)
            select_level minor
            ;;
        -p | --patch)
            select_level patch
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

LEVEL="${LEVEL:-patch}"

if ! cargo set-version --help > /dev/null 2>&1; then
    print_error_and_exit "cargo set-version not found. Install it with: cargo install --locked cargo-edit"
fi

cd "$REPO_ROOT"
OLD_VERSION=$(workspace_version)
cargo set-version --workspace --bump "$LEVEL"
NEW_VERSION=$(workspace_version)

# The fuzz crate is its own workspace, and its lock file records the versions of the workspace crates it uses.
# Updating only the workspace packages refreshes those versions and keeps every other dependency locked.
cargo update --workspace
cargo update --workspace --manifest-path fuzz/Cargo.toml
print_green "Bumped $LEVEL version from $OLD_VERSION to $NEW_VERSION"
