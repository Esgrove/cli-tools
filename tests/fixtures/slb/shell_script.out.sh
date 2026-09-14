#!/usr/bin/env bash
# Build the project and run the tests. The script stops at the first failing command.
set -euo pipefail

# The heredoc below contains a hash character that is not a comment, so it must be left alone.
cat <<'NOTES' > notes.txt
# not a comment
value = 1 # still not a comment
NOTES

# default build directory
BUILD_DIR="build"
# shellcheck disable=SC2086
cargo build --release ${FLAGS:-}

# print the result
echo "done"
