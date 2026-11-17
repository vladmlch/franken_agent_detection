//! O(single-element)-memory streaming deserialization for JSON array containers.
//!
//! Supports:
//! - Peeking the first non-whitespace, non-UTF-8-BOM byte of a `BufRead`
//!   stream without rewinding or re-reading from disk, so callers handling
//!   either top-level `[...]` arrays or top-level `{...}` objects can dispatch
//!   cleanly.
//! - Streaming elements of a top-level JSON array (`[...]`) or a nested array
//!   field inside a JSON object (`SeqStreamSeed` / `drive_seq_stream`) one
//!   element at a time through `serde::de::DeserializeSeed` and `SeqAccess`,
//!   without materializing a `Vec<T>` or whole-file `serde_json::Value` DOM.

use std::fmt;
use std::io::{self, BufRead, Read};
use std::marker::PhantomData;

use anyhow::Result;
use serde::Deserialize;
use serde::de::{DeserializeOwned, DeserializeSeed, Deserializer, SeqAccess, Visitor};

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Top-level JSON container kind observed at the start of a stream after
/// stripping an optional leading UTF-8 BOM and JSON whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonContainerKind {
    /// Starts with `[`.
    Array,
    /// Starts with `{`.
    Object,
    /// Starts with some other non-whitespace byte.
    Other(u8),
    /// Reached EOF after optional BOM and whitespace.
    Empty,
}

/// Reader wrapper returned by [`peek_json_container`] that replays any
/// non-BOM prefix bytes before delegating to the underlying `BufRead`.
#[derive(Debug)]
pub struct JsonStreamReader<R> {
    inner: R,
    pending: [u8; 3],
    pending_pos: usize,
    pending_len: usize,
}

impl<R> JsonStreamReader<R> {
    const fn new(inner: R, pending: [u8; 3], pending_len: usize) -> Self {
        Self {
            inner,
            pending,
            pending_pos: 0,
            pending_len,
        }
    }
}

impl<R: BufRead> Read for JsonStreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending_pos < self.pending_len {
            let available = &self.pending[self.pending_pos..self.pending_len];
            let to_copy = available.len().min(buf.len());
            buf[..to_copy].copy_from_slice(&available[..to_copy]);
            self.pending_pos += to_copy;
            return Ok(to_copy);
        }
        self.inner.read(buf)
    }
}

impl<R: BufRead> BufRead for JsonStreamReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pending_pos < self.pending_len {
            return Ok(&self.pending[self.pending_pos..self.pending_len]);
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        if self.pending_pos < self.pending_len {
            let remaining = self.pending_len - self.pending_pos;
            let consumed = amt.min(remaining);
            self.pending_pos += consumed;
            if amt > consumed {
                self.inner.consume(amt - consumed);
            }
        } else {
            self.inner.consume(amt);
        }
    }
}

/// Strip an optional leading UTF-8 BOM (`\u{FEFF}`) and JSON whitespace, then
/// peek the first remaining byte without consuming it or rewinding the stream.
pub fn peek_json_container<R: BufRead>(
    mut reader: R,
) -> io::Result<(JsonContainerKind, JsonStreamReader<R>)> {
    let mut pending = [0u8; 3];
    let mut pending_len = 0usize;

    while pending_len < UTF8_BOM.len() {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let needed = UTF8_BOM.len() - pending_len;
        let matched = available
            .iter()
            .zip(&UTF8_BOM[pending_len..])
            .take_while(|(a, b)| a == b)
            .count();
        if matched == 0 {
            break;
        }
        let available_len = available.len();
        let take = matched.min(needed);
        pending[pending_len..pending_len + take].copy_from_slice(&available[..take]);
        reader.consume(take);
        pending_len += take;
        if take < available_len && pending_len < UTF8_BOM.len() {
            break;
        }
    }

    if pending_len == UTF8_BOM.len() {
        pending_len = 0;
    }

    if pending_len == 0 {
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let available_len = available.len();
            let ws_len = available
                .iter()
                .take_while(|&&byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
                .count();
            if ws_len == 0 {
                break;
            }
            reader.consume(ws_len);
            if ws_len < available_len {
                break;
            }
        }
    }

    let first = if pending_len > 0 {
        Some(pending[0])
    } else {
        reader.fill_buf()?.first().copied()
    };

    let kind = match first {
        None => JsonContainerKind::Empty,
        Some(b'[') => JsonContainerKind::Array,
        Some(b'{') => JsonContainerKind::Object,
        Some(byte) => JsonContainerKind::Other(byte),
    };

    Ok((kind, JsonStreamReader::new(reader, pending, pending_len)))
}

