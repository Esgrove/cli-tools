//! Portable SIMD byte scanning kernels built on `fearless_simd`.
//!
//! Each public function picks the best SIMD level available at runtime and runs a kernel generic over [`Simd`].
//! The kernels classify a vector of bytes at a time and turn the lane masks into bitmasks,
//! so counting and searching become bit operations.
//! Input shorter than a 128-bit vector is classified byte by byte, because loading a vector costs more there.
//! The last block of longer input overlaps the one before it, so no kernel needs a scalar tail loop.
//!
//! A [`ByteSet`] is built once, usually as a constant,
//! and classifies a whole vector with two table lookups whatever the size of the set.
//! The level is fixed at compile time when the build target already has AVX2 or NEON,
//! so the dispatch folds away and the kernels inline into their callers.

use std::ops::ControlFlow;
use std::sync::LazyLock;

use fearless_simd::{Level, Simd, dispatch, prelude::*, u8x16};
use fearless_simd_macros::simd;

/// SIMD level the public functions run, detected once per process,
/// for builds whose target does not already guarantee AVX2 or NEON.
static LEVEL: LazyLock<Level> = LazyLock::new(Level::new);

/// A class of bytes the kernels test both as whole vectors of any width and as single bytes.
trait ByteClass {
    /// Lanes of the block holding a byte of the class.
    fn vector<S: Simd, V: SimdInt<S, Element = u8>>(&self, simd: S, block: V) -> V::Mask;

    /// Bitmask of the bytes in the class, for input shorter than 64 bytes.
    fn scalar_bits(&self, bytes: &[u8]) -> u64;
}

/// A set of bytes precomputed for classifying a whole vector with two table lookups.
///
/// Each high nibble in the set gets its own bit,
/// `high` maps a high nibble to its bit,
/// and `low` maps a low nibble to the bits of every high nibble it pairs with in the set.
/// A byte belongs to the set when `low[byte & 15] & high[byte >> 4]` is not zero.
/// That is exact for sets with at most eight distinct high nibbles, which covers every ASCII set.
/// `table` answers the same question one byte at a time for input shorter than a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSet {
    /// Bits of the high nibbles each low nibble pairs with.
    low: [u8; 16],
    /// Bit assigned to each high nibble, zero when the set has no byte with it.
    high: [u8; 16],
    /// Membership bitmap of all 256 byte values.
    table: [u64; 4],
}

/// One bit per input byte, set where the byte belongs to a [`ByteSet`].
///
/// Bit `i % 64` of word `i / 64` stands for byte `i`.
/// Built in one SIMD pass, so code that searches the same text many times walks bits instead of bytes,
/// and finding the next match from any position is a mask and `trailing_zeros`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteBitmap {
    /// The bits, 64 input bytes to a word.
    words: Vec<u64>,
}

/// UTF-8 continuation bytes and one extra byte.
struct ContinuationOr(u8);

/// Any byte of a set.
struct AnyOf<'a>(&'a [u8]);

/// One exact byte.
struct Exactly(u8);

impl ByteSet {
    /// Build the set from its bytes.
    ///
    /// # Panics
    /// Panics, at compile time for a constant, when the bytes have more than eight distinct high nibbles.
    #[must_use]
    pub const fn new(bytes: &[u8]) -> Self {
        let mut low: u128 = 0;
        let mut high: u128 = 0;
        let mut next_bit: u32 = 0;
        let mut table = [0u64; 4];
        let mut rest = bytes;
        while let Some((&byte, tail)) = rest.split_first() {
            let high_shift = (byte >> 4) as u32 * 8;
            let mut bit = ((high >> high_shift) & 0xFF) as u8;
            if bit == 0 {
                assert!(next_bit < 8, "a byte set holds at most eight distinct high nibbles");
                bit = 1 << next_bit;
                next_bit += 1;
                high |= (bit as u128) << high_shift;
            }
            low |= (bit as u128) << ((byte & 15) as u32 * 8);
            table = with_table_bit(table, byte);
            rest = tail;
        }
        Self {
            low: low.to_le_bytes(),
            high: high.to_le_bytes(),
            table,
        }
    }

