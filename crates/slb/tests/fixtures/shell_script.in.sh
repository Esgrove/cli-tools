#!/usr/bin/env bash
# Build the project and run the tests; the script stops at the first failing command.
set -euo pipefail

# Environment:
#   SERVER_PORT       game port to expect (default 9339)
#   PUBLIC_HOST_IP    address handed to clients for the realtime connection
#                     (default 127.0.0.1)
#   EXAMPLE_HOME      server home; when unset, the paths fall back to ~/.example then
#                     /example, and fail if neither exists
#   SKIP_DOCKER       set to 1 to assume that the database and the cache are already up

# The heredoc below contains a hash character that is not a comment, so it must be left alone.
cat <<'NOTES' > notes.txt
# not a comment
value = 1 # still not a comment
NOTES

BUILD_DIR="build" # default build directory
# shellcheck disable=SC2086
cargo build --release ${FLAGS:-}

echo "done" # print the result
