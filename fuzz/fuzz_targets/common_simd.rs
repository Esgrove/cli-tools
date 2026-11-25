//! Fuzz the SIMD byte scanning kernels against the plain iterator code they replace.
//!
//! The first byte gives the size of the byte set and the bytes after it form the set,
//! and the rest is the input, so sets and lengths vary across every block boundary.

#![no_main]

use cli_tools_common::simd;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [set_size, rest @ ..] = data else {
        return;
    };
    let (set, bytes) = rest.split_at(usize::from(*set_size % 5).min(rest.len()));

    let expected_position = bytes.iter().position(|byte| set.contains(byte));
    assert_eq!(simd::find_byte_of(bytes, set), expected_position);
    assert_eq!(simd::contains_byte_of(bytes, set), expected_position.is_some());

    let expected_adjacent = bytes.windows(2).any(|pair| pair.iter().all(|byte| set.contains(byte)));
    assert_eq!(simd::has_adjacent_bytes_of(bytes, set), expected_adjacent);

    if let Some(&needle) = set.first() {
        let expected_positions: Vec<usize> = bytes
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == needle).then_some(index))
            .collect();
        assert_eq!(simd::byte_positions(bytes, needle), expected_positions);
    }

    let text = String::from_utf8_lossy(bytes);
    assert_eq!(simd::count_chars(&text), text.chars().count());
    let expected_excluding = text.chars().filter(|character| *character != '.').count();
    assert_eq!(simd::count_chars_excluding(&text, b'.'), expected_excluding);
});