    /// Whether the byte is in the set.
    #[must_use]
    #[inline]
    pub const fn contains(&self, byte: u8) -> bool {
        let [first, second, third, fourth] = self.table;
        let word = match byte >> 6 {
            0 => first,
            1 => second,
            2 => third,
            _ => fourth,
        };
        (word >> (byte & 63)) & 1 != 0
    }
}

impl ByteBitmap {
    /// Build the bitmap of the bytes in the set.
    #[must_use]
    pub fn new(bytes: &[u8], set: &ByteSet) -> Self {
        let mut bitmap = Self::default();
        bitmap.fill(bytes, set);
        bitmap
    }

    /// Rebuild the bitmap for new input, reusing the allocation.
    pub fn fill(&mut self, bytes: &[u8], set: &ByteSet) {
        self.words.clear();
        self.words.resize(bytes.len().div_ceil(64), 0);
        dispatch!(level(), simd => fill_bitmap(simd, bytes, set, &mut self.words));
    }

    /// Position of the first set bit at or after `start`.
    #[must_use]
    #[inline]
    pub fn next_from(&self, start: usize) -> Option<usize> {
        let mut word_index = start / 64;
        let mut word = self.words.get(word_index)? & (u64::MAX << (start % 64));
        loop {
            if word != 0 {
                return Some(word_index * 64 + word.trailing_zeros() as usize);
            }
            word_index += 1;
            word = *self.words.get(word_index)?;
        }
    }

    /// Whether any bit is set from `start` up to but not including `end`.
    #[must_use]
    #[inline]
    pub fn any_in(&self, start: usize, end: usize) -> bool {
        self.next_from(start).is_some_and(|position| position < end)
    }

    /// Number of set bits.
    #[must_use]
    pub fn count(&self) -> usize {
        self.words.iter().map(|word| word.count_ones() as usize).sum()
    }
}

impl ByteClass for ByteSet {
    #[allow(
        clippy::inline_always,
        reason = "inlining into the #[simd] caller gives it the caller's target features"
    )]
    #[inline(always)]
    fn vector<S: Simd, V: SimdInt<S, Element = u8>>(&self, simd: S, block: V) -> V::Mask {
        let low_table = V::block_splat(V::Block::from_slice(simd, &self.low));
        let high_table = V::block_splat(V::Block::from_slice(simd, &self.high));
        let low_bits = low_table.swizzle_dyn_within_blocks((block & 0x0F).to_bytes());
        let high_bits = high_table.swizzle_dyn_within_blocks(((block >> 4) & 0x0F).to_bytes());
        (low_bits & high_bits).simd_ne(0)
    }

    #[inline]
    fn scalar_bits(&self, bytes: &[u8]) -> u64 {
        bits_where(bytes, |byte| self.contains(byte))
    }
}

impl ByteClass for ContinuationOr {
    #[allow(
        clippy::inline_always,
        reason = "inlining into the #[simd] caller gives it the caller's target features"
    )]
    #[inline(always)]
    fn vector<S: Simd, V: SimdInt<S, Element = u8>>(&self, _simd: S, block: V) -> V::Mask {
        (block & 0xC0).simd_eq(0x80) | block.simd_eq(self.0)
    }

    #[inline]
    fn scalar_bits(&self, bytes: &[u8]) -> u64 {
        bits_where(bytes, |byte| byte & 0xC0 == 0x80 || byte == self.0)
    }
}

impl ByteClass for AnyOf<'_> {
    #[allow(
        clippy::inline_always,
        reason = "inlining into the #[simd] caller gives it the caller's target features"
    )]
    #[inline(always)]
    fn vector<S: Simd, V: SimdInt<S, Element = u8>>(&self, simd: S, block: V) -> V::Mask {
        self.0.iter().fold(V::Mask::splat(simd, false), |matched, &byte| {
            matched | block.simd_eq(byte)
        })
    }

    #[inline]
    fn scalar_bits(&self, bytes: &[u8]) -> u64 {
        let mut table = [0u64; 4];
        for &byte in self.0 {
            if let Some(word) = table.get_mut(usize::from(byte >> 6)) {
                *word |= 1 << (byte & 63);
            }
        }
        bits_where(bytes, |byte| {
            table
                .get(usize::from(byte >> 6))
                .is_some_and(|word| word >> (byte & 63) & 1 != 0)
        })
    }
}

