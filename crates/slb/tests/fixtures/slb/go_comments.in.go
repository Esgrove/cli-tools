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

// Validate checks that every required field is set; it returns the first error it finds and
// stops, so a caller that wants every error at once must call it once per field on its own.
func Validate(header Header) error {
	if header.Size == 0 {
		return fmt.Errorf("size must be set") // size zero means unset
	}
	return nil
}

// See https://example.com/wire-format/spec for the byte layout this package implements.
const SpecURL = "https://example.com/wire-format/spec"
