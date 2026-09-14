// Package header provides parsing helpers for the wire format; the caller owns the buffer.
package header

import "fmt"

// Parse reads the header from the input and returns the parsed struct, and it reports an error
// when the input is
// truncated.
func Parse(input string) (Header, error) {
	query := `SELECT * FROM t -- not a comment`
	size := 42 // default size
	if input == "" {
		return Header{}, fmt.Errorf("empty input") // early return
	}
	// A comment block that is already formatted correctly.
	// It stays exactly as it is.
	return Header{Query: query, Size: size}, nil
}