impl ByteClass for Exactly {
    #[allow(
        clippy::inline_always,
        reason = "inlining into the #[simd] caller gives it the caller's target features"
    )]
    #[inline(always)]
    fn vector<S: Simd, V: SimdInt<S, Element = u8>>(&self, _simd: S, block: V) -> V::Mask {
        block.simd_eq(self.0)
    }

    #[inline]
    fn scalar_bits(&self, bytes: &[u8]) -> u64 {
        bits_where(bytes, |byte| byte == self.0)
    }
}

/// The SIMD level the public functions run in this process.
///
/// A build target that already guarantees NEON or AVX2 gives the level at compile time,
/// so the dispatch in each public function folds to one code path.
/// Other targets detect the level once at runtime.
#[must_use]
#[inline]
pub fn level() -> Level {
    let baseline = Level::baseline();
    if has_vector_baseline(baseline) {
        baseline
    } else {
        *LEVEL
    }
}

/// Byte index of the first byte in the precomputed set.
#[must_use]
#[inline]
pub fn find_byte_in(bytes: &[u8], set: &ByteSet) -> Option<usize> {
    dispatch!(level(), simd => find_first_in(simd, bytes, set))
}

/// Whether any byte is in the precomputed set.
#[must_use]
#[inline]
pub fn contains_byte_in(bytes: &[u8], set: &ByteSet) -> bool {
    find_byte_in(bytes, set).is_some()
}

/// Whether two bytes from the precomputed set stand next to each other anywhere in the input.
#[must_use]
#[inline]
pub fn has_adjacent_bytes_in(bytes: &[u8], set: &ByteSet) -> bool {
    dispatch!(level(), simd => adjacent_in(simd, bytes, set))
}

/// Number of UTF-8 characters in the text, the same as `text.chars().count()`.
#[must_use]
pub fn count_chars(text: &str) -> usize {
    let bytes = text.as_bytes();
    // A continuation byte is its own extra byte, so it adds nothing to the continuation count.
    bytes.len() - dispatch!(level(), simd => count_continuation_or_byte(simd, bytes, 0x80))
}

/// Number of UTF-8 characters in the text that are not the given ASCII byte.
#[must_use]
pub fn count_chars_excluding(text: &str, excluded: u8) -> usize {
    debug_assert!(excluded.is_ascii(), "the excluded byte must be ASCII");
    let bytes = text.as_bytes();
    bytes.len() - dispatch!(level(), simd => count_continuation_or_byte(simd, bytes, excluded))
}

/// Byte index of the first byte that is one of the bytes in the set.
#[must_use]
pub fn find_byte_of(bytes: &[u8], set: &[u8]) -> Option<usize> {
    dispatch!(level(), simd => find_first_of(simd, bytes, set))
}

/// Whether any byte is one of the bytes in the set.
#[must_use]
pub fn contains_byte_of(bytes: &[u8], set: &[u8]) -> bool {
    find_byte_of(bytes, set).is_some()
}

/// Whether two bytes from the set stand next to each other anywhere in the input.
#[must_use]
pub fn has_adjacent_bytes_of(bytes: &[u8], set: &[u8]) -> bool {
    dispatch!(level(), simd => adjacent_of(simd, bytes, set))
}

/// Byte indices of every occurrence of the byte.
#[must_use]
pub fn byte_positions(bytes: &[u8], needle: u8) -> Vec<usize> {
    let mut positions = Vec::new();
    dispatch!(level(), simd => push_positions(simd, bytes, needle, &mut positions));
    positions
}

/// Count UTF-8 continuation bytes together with occurrences of `extra`.
#[simd]
fn count_continuation_or_byte<S: Simd>(simd: S, bytes: &[u8], extra: u8) -> usize {
    let mut count = 0;
    visit_blocks(
        simd,
        bytes,
        &ContinuationOr(extra),
        #[inline(always)]
        |_, bits, _| {
            count += bits.count_ones() as usize;
            ControlFlow::Continue(())
        },
    );
    count
}

/// Byte index of the first byte in the set.
#[simd]
fn find_first_of<S: Simd>(simd: S, bytes: &[u8], set: &[u8]) -> Option<usize> {
    first_of_class(simd, bytes, &AnyOf(set))
}

