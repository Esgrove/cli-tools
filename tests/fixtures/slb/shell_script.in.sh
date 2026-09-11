#!/usr/bin/env bash
# Build the project and run the tests; the script stops at the first failing command.
set -euo pipefail

# The heredoc below contains a hash character that is not a comment, so it must be left alone.
cat <<'NOTES' > notes.txt
# not a comment
value = 1 # still not a comment
NOTES

BUILD_DIR="build" # default build directory
# shellcheck disable=SC2086
cargo build --release ${FLAGS:-}

echo "done" # print the result
