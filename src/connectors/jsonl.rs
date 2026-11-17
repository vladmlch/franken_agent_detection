//! Line-oriented transcript reads with record-local UTF-8 recovery.

use std::io::{self, BufRead};
use std::iter::FusedIterator;

/// Default per-line byte cap for [`JsonlLines`] (64 MiB).
pub const DEFAULT_MAX_JSONL_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Maximum scratch buffer capacity retained across lines (256 KiB).
pub const MAX_RETAINED_LINE_BUF: usize = 256 * 1024;

/// Read UTF-8 lines, skipping corrupt or oversized records while stopping at reader failures.
///
/// UTF-8 is checked only after a complete physical line was read successfully.
/// By default, invalid UTF-8 lines are skipped (unless [`Self::with_lossy_utf8`]
/// is enabled) and lines exceeding `max_line_bytes` are drained in bounded
/// chunks without allocating beyond the cap.
/// An I/O error, including an underlying `InvalidData`, is returned unchanged
/// exactly once. Further iteration never reads again, so a failing source
/// cannot cause an unbounded error-and-retry loop. Callers must discard any
/// accumulated conversation when an error occurs.
#[derive(Debug)]
pub struct JsonlLines<R> {
    reader: R,
    buf: Vec<u8>,
    max_line_bytes: usize,
    skipped_oversized_lines: usize,
    lossy_utf8: bool,
    done: bool,
}

impl<R> JsonlLines<R> {
    /// Create a line iterator with [`DEFAULT_MAX_JSONL_LINE_BYTES`] cap and strict UTF-8 line skipping.
    pub const fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            max_line_bytes: DEFAULT_MAX_JSONL_LINE_BYTES,
            skipped_oversized_lines: 0,
            lossy_utf8: false,
            done: false,
        }
    }

    /// Override the maximum allowed line payload length in bytes (excluding line terminator).
    #[allow(dead_code)]
    pub const fn with_max_line_bytes(mut self, max_bytes: usize) -> Self {
        self.max_line_bytes = max_bytes;
        self
    }

    /// Enable or disable lossy UTF-8 replacement (`U+FFFD`) instead of skipping invalid UTF-8 lines.
    #[allow(dead_code)]
    pub const fn with_lossy_utf8(mut self, lossy: bool) -> Self {
        self.lossy_utf8 = lossy;
        self
    }

    /// Return `true` if at least one line exceeded `max_line_bytes` and was skipped.
    #[allow(dead_code)]
    pub const fn skipped_oversized(&self) -> bool {
        self.skipped_oversized_lines > 0
    }

    /// Return the total number of lines skipped for exceeding `max_line_bytes`.
    #[allow(dead_code)]
    pub const fn skipped_oversized_count(&self) -> usize {
        self.skipped_oversized_lines
    }

    fn reset_scratch_buf(&mut self) {
        self.buf.clear();
        if self.buf.capacity() > MAX_RETAINED_LINE_BUF {
            self.buf.shrink_to(MAX_RETAINED_LINE_BUF);
        }
    }

    fn push_bounded(buf: &mut Vec<u8>, max_line_bytes: usize, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let new_len = buf.len() + chunk.len();
        if new_len > buf.capacity() {
            let doubled = buf.capacity().saturating_mul(2).max(new_len);
            if doubled > max_line_bytes || max_line_bytes < 8 {
                buf.reserve_exact(chunk.len());
            } else {
                buf.reserve(chunk.len());
            }
        }
        buf.extend_from_slice(chunk);
    }

    fn decode_line(bytes: &[u8], lossy_utf8: bool) -> Option<String> {
        match std::str::from_utf8(bytes) {
            Ok(line) => Some(line.to_owned()),
            Err(_) if lossy_utf8 => Some(String::from_utf8_lossy(bytes).into_owned()),
            Err(_) => None,
        }
    }
}

impl<R: BufRead> JsonlLines<R> {
    fn drain_oversized_line(&mut self) -> io::Result<()> {
        loop {
            let available = match self.reader.fill_buf() {
                Ok(buf) => buf,
                Err(error) => {
                    self.done = true;
                    return Err(error);
                }
            };
            if available.is_empty() {
                self.done = true;
                return Ok(());
            }
            if let Some(newline) = available.iter().position(|&byte| byte == b'\n') {
                self.reader.consume(newline + 1);
                return Ok(());
            }
            let len = available.len();
            self.reader.consume(len);
        }
    }