/// Byte index of the first byte in the precomputed set.
#[simd]
fn find_first_in<S: Simd>(simd: S, bytes: &[u8], set: &ByteSet) -> Option<usize> {
    first_of_class(simd, bytes, set)
}

/// Byte index of the first byte of the class.
#[allow(
    clippy::inline_always,
    reason = "inlining into the #[simd] caller gives it the caller's target features"
)]
#[inline(always)]
fn first_of_class<S: Simd, C: ByteClass>(simd: S, bytes: &[u8], class: &C) -> Option<usize> {
    let mut found = None;
    visit_blocks(
        simd,
        bytes,
        class,
        #[inline(always)]
        |offset, bits, _| {
            if bits == 0 {
                return ControlFlow::Continue(());
            }
            found = Some(offset + bits.trailing_zeros() as usize);
            ControlFlow::Break(())
        },
    );
    found
}

/// Whether two bytes in the set are adjacent.
#[simd]
fn adjacent_of<S: Simd>(simd: S, bytes: &[u8], set: &[u8]) -> bool {
    adjacent_in_class(simd, bytes, &AnyOf(set))
}

/// Whether two bytes in the precomputed set are adjacent.
#[simd]
fn adjacent_in<S: Simd>(simd: S, bytes: &[u8], set: &ByteSet) -> bool {
    adjacent_in_class(simd, bytes, set)
}

/// Whether two bytes of the class are adjacent.
#[allow(
    clippy::inline_always,
    reason = "inlining into the #[simd] caller gives it the caller's target features"
)]
#[inline(always)]
fn adjacent_in_class<S: Simd, C: ByteClass>(simd: S, bytes: &[u8], class: &C) -> bool {
    let mut previous_matched = false;
    let mut adjacent = false;
    visit_blocks(
        simd,
        bytes,
        class,
        #[inline(always)]
        |_, bits, valid| {
            if bits & (bits >> 1) != 0 || previous_matched && bits & 1 != 0 {
                adjacent = true;
                return ControlFlow::Break(());
            }
            previous_matched = (bits >> (valid - 1)) & 1 != 0;
            ControlFlow::Continue(())
        },
    );
    adjacent
}

/// Set the bit of every byte in the set, into words already sized and zeroed for the input.
#[simd]
fn fill_bitmap<S: Simd>(simd: S, bytes: &[u8], set: &ByteSet, words: &mut [u64]) {
    visit_blocks(
        simd,
        bytes,
        set,
        #[inline(always)]
        |offset, bits, _| {
            let shift = offset % 64;
            if let Some(word) = words.get_mut(offset / 64) {
                *word |= bits << shift;
            }
            if shift != 0
                && let Some(word) = words.get_mut(offset / 64 + 1)
            {
                *word |= bits >> (64 - shift);
            }
            ControlFlow::Continue(())
        },
    );
}

/// Append the byte index of every occurrence of the needle.
#[simd]
fn push_positions<S: Simd>(simd: S, bytes: &[u8], needle: u8, positions: &mut Vec<usize>) {
    visit_blocks(
        simd,
        bytes,
        &Exactly(needle),
        #[inline(always)]
        |offset, mut bits, _| {
            while bits != 0 {
                positions.push(offset + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
            ControlFlow::Continue(())
        },
    );
}

/// Call `visit` with the bitmask of classified bytes for each block of the input, in order.
///
/// The callback gets the byte offset of bit zero, the bitmask, and how many low bits of it are valid.
/// Input shorter than a 128-bit vector is classified byte by byte,
/// input shorter than a native vector uses 128-bit blocks, and longer input uses native blocks.
#[allow(
    clippy::inline_always,
    reason = "inlining into the #[simd] caller gives it the caller's target features"
)]
#[inline(always)]
fn visit_blocks<S: Simd, C: ByteClass>(
    simd: S,
    bytes: &[u8],
    class: &C,
    visit: impl FnMut(usize, u64, usize) -> ControlFlow<()>,
) {
    let length = bytes.len();
    if length == 0 {
        return;
    }
    if length < u8x16::<S>::LEN {
        let mut visit = visit;
        let _ = visit(0, class.scalar_bits(bytes), length);
    } else if length < S::u8s::LEN {
        visit_width::<S, u8x16<S>, C>(simd, bytes, class, visit);
    } else {
        visit_width::<S, S::u8s, C>(simd, bytes, class, visit);
    }
}

