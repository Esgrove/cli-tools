#!/bin/bash
set -eo pipefail

# Commit the changed Cargo.toml and Cargo.lock files of the repository, and nothing else.
# The xtask bump and update groups use it to check that the files are clean before changing them,
# and to commit them once every step has passed.

# Import common functions
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=./common.sh
source "$DIR/common.sh"

USAGE="Usage: $0 [OPTIONS] [MESSAGE]

Commit the changed Cargo.toml and Cargo.lock files with the given message.
Other changes in the working tree, staged or not, stay out of the commit.
The message may contain {version}, which is replaced with the [workspace.package] version.

OPTIONS: All options are optional
    -c | --check
        Only check that no Cargo.toml or Cargo.lock file has uncommitted changes, and fail if one has.

    -h | --help
        Display these instructions.

    -v | --verbose
        Display commands being executed.
"

CHECK=false
MESSAGE=""

while [ $# -gt 0 ]; do
    case "$1" in
        -c | --check)
            CHECK=true
            ;;
        -h | --help)
            echo "$USAGE"
            exit 0
            ;;
        -v | --verbose)
            set -x
            ;;
        -*)
            print_usage_and_exit "Unknown option: $1"
            ;;
        *)
            [ -z "$MESSAGE" ] || print_usage_and_exit "Give the commit message as one argument"
            MESSAGE="$1"
            ;;
    esac
    shift
done

PATHSPECS=(':(glob)**/Cargo.toml' ':(glob)**/Cargo.lock')

cd "$REPO_ROOT"
CHANGED=$(git status --porcelain -- "${PATHSPECS[@]}")

if [ "$CHECK" = true ]; then
    if [ -n "$CHANGED" ]; then
        print_error_and_exit "Commit or stash these changes first, so they do not end up in the commit:
$CHANGED"
    fi
    print_green "No uncommitted Cargo.toml or Cargo.lock changes"
    exit 0
fi

[ -n "$MESSAGE" ] || print_usage_and_exit "Missing commit message"

if [ -z "$CHANGED" ]; then
    print_yellow "No Cargo.toml or Cargo.lock changes to commit"
    exit 0
fi

if [[ "$MESSAGE" == *"{version}"* ]]; then
    VERSION=$(workspace_version)
    MESSAGE="${MESSAGE//\{version\}/$VERSION}"
fi

# Committing with pathspecs commits only those paths, leaving anything else already staged in the index.
git commit --message "$MESSAGE" -- "${PATHSPECS[@]}"
print_green "Committed: $MESSAGE"
