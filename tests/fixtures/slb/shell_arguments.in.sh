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
main() {
    echo "$@"
}