/// Visit the input in blocks of vector type `V`, overlapping the last block with the one before it.
///
/// The input must hold at least one full block.
#[allow(
    clippy::inline_always,
    reason = "inlining into the #[simd] caller gives it the caller's target features"
)]
#[inline(always)]
fn visit_width<S: Simd, V: SimdInt<S, Element = u8>, C: ByteClass>(
    simd: S,
    bytes: &[u8],
    class: &C,
    mut visit: impl FnMut(usize, u64, usize) -> ControlFlow<()>,
) {
    let lanes = V::LEN;
    let mut chunks = bytes.chunks_exact(lanes);
    for (index, chunk) in chunks.by_ref().enumerate() {
        let bits = class.vector(simd, V::from_slice(simd, chunk)).to_bitmask();
        if visit(index * lanes, bits, lanes).is_break() {
            return;
        }
    }
    let remainder = chunks.remainder().len();
    if remainder > 0 {
        let length = bytes.len();
        let block = V::from_slice(simd, bytes.get(length - lanes..).unwrap_or_default());
        let bits = class.vector(simd, block).to_bitmask() >> (lanes - remainder);
        let _ = visit(length - remainder, bits, remainder);
    }
}

/// The membership table with the bit of one more byte set.
const fn with_table_bit(table: [u64; 4], byte: u8) -> [u64; 4] {
    let [first, second, third, fourth] = table;
    let bit = 1u64 << (byte & 63);
    match byte >> 6 {
        0 => [first | bit, second, third, fourth],
        1 => [first, second | bit, third, fourth],
        2 => [first, second, third | bit, fourth],
        _ => [first, second, third, fourth | bit],
    }
}

/// Whether the compile-time level already has 256-bit AVX2 or NEON vectors.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline]
fn has_vector_baseline(baseline: Level) -> bool {
    baseline.as_avx2().is_some()
}

/// Whether the compile-time level already has 256-bit AVX2 or NEON vectors.
#[cfg(target_arch = "aarch64")]
#[inline]
fn has_vector_baseline(baseline: Level) -> bool {
    baseline.as_neon().is_some()
}

/// Whether the compile-time level already has 256-bit AVX2 or NEON vectors.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
#[inline]
const fn has_vector_baseline(_baseline: Level) -> bool {
    false
}

/// Bitmask with bit `i` set when byte `i` matches, for input shorter than 64 bytes.
#[inline]
fn bits_where(bytes: &[u8], matches: impl Fn(u8) -> bool) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0, |bits, (index, &byte)| bits | u64::from(matches(byte)) << index)
}

/// Shared SIMD level and input builders for the kernel tests.
#[cfg(test)]
mod test_helpers {
    use fearless_simd::Level;

    /// Every SIMD level this machine supports and the scalar fallback,
    /// so the tests cover each vector width and the path machines without SIMD take.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub(super) fn test_levels() -> Vec<Level> {
        let mut levels = vec![Level::new(), Level::fallback()];
        let level = Level::new();
        levels.extend(level.as_sse2().map(Level::Sse2));
        levels.extend(level.as_sse4_2().map(Level::Sse4_2));
        levels.extend(level.as_avx2().map(Level::Avx2));
        levels.extend(level.as_avx512().map(Level::Avx512));
        levels
    }

    /// The native SIMD level and the scalar fallback,
    /// so the tests cover both the vector path and the path machines without SIMD take.
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    pub(super) fn test_levels() -> Vec<Level> {
        vec![Level::new(), Level::fallback()]
    }

    /// Deterministic mixed ASCII and multibyte strings of every length up to about 200 bytes.
    pub(super) fn test_inputs() -> Vec<String> {
        const FRAGMENTS: &[&str] = &[
            "a", "Z", " ", ".", "..", "-", "_", "\t", "é", "€", "—", "😀", "\u{a0}", "\u{3000}", "x265", "fn", "\"",
            "'",
        ];
        let mut inputs = vec![String::new()];
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..40 {
            let mut text = String::new();
            while text.len() < 200 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let index = (state >> 33) as usize % FRAGMENTS.len();
                text.push_str(FRAGMENTS.get(index).copied().unwrap_or_default());
                inputs.push(text.clone());
            }
        }
        inputs
    }
}