    fn read_physical_line(&mut self) -> io::Result<Option<String>> {
        self.reset_scratch_buf();
        let mut pending_cr = false;

        let outcome = loop {
            // Read bytes directly: read_line() conflates invalid UTF-8 with an
            // underlying InvalidData error. Do not retry an actual I/O error,
            // even Interrupted, without evidence that the source progressed.
            let available = match self.reader.fill_buf() {
                Ok(buf) => buf,
                Err(error) => {
                    self.done = true;
                    break Err(error);
                }
            };

            if available.is_empty() {
                self.done = true;
                if pending_cr {
                    // A final line ended with `\r` at EOF when `self.buf.len() == max_line_bytes`,
                    // so its length is `max_line_bytes + 1 > max_line_bytes`.
                    self.skipped_oversized_lines += 1;
                    break Ok(None);
                }
                if self.buf.is_empty() {
                    break Ok(None);
                }
                break Ok(Self::decode_line(&self.buf, self.lossy_utf8));
            }

            if pending_cr {
                if available[0] == b'\n' {
                    self.reader.consume(1);
                    break Ok(Self::decode_line(&self.buf, self.lossy_utf8));
                }
                self.skipped_oversized_lines += 1;
                self.reset_scratch_buf();
                if let Err(error) = self.drain_oversized_line() {
                    break Err(error);
                }
                break Ok(None);
            }

            if let Some(newline) = available.iter().position(|&byte| byte == b'\n') {
                let decoded = Self::consume_newline_slice(
                    available,
                    &mut self.buf,
                    self.max_line_bytes,
                    &mut self.skipped_oversized_lines,
                    self.lossy_utf8,
                    newline,
                );
                self.reader.consume(newline + 1);
                break Ok(decoded);
            }

            let total_len = self.buf.len().saturating_add(available.len());
            if total_len <= self.max_line_bytes {
                Self::push_bounded(&mut self.buf, self.max_line_bytes, available);
                let consumed = available.len();
                self.reader.consume(consumed);
                continue;
            }

            if total_len == self.max_line_bytes.saturating_add(1)
                && available.last() == Some(&b'\r')
            {
                let prefix_len = available.len() - 1;
                Self::push_bounded(&mut self.buf, self.max_line_bytes, &available[..prefix_len]);
                let consumed = available.len();
                self.reader.consume(consumed);
                pending_cr = true;
                continue;
            }

            let consumed = available.len();
            self.reader.consume(consumed);
            self.skipped_oversized_lines += 1;
            self.reset_scratch_buf();
            if let Err(error) = self.drain_oversized_line() {
                break Err(error);
            }
            break Ok(None);
        };

        self.reset_scratch_buf();
        outcome
    }

    fn consume_newline_slice(
        available: &[u8],
        buf: &mut Vec<u8>,
        max_line_bytes: usize,
        skipped_oversized_lines: &mut usize,
        lossy_utf8: bool,
        newline: usize,
    ) -> Option<String> {
        if buf.is_empty() {
            // Fast path: full line resides inside the reader's buffer,
            // avoiding scratch allocation/copies.
            // Match std::io::Lines: strip LF and its optional preceding CR,
            // but retain a standalone CR or a final line without a newline.
            let mut end = newline;
            if end > 0 && available[end - 1] == b'\r' {
                end -= 1;
            }
            if end > max_line_bytes {
                *skipped_oversized_lines += 1;
                return None;
            }
            return Self::decode_line(&available[..end], lossy_utf8);
        }

        let mut chunk_end = newline;
        if chunk_end > 0 && available[chunk_end - 1] == b'\r' {
            chunk_end -= 1;
        } else if chunk_end == 0 && buf.last() == Some(&b'\r') {
            buf.pop();
        }

        let total_len = buf.len() + chunk_end;
        if total_len > max_line_bytes {
            *skipped_oversized_lines += 1;
            return None;
        }

        Self::push_bounded(buf, max_line_bytes, &available[..chunk_end]);
        Self::decode_line(buf, lossy_utf8)
    }
}

