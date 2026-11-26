/**
 * Compute the checksum of the buffer and return it.
 * Zero means empty.
 *
 * @param buffer the input buffer that is read fully.
 */
int checksum(const char* buffer) {
    // running total
    int sum = 0;
    // keep
    const char* url = "http://x";
    return sum;
}

/**
 * Reads every frame from the socket until the peer closes the connection or the read deadline elapses,
 * whichever happens first, and returns the frames collected so far.
 *
 * The deadline is measured from the first call, not the last successful read,
 * so a peer that trickles data one byte at a time is still cut off eventually.
 */
std::vector<Frame> readFrames(Socket& socket, std::chrono::milliseconds deadline) {
    // collected frames
    std::vector<Frame> frames;
    return frames;
}

/**
 * Configuration values for the parser.
 * Doxygen post-member doc comments stay trailing on the same line
 * so they continue documenting the field right before them.
 */
struct ParserConfig {
    int max_depth = 32; ///< Maximum recursion depth; stays on the member line.
    bool strict = false; //!< Reject ambiguous tokens; stays on the member line.
};

// Raw string literals and mid-line block comments must never be rewritten.
// A trailing comment containing a block-comment closer also stays in place rather than moving inside the block.
const char* sample_raw_strings(int value) {
    const char* plain = R"(
// Inside a C++ raw string; semicolons stay untouched.
// Wrapped lines stay verbatim.
)";
    const char* custom = R"SQL(
// Inside a custom-delimited raw string; stays verbatim.
const char* nested = ")";
)SQL";
    int acc = value; /* mid-line block comment
    // closes mid-line block */ acc += 1; return plain;
    /* block comment before a trailing comment that contains a closing marker
    inside block
    */ int guarded = acc; // handles glob /*/foo
    return custom ? plain : nullptr;
}