#[cfg(test)]
mod test_count_chars {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn matches_std_char_count_at_every_level() {
        for level in test_levels() {
            for text in test_inputs() {
                let continuation = dispatch!(level, simd => count_continuation_or_byte(simd, text.as_bytes(), 0x80));
                assert_eq!(text.len() - continuation, text.chars().count(), "{level:?} {text:?}");
            }
        }
    }

    #[test]
    fn excludes_the_given_byte() {
        for level in test_levels() {
            for text in test_inputs() {
                let excluded = dispatch!(level, simd => count_continuation_or_byte(simd, text.as_bytes(), b'.'));
                let expected = text.chars().filter(|character| *character != '.').count();
                assert_eq!(text.len() - excluded, expected, "{level:?} {text:?}");
            }
        }
    }

    #[test]
    fn public_functions_count_characters() {
        assert_eq!(count_chars(""), 0);
        assert_eq!(count_chars("héllo wörld"), 11);
        assert_eq!(count_chars_excluding("Photo.Lab.TV", b'.'), 10);
    }
}

#[cfg(test)]
mod test_find_byte_of {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn matches_scalar_position_at_every_level() {
        let sets: &[&[u8]] = &[b".", b" \t", b"\"'`", &[0xE2, 0xC2], b"#"];
        for level in test_levels() {
            for text in test_inputs() {
                let bytes = text.as_bytes();
                for set in sets {
                    let expected = bytes.iter().position(|byte| set.contains(byte));
                    let found = dispatch!(level, simd => find_first_of(simd, bytes, set));
                    assert_eq!(found, expected, "{level:?} {set:?} {text:?}");
                }
            }
        }
    }

    #[test]
    fn empty_set_or_input_finds_nothing() {
        assert_eq!(find_byte_of(b"", b"a"), None);
        assert_eq!(find_byte_of(b"abc", b""), None);
        assert!(contains_byte_of(b"no comment here // yes", b"/"));
        assert!(!contains_byte_of(b"let value = compute(input);", b"\"'/`"));
    }

    #[test]
    fn finds_bytes_in_the_overlapping_last_block() {
        let mut text = "a".repeat(100);
        text.push('#');
        assert_eq!(find_byte_of(text.as_bytes(), b"#"), Some(100));
    }
}

#[cfg(test)]
mod test_byte_set {
    use super::test_helpers::*;
    use super::*;

    /// Sets the project uses and edge cases: one byte, multibyte lead bytes, and eight distinct high nibbles.
    const SETS: &[&[u8]] = &[
        b"/\"'`<#r",
        b" \t\n\r\x0b\x0c\xc2\xe1\xe2\xe3",
        b"=({",
        b"._- \t",
        b"`",
        b"\x00\x1f\x20\x7f\x80\xbf\xc0\xff",
        b"\x01\x11\x21\x31\x41\x51\x61\x71",
        b"",
    ];

    #[test]
    fn contains_matches_slice_membership_for_every_byte() {
        for set in SETS {
            let byte_set = ByteSet::new(set);
            for byte in 0..=u8::MAX {
                assert_eq!(byte_set.contains(byte), set.contains(&byte), "{set:?} {byte:#04x}");
            }
        }
    }

    #[test]
    fn vector_classification_matches_every_byte_at_every_level() {
        let all_bytes: Vec<u8> = (0..=u8::MAX).collect();
        for level in test_levels() {
            for set in SETS {
                let byte_set = ByteSet::new(set);
                for byte in 0..=u8::MAX {
                    let mut input = all_bytes.clone();
                    input.retain(|other| !set.contains(other) || *other == byte);
                    let rotation = usize::from(byte) % input.len();
                    input.rotate_left(rotation);
                    let expected = input.iter().position(|other| set.contains(other));
                    let found = dispatch!(level, simd => find_first_in(simd, &input, &byte_set));
                    assert_eq!(found, expected, "{level:?} {set:?} {byte:#04x}");
                }
            }
        }
    }