impl<R: BufRead> Iterator for JsonlLines<R> {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            match self.read_physical_line() {
                Ok(Some(line)) => return Some(Ok(line)),
                Ok(None) => {
                    if self.done {
                        return None;
                    }
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

impl<R: BufRead> FusedIterator for JsonlLines<R> {}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::{Cursor, ErrorKind, Read};
    use std::rc::Rc;

    use super::*;

    struct FailingReader {
        prefix: Cursor<Vec<u8>>,
        kind: ErrorKind,
        attempts: Rc<Cell<usize>>,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let len = available.len().min(buffer.len());
            buffer[..len].copy_from_slice(&available[..len]);
            self.consume(len);
            Ok(len)
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if usize::try_from(self.prefix.position()).unwrap() < self.prefix.get_ref().len() {
                return self.prefix.fill_buf();
            }
            self.attempts.set(self.attempts.get() + 1);
            Err(io::Error::new(self.kind, "persistent source failure"))
        }

        fn consume(&mut self, amount: usize) {
            self.prefix.consume(amount);
        }
    }

    #[test]
    fn matches_standard_line_endings_and_complete_final_line() {
        let input = b"first\r\n\nthird\ncarriage\rfinal\r";
        let expected = Cursor::new(input)
            .lines()
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let actual = JsonlLines::new(Cursor::new(input))
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual, ["first", "", "third", "carriage\rfinal\r"]);
    }

    #[test]
    fn skips_only_corrupt_utf8_lines_and_keeps_following_records() {
        let input = b"before\n\xff\xfe broken\r\n\xef\xbb\xbfafter\n\xff";
        let lines = JsonlLines::new(Cursor::new(input))
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(lines, ["before", "\u{feff}after"]);
    }

