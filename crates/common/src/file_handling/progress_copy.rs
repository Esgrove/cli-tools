//! Chunked file copying that reports progress and can be stopped between chunks.
//!
//! Used by the cross device moves in the parent module,
//! so a tool can show a byte progress bar for large copies to slow network drives and abort them cleanly.

use std::io::{self, Read, Write};

/// Size of the chunks copied between progress reports.
const COPY_CHUNK_SIZE: usize = 1024 * 1024;

/// Callback reporting the bytes written by a chunked copy.
///
/// It receives the size of each written chunk and returns false to stop the copy,
/// which then fails with [`io::ErrorKind::Interrupted`].
pub type CopyProgress<'a> = &'a dyn Fn(u64) -> bool;

/// Copy everything from `reader` to `writer` in chunks, reporting each written chunk to `progress`.
///
/// Returns the number of bytes copied.
///
/// # Errors
/// Returns an error if reading or writing fails,
/// or an [`io::ErrorKind::Interrupted`] error when `progress` returns false.
pub(super) fn copy_with_progress<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    progress: CopyProgress<'_>,
) -> io::Result<u64> {
    let mut buffer = vec![0_u8; COPY_CHUNK_SIZE];
    let mut copied: u64 = 0;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let chunk = buffer
            .get(..count)
            .ok_or_else(|| io::Error::other("Read reported more bytes than the buffer holds"))?;
        writer.write_all(chunk)?;
        copied += count as u64;
        if !progress(count as u64) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Copy stopped by the progress callback",
            ));
        }
    }
    writer.flush()?;
    Ok(copied)
}

#[cfg(test)]
mod test_copy_with_progress {
    use std::cell::Cell;
    use std::io::Cursor;

    use super::*;

    #[test]
    fn copies_everything_and_reports_each_chunk() {
        let content = vec![7_u8; COPY_CHUNK_SIZE * 2 + 123];
        let mut output = Vec::new();
        let reported = Cell::new(0_u64);
        let calls = Cell::new(0_usize);

        let copied = copy_with_progress(&mut Cursor::new(&content), &mut output, &|bytes| {
            reported.set(reported.get() + bytes);
            calls.set(calls.get() + 1);
            true
        })
        .unwrap();

        assert_eq!(copied, content.len() as u64);
        assert_eq!(reported.get(), content.len() as u64);
        assert_eq!(calls.get(), 3);
        assert_eq!(output, content);
    }

    #[test]
    fn copies_an_empty_input_without_reporting() {
        let mut output = Vec::new();
        let calls = Cell::new(0_usize);

        let copied = copy_with_progress(&mut Cursor::new(Vec::<u8>::new()), &mut output, &|_| {
            calls.set(calls.get() + 1);
            true
        })
        .unwrap();

        assert_eq!(copied, 0);
        assert_eq!(calls.get(), 0);
        assert_eq!(output.len(), 0);
    }

    #[test]
    fn stops_when_progress_returns_false() {
        let content = vec![1_u8; COPY_CHUNK_SIZE * 3];
        let mut output = Vec::new();

        let error = copy_with_progress(&mut Cursor::new(&content), &mut output, &|_| false).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(output.len(), COPY_CHUNK_SIZE);
    }
}
