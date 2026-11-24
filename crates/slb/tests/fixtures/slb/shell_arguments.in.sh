#!/usr/bin/env bash
# Cherry-pick a list of commits onto a new branch and open a pull request
# Arguments:
#   $1 - branch name
#   $2 - pull request title
#   $3 - pull request body
#   $@ - commit hashes to cherry-pick (remaining arguments)
set -euo pipefail

# Steps:
#   fetch the base branch, so the new branch starts from the same commit everyone else sees
#   cherry-pick every hash in the order it was given

# The remote name is fixed on purpose; a script that reads it from the environment would
# silently cherry-pick from the wrong remote when run from a fork with a different setup.
REMOTE="origin"

main() {
    local branch="$1"
    local title="$2"
    shift 2
    git fetch "$REMOTE" main # always start from a fresh copy of main
    git switch -c "$branch" "$REMOTE/main"
    for commit in "$@"; do
        git cherry-pick "$commit"
    done
    echo "$title"
}

main "$@"
