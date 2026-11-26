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

# A heredoc header can carry a trailing comment; the body still has to stay protected.
cat <<EOF > config.env # write runtime defaults
# Inside heredoc after trailing comment; semicolons stay verbatim.
# Wrapped lines inside the heredoc
# are never reflowed.
EOF

# Multiline double and single quoted strings may contain leading hashes that are literal data.
DOUBLE_QUOTED="header line
# inside double quotes; must not be formatted
# even when wrapped across lines
footer line"

SINGLE_QUOTED='header line
# inside single quotes; must not be formatted
# even when wrapped across lines
footer line'

BUILD_DIR="build" # default build directory
# shellcheck disable=SC2086
cargo build --release ${FLAGS:-}

# A semicolon immediately followed by a hash starts a real shell comment.

mkdir -p "$BUILD_DIR";# ensure output directory exists

echo "done" # print the result