/// Drive a `SeqAccess` one deserialized element at a time through `on_element`,
/// capturing any callback `anyhow::Error` intact in `callback_error`.
pub fn drive_seq_stream<'de, A, T, F>(
    mut seq: A,
    on_element: &mut F,
    callback_error: &mut Option<anyhow::Error>,
) -> std::result::Result<(), A::Error>
where
    A: SeqAccess<'de>,
    T: Deserialize<'de>,
    F: FnMut(T) -> Result<()>,
{
    while let Some(element) = seq.next_element::<T>()? {
        if let Err(err) = on_element(element) {
            *callback_error = Some(err);
            return Err(serde::de::Error::custom(
                "streaming element callback aborted",
            ));
        }
    }
    Ok(())
}

/// `DeserializeSeed` that streams elements of a JSON array one at a time to
/// `on_element` without allocating a `Vec<T>`.
pub struct SeqStreamSeed<'a, T, F> {
    on_element: &'a mut F,
    callback_error: &'a mut Option<anyhow::Error>,
    _marker: PhantomData<fn() -> T>,
}

impl<'a, T, F> SeqStreamSeed<'a, T, F> {
    pub const fn new(on_element: &'a mut F, callback_error: &'a mut Option<anyhow::Error>) -> Self {
        Self {
            on_element,
            callback_error,
            _marker: PhantomData,
        }
    }
}

impl<'de, T, F> DeserializeSeed<'de> for SeqStreamSeed<'_, T, F>
where
    T: Deserialize<'de>,
    F: FnMut(T) -> Result<()>,
{
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SeqVisitor<'a, T, F> {
            on_element: &'a mut F,
            callback_error: &'a mut Option<anyhow::Error>,
            _marker: PhantomData<fn() -> T>,
        }

        impl<'de, T, F> Visitor<'de> for SeqVisitor<'_, T, F>
        where
            T: Deserialize<'de>,
            F: FnMut(T) -> Result<()>,
        {
            type Value = ();

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON array")
            }

            fn visit_seq<A>(self, seq: A) -> std::result::Result<(), A::Error>
            where
                A: SeqAccess<'de>,
            {
                drive_seq_stream(seq, self.on_element, self.callback_error)
            }
        }

        deserializer.deserialize_seq(SeqVisitor {
            on_element: self.on_element,
            callback_error: self.callback_error,
            _marker: PhantomData,
        })
    }
}

/// Stream a top-level JSON array `[elem1, elem2, ...]` from `reader`, invoking
/// `on_element` once per element without materializing a `Vec<T>` or DOM.
///
/// Tolerates a leading UTF-8 BOM and JSON whitespace, rejects trailing
/// non-whitespace garbage after `]`, and returns `Err` on truncated mid-array
/// JSON or underlying I/O errors.
#[allow(dead_code)]
pub fn stream_json_array<R, T, F>(reader: R, mut on_element: F) -> Result<()>
where
    R: BufRead,
    T: DeserializeOwned,
    F: FnMut(T) -> Result<()>,
{
    let (_, stream_reader) = peek_json_container(reader)?;
    stream_json_array_after_peek(stream_reader, &mut on_element)
}

