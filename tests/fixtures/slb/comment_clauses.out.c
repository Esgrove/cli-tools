#include <stdio.h>

// Outer deadline is one second.
// Five seconds leaves plenty of slack for thread scheduling on a loaded build machine
// without allowing the test to pass when the deadline is not being enforced at all.
// The helper retries until the budget runs out,
// so a slow machine still reaches the deadline, and a fast one is not delayed.

int main(void) {
    // number of retries so far
    int retries = 0;
    // The buffer is sized for the worst case frame, sixty four kilobytes, plus one byte for the terminator.
    char buffer[65537];
    // See https://example.com/protocol/spec for the full frame layout. The header is fixed size.
    retries++;
    return 0;
}
