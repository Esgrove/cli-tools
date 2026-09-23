//! Portable SIMD byte scanning kernels built on `fearless_simd`.
//!
//! Each public function picks the best SIMD level available at runtime and runs a kernel generic over [`Simd`].
//! The kernels classify a vector of bytes at a time and turn the lane masks into bitmasks,
//! so counting and searching become bit operations.
//! Input shorter than a 128-bit vector is classified byte by byte, because loading a vector costs more there.
//! The last block of longer input overlaps the one before it, so no kernel needs a scalar tail loop.
//!
//! The `CLI_TOOLS_SIMD` environment variable overrides the choice for benchmarking.
//! "scalar" runs plain loops instead of the kernels,
//! and "sse2", "sse4.2", "avx2", or "avx512" caps the SIMD level on x86.

use std::ops::ControlFlow;
use std::sync::LazyLock;

use fearless_simd::{Level, Simd, dispatch, prelude::*, u8x16};
use fearless_simd_macros::simd;

/// Environment variable that selects the implementation, read once per process.
pub const MODE_VARIABLE: &str = "CLI_TOOLS_SIMD";

/// Implementation the public functions run, chosen once from [`MODE_VARIABLE`].
static MODE: LazyLock<Mode> = LazyLock::new(|| Mode::from_setting(std::env::var(MODE_VARIABLE).ok().as_deref()));

/// Which implementation the public functions run.
#[derive(Debug, Clone, Copy)]
pub enum Mode {
    /// SIMD kernels at the given level.
    Simd(Level),
    /// Plain scalar loops, the reference the kernels are measured against.
    Scalar,
}

/// A class of bytes the kernels test both as whole vectors of any width and as single bytes.
trait ByteClass {
    /// Lanes of the block holding a byte of the class.
    fn vector<S: Simd, V: SimdInt<S, Element = u8>>(&self, simd: S, block: V) -> V::Mask;

    /// Bitmask of the bytes in the class, for input shorter than 64 bytes.
    fn scalar_bits(&self, bytes: &[u8]) -> u64;
}

/// UTF-8 continuation bytes and one extra byte.
struct ContinuationOr(u8);

/// Any byte of a set.
struct AnyOf<'a>(&'a [u8]);

/// One exact byte.
struct Exactly(u8);