/// Stream a top-level JSON array from a `JsonStreamReader` already returned by
/// [`peek_json_container`].
pub fn stream_json_array_after_peek<R, T, F>(
    stream_reader: JsonStreamReader<R>,
    on_element: &mut F,
) -> Result<()>
where
    R: BufRead,
    T: DeserializeOwned,
    F: FnMut(T) -> Result<()>,
{
    let mut deserializer = serde_json::Deserializer::from_reader(stream_reader);
    let mut callback_error = None;
    let result =
        SeqStreamSeed::<T, _>::new(on_element, &mut callback_error).deserialize(&mut deserializer);
    if let Some(err) = callback_error {
        return Err(err);
    }
    result?;
    deserializer.end()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use anyhow::bail;
    use serde_json::{Value, json};

    use super::*;

    /// Custom `BufRead` yielding one byte per `fill_buf` call to verify
    /// BOM/whitespace peeking and replay across arbitrary chunk boundaries.
    struct OneByteBufRead<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> OneByteBufRead<'a> {
        const fn new(data: &'a [u8]) -> Self {
            Self { data, pos: 0 }
        }
    }

    impl Read for OneByteBufRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let len = available.len().min(buf.len());
            buf[..len].copy_from_slice(&available[..len]);
            self.consume(len);
            Ok(len)
        }
    }

    impl BufRead for OneByteBufRead<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.pos >= self.data.len() {
                Ok(&[])
            } else {
                Ok(&self.data[self.pos..=self.pos])
            }
        }

        fn consume(&mut self, amt: usize) {
            self.pos = (self.pos + amt).min(self.data.len());
        }
    }

    struct FailingTailReader {
        prefix: Cursor<&'static [u8]>,
    }

    impl Read for FailingTailReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.prefix.read(buf)?;
            if n == 0 {
                Err(io::Error::other("simulated stream failure"))
            } else {
                Ok(n)
            }
        }
    }

    #[test]
    fn streams_empty_array_without_invoking_callback() {
        let mut count = 0usize;
        stream_json_array::<_, Value, _>(Cursor::new(b"[]"), |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn streams_single_and_multi_element_arrays_in_order() {
        let mut single = Vec::new();
        stream_json_array::<_, Value, _>(Cursor::new(br#"[{"id":1}]"#), |v| {
            single.push(v);
            Ok(())
        })
        .unwrap();
        assert_eq!(single, vec![json!({"id": 1})]);

        let mut multi = Vec::new();
        stream_json_array::<_, Value, _>(
            Cursor::new(br#"  [ {"id": 1, "text": "a"}, {"id": 2, "text": "b"}, {"id": 3} ]  "#),
            |v| {
                multi.push(v);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            multi,
            vec![
                json!({"id": 1, "text": "a"}),
                json!({"id": 2, "text": "b"}),
                json!({"id": 3}),
            ]
        );
    }

    #[test]
    fn tolerates_leading_utf8_bom_and_whitespace_including_split_chunks() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\xEF\xBB\xBF");
        bytes.extend_from_slice(b" \t\r\n [{\"k\":\"v1\"},{\"k\":\"v2\"}] \n");

        for use_one_byte in [false, true] {
            let mut items = Vec::new();
            if use_one_byte {
                stream_json_array::<_, Value, _>(OneByteBufRead::new(&bytes), |v| {
                    items.push(v);
                    Ok(())
                })
                .unwrap();
            } else {
                stream_json_array::<_, Value, _>(Cursor::new(&bytes), |v| {
                    items.push(v);
                    Ok(())
                })
                .unwrap();
            }
            assert_eq!(items, vec![json!({"k": "v1"}), json!({"k": "v2"})]);
        }
    }

    #[test]
    fn peek_dispatches_array_vs_object_without_rewinding() {
        let mut array_bytes = Vec::from(b"\xEF\xBB\xBF\n  [{\"n\":1},{\"n\":2}]".as_slice());
        let (kind, reader) = peek_json_container(OneByteBufRead::new(&array_bytes)).unwrap();
        assert_eq!(kind, JsonContainerKind::Array);
        let mut seen = Vec::new();
        stream_json_array_after_peek::<_, Value, _>(reader, &mut |v| {
            seen.push(v);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![json!({"n": 1}), json!({"n": 2})]);

        array_bytes.clear();
        array_bytes.extend_from_slice(b"\xEF\xBB\xBF  \t{\"session\":\"s1\",\"items\":[1,2]}");
        let (kind, reader) = peek_json_container(OneByteBufRead::new(&array_bytes)).unwrap();
        assert_eq!(kind, JsonContainerKind::Object);
        let obj: Value = serde_json::from_reader(reader).unwrap();
        assert_eq!(obj, json!({"session": "s1", "items": [1, 2]}));

        let (empty_kind, _) = peek_json_container(Cursor::new(b"\xEF\xBB\xBF  \r\n\t")).unwrap();
        assert_eq!(empty_kind, JsonContainerKind::Empty);

        // Partial BOM bytes must be replayed verbatim, never dropped.
        let (partial_kind, mut partial_reader) =
            peek_json_container(OneByteBufRead::new(b"\xEF\x00")).unwrap();
        assert_eq!(partial_kind, JsonContainerKind::Other(0xEF));
        let mut replayed = Vec::new();
        partial_reader.read_to_end(&mut replayed).unwrap();
        assert_eq!(replayed, b"\xEF\x00");
    }

    #[test]
    fn truncated_mid_array_and_trailing_garbage_return_err() {
        for bad in [
            b"".as_slice(),
            b"   \n",
            b"\xEF\xBB\xBF",
            b"[",
            b"[{\"a\":1}",
            b"[{\"a\":1},",
            b"[{\"a\":1},{\"a\":",
            b"[{\"a\":1}] trailing",
            b"{\"a\":1}",
        ] {
            let res = stream_json_array::<_, Value, _>(Cursor::new(bad), |_| Ok(()));
            assert!(
                res.is_err(),
                "expected Err for invalid/truncated JSON {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn callback_error_and_late_io_error_are_propagated() {
        let mut count = 0usize;
        let err = stream_json_array::<_, Value, _>(Cursor::new(b"[1, 2, 3]"), |v| {
            count += 1;
            if v == json!(2) {
                bail!("stop at two");
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(count, 2);
        assert!(err.to_string().contains("stop at two"));

        let reader = BufReader::new(FailingTailReader {
            prefix: Cursor::new(b"[1, 2]"),
        });
        let err = stream_json_array::<_, Value, _>(reader, |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("simulated stream failure"));
    }
}
