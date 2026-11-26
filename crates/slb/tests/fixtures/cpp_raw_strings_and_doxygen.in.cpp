#pragma once

#include <string>

/**
 * Storage engine options; each field uses a Doxygen post-member comment that must remain
 * attached to the field declaration rather than being moved above it.
 */
struct StorageOptions {
    int max_open_files = 256; ///< Maximum open descriptors; stays trailing on this member.
    int write_buffer_mb = 64; //!< Write buffer limit in MiB; stays trailing on this member.
    bool sync_writes = true; ///< Flush every commit to disk; stays trailing on this member.
};

// C++11 raw string literals with default and custom delimiters may hold comment-like lines;
// the scanner must track them so nothing inside a raw string is ever formatted.
inline const char* default_schema_sql() {
    return R"(
// This line lives inside a C++ raw string; semicolons stay untouched.
// Wrapped lines inside the raw string
// must remain verbatim.
CREATE TABLE entries (id INTEGER PRIMARY KEY, payload TEXT);
)";
}

inline const char* custom_delimiter_template() {
    return R"TEMPLATE(
// Inside a custom-delimited raw string; nested )" does not close it early.
// Semicolons and em dashes — stay verbatim here too.
const char* example = "value)";
)TEMPLATE";
}

// Mid-line block comments whose closing line begins with double slashes must not expose
// trailing code on the closing line to line-comment rewriting.
inline int evaluate_guarded(int input) {
    int result = input; /* start mid-line block comment
    // close mid-line block comment */ result += 10; return result;
}

// A trailing comment containing a block-comment closer after a preceding block comment must
// stay trailing rather than moving onto a line where it could interfere with block delimiters.
inline int glob_guard(int count) {
    /* preceding block comment
       that spans two lines
    */ int adjusted = count + 1; // match pattern /*/cache
    return adjusted; // return final count
}