    #[test]
    fn persistent_io_errors_are_preserved_and_terminal() {
        for kind in [
            ErrorKind::Other,
            ErrorKind::InvalidData,
            ErrorKind::Interrupted,
        ] {
            let attempts = Rc::new(Cell::new(0));
            let mut lines = JsonlLines::new(FailingReader {
                prefix: Cursor::new(Vec::new()),
                kind,
                attempts: Rc::clone(&attempts),
            });
            let error = lines.next().unwrap().unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "persistent source failure");
            assert_eq!(attempts.get(), 1);
            for _ in 0..3 {
                assert!(lines.next().is_none());
            }
            assert_eq!(attempts.get(), 1, "a failed read must never be retried");
        }
    }

    #[test]
    fn io_error_after_valid_prefix_discards_incomplete_physical_line() {
        let attempts = Rc::new(Cell::new(0));
        let mut lines = JsonlLines::new(FailingReader {
            prefix: Cursor::new(b"complete\nunfinished".to_vec()),
            kind: ErrorKind::Other,
            attempts: Rc::clone(&attempts),
        });
        assert_eq!(lines.next().unwrap().unwrap(), "complete");
        assert_eq!(lines.next().unwrap().unwrap_err().kind(), ErrorKind::Other);
        assert!(lines.next().is_none());
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn eof_is_terminal_even_if_reader_could_later_return_data() {
        struct EofThenData {
            prefix: Cursor<&'static [u8]>,
            polls: Rc<Cell<usize>>,
            observed_eof: bool,
        }

        impl Read for EofThenData {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                unreachable!("the iterator uses BufRead directly")
            }
        }

        impl BufRead for EofThenData {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                self.polls.set(self.polls.get() + 1);
                if usize::try_from(self.prefix.position()).unwrap() < self.prefix.get_ref().len() {
                    return self.prefix.fill_buf();
                }
                if self.observed_eof {
                    Ok(b"late\n")
                } else {
                    self.observed_eof = true;
                    Ok(b"")
                }
            }

            fn consume(&mut self, amount: usize) {
                self.prefix.consume(amount);
            }
        }

        for (prefix, expected) in [
            (b"".as_slice(), None),
            (b"final".as_slice(), Some("final")),
            (b"\xff".as_slice(), None),
        ] {
            let polls = Rc::new(Cell::new(0));
            let mut lines = JsonlLines::new(EofThenData {
                prefix: Cursor::new(prefix),
                polls: Rc::clone(&polls),
                observed_eof: false,
            });
            assert_eq!(lines.next().transpose().unwrap().as_deref(), expected);
            for _ in 0..3 {
                assert!(lines.next().is_none());
            }
            assert_eq!(polls.get(), if prefix.is_empty() { 1 } else { 2 });
        }
    }

    #[test]
    fn normal_multiline_and_crlf_without_trailing_newline_across_small_chunks() {
        use std::io::BufReader;

        let input = b"{\"id\":1}\r\n{\"id\":2}\n{\"id\":3,\"tail\":\"no-newline\"}";
        for cap in [1, 2, 3, 7, 64] {
            let reader = BufReader::with_capacity(cap, Cursor::new(input));
            let actual = JsonlLines::new(reader)
                .collect::<io::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                actual,
                [
                    "{\"id\":1}",
                    "{\"id\":2}",
                    "{\"id\":3,\"tail\":\"no-newline\"}"
                ],
                "failed with BufReader capacity {cap}"
            );
        }
    }

    #[test]
    fn invalid_utf8_lossy_recovery_replaces_bad_bytes_and_preserves_lines() {
        use std::io::BufReader;

        let input = b"before\n\xff\xfe broken\r\n\xef\xbb\xbfafter\n\xff";
        // Default fast path (single chunk)
        let fast = JsonlLines::new(Cursor::new(input))
            .with_lossy_utf8(true)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            fast,
            [
                "before",
                "\u{FFFD}\u{FFFD} broken",
                "\u{feff}after",
                "\u{FFFD}"
            ]
        );

        // Multi-chunk scratch path
        let chunked = JsonlLines::new(BufReader::with_capacity(3, Cursor::new(input)))
            .with_lossy_utf8(true)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(chunked, fast);
    }

    #[test]
    fn scratch_buffer_reuses_within_cap_and_shrinks_after_large_line() {
        use std::io::BufReader;

        let small_a = format!("{{\"k\":\"{}\"}}\n", "a".repeat(12 * 1024));
        let small_b = format!("{{\"k\":\"{}\"}}\n", "b".repeat(12 * 1024));
        let huge = format!("{{\"k\":\"{}\"}}\n", "x".repeat(320 * 1024));
        let tail = "{\"k\":\"tail\"}\n";

        let mut data = Vec::new();
        data.extend_from_slice(small_a.as_bytes());
        data.extend_from_slice(small_b.as_bytes());
        data.extend_from_slice(huge.as_bytes());
        data.extend_from_slice(tail.as_bytes());

        // Small BufReader forces multi-chunk accumulation into `self.buf`.
        let mut lines = JsonlLines::new(BufReader::with_capacity(4096, Cursor::new(data)));
        assert_eq!(lines.buf.capacity(), 0);

        let first = lines.next().unwrap().unwrap();
        assert_eq!(first.len(), small_a.len() - 1);
        let cap_after_first = lines.buf.capacity();
        let ptr_after_first = lines.buf.as_ptr();
        assert!(cap_after_first >= first.len());
        assert!(cap_after_first <= MAX_RETAINED_LINE_BUF);

        let second = lines.next().unwrap().unwrap();
        assert_eq!(second.len(), small_b.len() - 1);
        assert_eq!(
            lines.buf.capacity(),
            cap_after_first,
            "scratch buffer capacity should be reused without growing"
        );
        assert_eq!(
            lines.buf.as_ptr(),
            ptr_after_first,
            "scratch buffer should be reused in place without reallocation"
        );

        let third = lines.next().unwrap().unwrap();
        assert_eq!(third.len(), huge.len() - 1);
        assert!(
            lines.buf.capacity() <= MAX_RETAINED_LINE_BUF,
            "scratch buffer capacity {} exceeded MAX_RETAINED_LINE_BUF ({})",
            lines.buf.capacity(),
            MAX_RETAINED_LINE_BUF
        );

        let fourth = lines.next().unwrap().unwrap();
        assert_eq!(fourth, "{\"k\":\"tail\"}");
        assert!(lines.buf.capacity() <= MAX_RETAINED_LINE_BUF);
        assert!(lines.next().is_none());
        assert!(!lines.skipped_oversized());
        assert_eq!(lines.skipped_oversized_count(), 0);
    }

    #[test]
    fn oversized_line_is_drained_bounded_and_subsequent_lines_survive() {
        use std::io::BufReader;

        let max_bytes = 16 * 1024;
        let oversized_a = vec![b'a'; 512 * 1024];
        let oversized_b = vec![b'b'; max_bytes + 1];

        let mut data = Vec::new();
        data.extend_from_slice(b"{\"line\":1}\n");
        data.extend_from_slice(&oversized_a);
        data.push(b'\n');
        data.extend_from_slice(b"{\"line\":2}\r\n");
        data.extend_from_slice(&oversized_b);
        data.push(b'\n');
        data.extend_from_slice(b"{\"line\":3}");

        // Multi-chunk BufReader exercises bounded accumulation + chunked drain.
        let mut lines = JsonlLines::new(BufReader::with_capacity(1024, Cursor::new(data.clone())))
            .with_max_line_bytes(max_bytes);
        assert!(!lines.skipped_oversized());
        assert_eq!(lines.skipped_oversized_count(), 0);

        let mut collected = Vec::new();
        while let Some(item) = lines.next() {
            collected.push(item.unwrap());
            assert!(
                lines.buf.capacity() <= max_bytes,
                "scratch capacity {} exceeded max_bytes {}",
                lines.buf.capacity(),
                max_bytes
            );
        }
        assert_eq!(collected, ["{\"line\":1}", "{\"line\":2}", "{\"line\":3}"]);
        assert!(lines.skipped_oversized());
        assert_eq!(lines.skipped_oversized_count(), 2);

        // Single-chunk fast-path check via Cursor: only the final line without `\n`
        // uses `self.buf`; the 512 KiB oversized lines never allocate into `self.buf`.
        let mut fast = JsonlLines::new(Cursor::new(data)).with_max_line_bytes(max_bytes);
        let fast_collected = fast.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
        assert_eq!(fast_collected, collected);
        assert_eq!(fast.buf.capacity(), "{\"line\":3}".len());
        assert!(fast.skipped_oversized());
        assert_eq!(fast.skipped_oversized_count(), 2);
    }

    #[test]
    fn oversized_final_line_without_newline_and_exact_limit_crlf_boundary() {
        use std::io::BufReader;

        let max_bytes = 16;
        // Exact-limit line with CRLF split across chunk boundaries, followed by exact-limit EOF,
        // vs max_bytes + 1 at EOF without trailing newline.
        let exact_crlf = format!("{}\r\n", "x".repeat(max_bytes));
        let exact_lf = format!("{}\n", "y".repeat(max_bytes));
        let oversized_eof = "z".repeat(max_bytes + 1);

        let mut input = Vec::new();
        input.extend_from_slice(exact_crlf.as_bytes());
        input.extend_from_slice(exact_lf.as_bytes());
        input.extend_from_slice(oversized_eof.as_bytes());

        for cap in [1, 3, 17, 64] {
            let mut lines = JsonlLines::new(BufReader::with_capacity(cap, Cursor::new(&input)))
                .with_max_line_bytes(max_bytes);
            let collected = lines.by_ref().collect::<io::Result<Vec<_>>>().unwrap();
            assert_eq!(
                collected,
                ["x".repeat(max_bytes), "y".repeat(max_bytes)],
                "failed exact boundary check with cap={cap}"
            );
            assert!(lines.skipped_oversized());
            assert_eq!(lines.skipped_oversized_count(), 1);
            assert!(lines.buf.capacity() <= max_bytes);
        }
    }

    #[test]
    fn io_error_during_oversized_drain_is_terminal() {
        use std::io::BufReader;

        let attempts = Rc::new(Cell::new(0));
        let mut prefix = Vec::new();
        prefix.extend_from_slice(b"ok\n");
        prefix.extend_from_slice(&vec![b'q'; 4096]);
        let reader = BufReader::with_capacity(
            256,
            FailingReader {
                prefix: Cursor::new(prefix),
                kind: ErrorKind::Other,
                attempts: Rc::clone(&attempts),
            },
        );
        let mut lines = JsonlLines::new(reader).with_max_line_bytes(512);
        assert_eq!(lines.next().unwrap().unwrap(), "ok");
        assert_eq!(lines.next().unwrap().unwrap_err().kind(), ErrorKind::Other);
        assert!(lines.next().is_none());
        assert_eq!(attempts.get(), 1);
        assert!(lines.skipped_oversized());
    }
}
