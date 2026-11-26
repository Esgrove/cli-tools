/**
 * Compute the checksum of the buffer and
 * return it; zero means empty.
 *
 * @param buffer the input buffer that
 *        is read fully.
 */
int checksum(const char* buffer) {
    int sum = 0; // running total
    const char* url = "http://x"; // keep
    return sum;
}

/**
 * Reads every frame from the socket until the peer closes the connection or the read
 * deadline elapses — whichever happens first — and returns the frames collected so far.
 *
 * The deadline is measured from the first call, not the last successful read, so a peer
 * that trickles data one byte at a time is still cut off eventually.
 */
std::vector<Frame> readFrames(Socket& socket, std::chrono::milliseconds deadline) {
    std::vector<Frame> frames; // collected frames
    return frames;
}
