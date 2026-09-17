#include <stdio.h>

int main(void) {
    // Outer deadline is one second.
    // Five seconds leaves plenty of slack for thread scheduling on a loaded build machine
    // without allowing the test to pass when the deadline is not being enforced at all.
    // The helper retries until the budget runs out,
    // so a slow machine still reaches the deadline, and a fast one is not delayed.
    return 0;
}