    #[test]
    fn find_matches_scalar_position_at_every_level() {
        for level in test_levels() {
            for text in test_inputs() {
                let bytes = text.as_bytes();
                for set in SETS {
                    let byte_set = ByteSet::new(set);
                    let expected = bytes.iter().position(|byte| set.contains(byte));
                    let found = dispatch!(level, simd => find_first_in(simd, bytes, &byte_set));
                    assert_eq!(found, expected, "{level:?} {set:?} {text:?}");
                    let expected_adjacent = bytes.windows(2).any(|pair| pair.iter().all(|byte| set.contains(byte)));
                    let adjacent = dispatch!(level, simd => adjacent_in(simd, bytes, &byte_set));
                    assert_eq!(adjacent, expected_adjacent, "{level:?} {set:?} {text:?}");
                }
            }
        }
    }

    #[test]
    fn public_functions_use_the_set() {
        const TRIGGERS: ByteSet = ByteSet::new(b"/\"'");
        assert_eq!(find_byte_in(b"let value = 1; // note", &TRIGGERS), Some(15));
        assert!(contains_byte_in(b"print(\"x\")", &TRIGGERS));
        assert!(!contains_byte_in(b"plain words only", &TRIGGERS));
        assert!(has_adjacent_bytes_in(b"a//b", &TRIGGERS));
        assert!(!has_adjacent_bytes_in(b"a/b/c", &TRIGGERS));
    }

    #[test]
    #[should_panic(expected = "at most eight distinct high nibbles")]
    fn rejects_more_than_eight_high_nibbles() {
        let _ = ByteSet::new(&[0x00, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80]);
    }
}

#[cfg(test)]
mod test_byte_bitmap {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn matches_scalar_positions_at_every_level() {
        let set = ByteSet::new(b" \t.\xe2");
        for level in test_levels() {
            for text in test_inputs() {
                let bytes = text.as_bytes();
                let mut bitmap = ByteBitmap::default();
                bitmap.words.resize(bytes.len().div_ceil(64), 0);
                dispatch!(level, simd => fill_bitmap(simd, bytes, &set, &mut bitmap.words));
                let expected: Vec<usize> = bytes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, byte)| set.contains(*byte).then_some(index))
                    .collect();
                let mut found = Vec::new();
                let mut position = 0;
                while let Some(next) = bitmap.next_from(position) {
                    found.push(next);
                    position = next + 1;
                }
                assert_eq!(found, expected, "{level:?} {text:?}");
                assert_eq!(bitmap.count(), expected.len(), "{level:?} {text:?}");
            }
        }
    }

    #[test]
    fn searches_ranges_and_reuses_the_allocation() {
        let set = ByteSet::new(b"#");
        let mut bitmap = ByteBitmap::new(b"ab#cd", &set);
        assert_eq!(bitmap.next_from(0), Some(2));
        assert_eq!(bitmap.next_from(3), None);
        assert!(bitmap.any_in(0, 3));
        assert!(!bitmap.any_in(0, 2));
        assert!(!bitmap.any_in(3, 5));
        let long = format!("{}#{}#", "x".repeat(70), "y".repeat(60));
        bitmap.fill(long.as_bytes(), &set);
        assert_eq!(bitmap.next_from(0), Some(70));
        assert_eq!(bitmap.next_from(71), Some(131));
        assert_eq!(bitmap.next_from(132), None);
        assert_eq!(bitmap.next_from(500), None);
        bitmap.fill(b"", &set);
        assert_eq!(bitmap.next_from(0), None);
    }
}

#[cfg(test)]
mod test_adjacent_bytes {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn matches_scalar_windows_at_every_level() {
        let set = b"._- \t";
        for level in test_levels() {
            for text in test_inputs() {
                let bytes = text.as_bytes();
                let expected = bytes.windows(2).any(|pair| pair.iter().all(|byte| set.contains(byte)));
                let adjacent = dispatch!(level, simd => adjacent_of(simd, bytes, set));
                assert_eq!(adjacent, expected, "{level:?} {text:?}");
            }
        }
    }

    #[test]
    fn detects_pairs_across_block_boundaries() {
        for split in [15, 16, 31, 32, 63, 64] {
            let mut text = "a".repeat(split);
            text.push_str("..");
            text.push_str(&"b".repeat(80));
            assert!(has_adjacent_bytes_of(text.as_bytes(), b"."), "split at {split}");
        }
        assert!(!has_adjacent_bytes_of(b"Movie.Title.2024", b"._-"));
    }
}