impl Mode {
    /// Mode for a setting value, falling back to the detected level for unknown or unsupported values.
    fn from_setting(setting: Option<&str>) -> Self {
        let detected = Level::new();
        let setting = setting.map(str::to_ascii_lowercase);
        let capped = match setting.as_deref() {
            Some("scalar") => return Self::Scalar,
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Some("sse2") => detected.as_sse2().map(Level::Sse2),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Some("sse4.2") => detected.as_sse4_2().map(Level::Sse4_2),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Some("avx2") => detected.as_avx2().map(Level::Avx2),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            Some("avx512") => detected.as_avx512().map(Level::Avx512),
            _ => None,
        };
        Self::Simd(capped.unwrap_or(detected))
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

/// The implementation the public functions run in this process.
#[must_use]
pub fn mode() -> Mode {
    *MODE
}

/// Number of UTF-8 characters in the text, the same as `text.chars().count()`.
#[must_use]
pub fn count_chars(text: &str) -> usize {
    let bytes = text.as_bytes();
    match mode() {
        // A continuation byte is its own extra byte, so it adds nothing to the continuation count.
        Mode::Simd(level) => bytes.len() - dispatch!(level, simd => count_continuation_or_byte(simd, bytes, 0x80)),
        Mode::Scalar => text.chars().count(),
    }
}

/// Number of UTF-8 characters in the text that are not the given ASCII byte.
#[must_use]
pub fn count_chars_excluding(text: &str, excluded: u8) -> usize {
    debug_assert!(excluded.is_ascii(), "the excluded byte must be ASCII");
    let bytes = text.as_bytes();
    match mode() {
        Mode::Simd(level) => bytes.len() - dispatch!(level, simd => count_continuation_or_byte(simd, bytes, excluded)),
        Mode::Scalar => text
            .chars()
            .filter(|character| *character != char::from(excluded))
            .count(),
    }
}

/// Byte index of the first byte that is one of the bytes in the set.
#[must_use]
pub fn find_byte_of(bytes: &[u8], set: &[u8]) -> Option<usize> {
    match mode() {
        Mode::Simd(level) => dispatch!(level, simd => find_first_of(simd, bytes, set)),
        Mode::Scalar => bytes.iter().position(|byte| set.contains(byte)),
    }
}

/// Whether any byte is one of the bytes in the set.
#[must_use]
pub fn contains_byte_of(bytes: &[u8], set: &[u8]) -> bool {
    find_byte_of(bytes, set).is_some()
}

/// Whether two bytes from the set stand next to each other anywhere in the input.
#[must_use]
pub fn has_adjacent_bytes_of(bytes: &[u8], set: &[u8]) -> bool {
    match mode() {
        Mode::Simd(level) => dispatch!(level, simd => adjacent_of(simd, bytes, set)),
        Mode::Scalar => bytes.windows(2).any(|pair| pair.iter().all(|byte| set.contains(byte))),
    }
}

/// Byte indices of every occurrence of the byte.
#[must_use]
pub fn byte_positions(bytes: &[u8], needle: u8) -> Vec<usize> {
    match mode() {
        Mode::Simd(level) => {
            let mut positions = Vec::new();
            dispatch!(level, simd => push_positions(simd, bytes, needle, &mut positions));
            positions
        }
        Mode::Scalar => bytes
            .iter()
            .enumerate()
            .filter_map(|(index, &byte)| (byte == needle).then_some(index))
            .collect(),
    }
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
    let mut found = None;
    visit_blocks(
        simd,
        bytes,
        &AnyOf(set),
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
    let mut previous_matched = false;
    let mut adjacent = false;
    visit_blocks(
        simd,
        bytes,
        &AnyOf(set),
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

/// Bitmask with bit `i` set when byte `i` matches, for input shorter than 64 bytes.
#[inline]
fn bits_where(bytes: &[u8], matches: impl Fn(u8) -> bool) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0, |bits, (index, &byte)| bits | u64::from(matches(byte)) << index)
}

/// Every SIMD level this machine supports, so the tests cover each vector width.
#[cfg(all(test, any(target_arch = "x86", target_arch = "x86_64")))]
fn test_levels() -> Vec<Level> {
    let mut levels = vec![Level::new()];
    let level = Level::new();
    levels.extend(level.as_sse2().map(Level::Sse2));
    levels.extend(level.as_sse4_2().map(Level::Sse4_2));
    levels.extend(level.as_avx2().map(Level::Avx2));
    levels.extend(level.as_avx512().map(Level::Avx512));
    levels
}

/// Every SIMD level this machine supports, so the tests cover each vector width.
#[cfg(all(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
fn test_levels() -> Vec<Level> {
    vec![Level::new()]
}

/// Deterministic mixed ASCII and multibyte strings of every length up to about 200 bytes.
#[cfg(test)]
fn test_inputs() -> Vec<String> {
    const FRAGMENTS: &[&str] = &[
        "a", "Z", " ", ".", "..", "-", "_", "\t", "é", "€", "—", "😀", "\u{a0}", "\u{3000}", "x265", "fn", "\"", "'",
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

#[cfg(test)]
mod test_count_chars {
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
mod test_adjacent_bytes {
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
        assert!(byte_positions(b"", b'.').is_empty());
    }
}

#[cfg(test)]
mod test_mode {
    use super::*;

    #[test]
    fn scalar_setting_selects_scalar_loops() {
        assert!(matches!(Mode::from_setting(Some("scalar")), Mode::Scalar));
        assert!(matches!(Mode::from_setting(Some("SCALAR")), Mode::Scalar));
    }

    #[test]
    fn missing_or_unknown_setting_uses_the_detected_level() {
        assert!(matches!(Mode::from_setting(None), Mode::Simd(_)));
        assert!(matches!(Mode::from_setting(Some("fastest")), Mode::Simd(_)));
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn level_setting_caps_the_level_when_supported() {
        let Mode::Simd(level) = Mode::from_setting(Some("sse2")) else {
            panic!("sse2 should select a SIMD level");
        };
        assert!(matches!(level, Level::Sse2(_)));
        if Level::new().as_avx2().is_some() {
            let Mode::Simd(level) = Mode::from_setting(Some("avx2")) else {
                panic!("avx2 should select a SIMD level");
            };
            assert!(matches!(level, Level::Avx2(_)));
        }
    }
}