#[cfg(test)]
mod test_byte_positions {
    use super::test_helpers::*;
    use super::*;

    #[test]
    fn matches_scalar_positions_at_every_level() {
        for level in test_levels() {
            for text in test_inputs() {
                let bytes = text.as_bytes();
                let expected: Vec<usize> = bytes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, byte)| (*byte == b'.').then_some(index))
                    .collect();
                let mut positions = Vec::new();
                dispatch!(level, simd => push_positions(simd, bytes, b'.', &mut positions));
                assert_eq!(positions, expected, "{level:?} {text:?}");
            }
        }
    }

    #[test]
    fn public_function_returns_positions() {
        assert_eq!(byte_positions(b"A.B.C", b'.'), vec![1, 3]);
        assert_eq!(byte_positions(b"", b'.'), [] as [usize; 0]);
    }
}

#[cfg(test)]
mod test_kernel_properties {
    use proptest::prelude::*;

    use super::test_helpers::*;
    use super::*;

    /// Bytes drawn mostly from a small alphabet so set members and adjacent pairs are common,
    /// with lengths spanning several 16, 32, and 64 byte blocks.
    fn bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop_oneof![
                4 => prop::sample::select(b"._- \t#ab".to_vec()),
                1 => any::<u8>(),
            ],
            0..300,
        )
    }

    /// Up to four bytes to search for.
    fn set_strategy() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(prop_oneof![prop::sample::select(b"._- #".to_vec()), any::<u8>()], 0..4)
    }

    proptest! {
        #[test]
        fn count_chars_matches_std(text in "\\PC{0,200}") {
            for level in test_levels() {
                let continuation = dispatch!(level, simd => count_continuation_or_byte(simd, text.as_bytes(), 0x80));
                prop_assert_eq!(text.len() - continuation, text.chars().count(), "{:?}", level);
            }
        }

        #[test]
        fn count_chars_excluding_matches_std(text in "[._a-z é😀]{0,200}") {
            for level in test_levels() {
                let excluded = dispatch!(level, simd => count_continuation_or_byte(simd, text.as_bytes(), b'.'));
                let expected = text.chars().filter(|character| *character != '.').count();
                prop_assert_eq!(text.len() - excluded, expected, "{:?}", level);
            }
        }

        #[test]
        fn find_first_of_matches_position(bytes in bytes_strategy(), set in set_strategy()) {
            let expected = bytes.iter().position(|byte| set.contains(byte));
            for level in test_levels() {
                let found = dispatch!(level, simd => find_first_of(simd, &bytes, &set));
                prop_assert_eq!(found, expected, "{:?}", level);
            }
        }

        #[test]
        fn adjacent_of_matches_windows(bytes in bytes_strategy(), set in set_strategy()) {
            let expected = bytes.windows(2).any(|pair| pair.iter().all(|byte| set.contains(byte)));
            for level in test_levels() {
                let adjacent = dispatch!(level, simd => adjacent_of(simd, &bytes, &set));
                prop_assert_eq!(adjacent, expected, "{:?}", level);
            }
        }

        #[test]
        fn byte_set_find_matches_position(bytes in bytes_strategy(), set in set_strategy()) {
            let byte_set = ByteSet::new(&set);
            let expected = bytes.iter().position(|byte| set.contains(byte));
            for level in test_levels() {
                let found = dispatch!(level, simd => find_first_in(simd, &bytes, &byte_set));
                prop_assert_eq!(found, expected, "{:?}", level);
            }
        }

        #[test]
        fn push_positions_matches_enumerate(bytes in bytes_strategy(), needle in prop::sample::select(b".# a".to_vec())) {
            let expected: Vec<usize> = bytes
                .iter()
                .enumerate()
                .filter_map(|(index, byte)| (*byte == needle).then_some(index))
                .collect();
            for level in test_levels() {
                let mut positions = Vec::new();
                dispatch!(level, simd => push_positions(simd, &bytes, needle, &mut positions));
                prop_assert_eq!(&positions, &expected, "{:?}", level);
            }
        }
    }
}
