//! Read one bounded rollout snapshot without certifying incomplete input.

use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Take};
#[cfg(feature = "codex-zstd")]
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::Value;

use super::super::utils::MAX_SCAN_FILE_BYTES;

const PROGRESS_LINE_STRIDE: usize = 1024;
/// Maximum single JSONL line buffered by [`Records`] before draining to the
/// next newline to bound memory on malformed or huge records.
const MAX_RECORD_LINE_BYTES: usize = 64 * 1024 * 1024;
/// Scratch buffer capacity threshold above which [`Records`] shrinks its
/// reusable byte buffer back down after processing a large line.
const MAX_REUSABLE_LINE_CAPACITY: usize = 256 * 1024;
/// Maximum zstd back-reference window (`2^26` = 64 MiB), matching
/// `agentsview`'s `WithDecoderMaxWindow(64<<20)`, so corrupt or adversarial
/// frame headers cannot force huge decoder window allocations.
#[cfg(feature = "codex-zstd")]
const ZSTD_WINDOW_LOG_MAX: u32 = 26;
#[cfg(feature = "codex-zstd")]
const ZSTD_FRAME_MAGIC: u32 = 0xFD2F_B528;
#[cfg(feature = "codex-zstd")]
const ZSTD_SKIPPABLE_MAGIC_MIN: u32 = 0x184D_2A50;
#[cfg(feature = "codex-zstd")]
const ZSTD_SKIPPABLE_MAGIC_MAX: u32 = 0x184D_2A5F;

/// Largest Codex rollout the primary reader admits. The reader streams records
/// through a length-bounded handle, so this is an admission budget, not a
/// memory bound. It defaults to the shared 100 MiB scan budget; an embedder
/// that admits larger rollouts (cass: `CASS_CODEX_MAX_SOURCE_BYTES`) raises it.
static ROLLOUT_BYTE_BUDGET: AtomicU64 = AtomicU64::new(MAX_SCAN_FILE_BYTES);

/// Set the largest Codex rollout (bytes) the primary reader admits, process-wide.
/// Values below one byte are treated as one byte.
pub fn set_codex_rollout_byte_budget(bytes: u64) {
    ROLLOUT_BYTE_BUDGET.store(bytes.max(1), Ordering::Relaxed);
}

/// The current Codex rollout admission budget (bytes).
#[must_use]
pub fn codex_rollout_byte_budget() -> u64 {
    ROLLOUT_BYTE_BUDGET.load(Ordering::Relaxed)
}

fn admit_rollout_len(len: u64, budget: u64) -> io::Result<()> {
    if len > budget {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Codex rollout ({len} bytes) exceeds the {budget}-byte scan budget"),
        ));
    }
    Ok(())
}

pub(super) struct RolloutReader<'a> {
    path: &'a Path,
    /// The opened file, for the end-of-read snapshot check.
    file: File,
    before: Metadata,
    /// A compressed rollout's records are its decoded text, so the decoder's
    /// clean end, not the file length, says the whole file was read.
    compressed: bool,
    records: Records<'a, Box<dyn BufRead + 'a>>,
    finished: bool,
}

impl<'a> RolloutReader<'a> {
    pub(super) fn open(
        path: &'a Path,
        compressed: bool,
        progress_tick: Option<&'a (dyn Fn() + Send + Sync)>,
    ) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("open Codex rollout {}", path.display()))?;
        let before = file.metadata().context("inspect opened Codex rollout")?;
        if !before.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Codex rollout is not a regular file",
            )
            .into());
        }
        let budget = codex_rollout_byte_budget();
        admit_rollout_len(before.len(), budget)?;
        // Bound the underlying reader, not the result of read_line: even a
        // newline-free record or a concurrent appender cannot grow it forever.
        let source = file
            .try_clone()
            .context("duplicate opened Codex rollout handle")?
            .take(before.len());
        let input: Box<dyn BufRead + 'a> = if compressed {
            Box::new(
                compressed_input(source, budget)
                    .context("start decoding compressed Codex rollout")?,
            )
        } else {
            Box::new(BufReader::new(source))
        };
        Ok(Self {
            path,
            file,
            before,
            compressed,
            records: Records::new(input, progress_tick),
            finished: false,
        })
    }

    pub(super) fn next_record(&mut self) -> Result<Option<(usize, Value)>> {
        if self.finished {
            return Ok(None);
        }
        let record = self
            .records
            .next_record()
            .with_context(|| format!("read Codex rollout {}", self.path.display()))?;
        if record.is_none() {
            // The caller accumulates a source privately until this validated
            // EOF. Nothing from a failed source may reach its consumer sink.
            self.validate_snapshot()?;
            self.finished = true;
        }
        Ok(record)
    }

    fn validate_snapshot(&self) -> Result<()> {
        // The decoder ends cleanly only after a complete frame with no input
        // left, so a compressed rollout cut short has already failed.
        if !self.compressed && self.records.bytes_read != self.before.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Codex rollout was truncated while reading; retry this source",
            )
            .into());
        }
        let after = self
            .file
            .metadata()
            .context("recheck opened Codex rollout")?;
        let named = fs::metadata(self.path).context("recheck Codex rollout path")?;
        if !same_snapshot(&self.before, &after)? || !same_snapshot(&self.before, &named)? {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Codex rollout changed while reading; retry this source",
            )
            .into());
        }
        Ok(())
    }
}

/// The decoded text of a compressed rollout (`rollout-*.jsonl.zst`, which
/// Codex writes when its `local_thread_store_compression` feature compresses
/// a finished session), read from `compressed`.
///
/// Reading fails, instead of ending early, when the decoded text passes
/// `budget` bytes, when the stream is cut short mid-frame (an
/// [`io::ErrorKind::UnexpectedEof`]) and when the bytes are not zstd, so a
/// caller never takes part of a session for all of it. Past the budget the
/// error kind is [`io::ErrorKind::FileTooLarge`].
///
/// # Errors
///
/// Returns an error when the decoder cannot be created.
#[cfg(feature = "codex-zstd")]
pub fn decompressed_rollout<R: Read>(compressed: R, budget: u64) -> io::Result<impl BufRead> {
    let mut decoder = zstd::stream::read::Decoder::new(compressed)?;
    decoder.window_log_max(ZSTD_WINDOW_LOG_MAX)?;
    Ok(BufReader::new(DecodedBudget {
        inner: decoder,
        decoded: 0,
        budget,
    }))
}

/// The total decoded length declared across all zstd data frames in `source`.
///
/// Walks RFC 8878 frame headers, skippable frames (`0x184D2A50..=0x184D2A5F`),
/// and 3-byte block headers from the current position of `source` without
/// decompressing, then restores `source` to where it was. Sums declared
/// `Frame_Content_Size` across all data frames when every data frame declares
/// one. Returns `Ok(None)` when any data frame omits `Frame_Content_Size`,
/// when `source` is empty, or when `source` does not start with a zstd/skippable
/// frame magic.
///
/// # Errors
///
/// Returns an error when `source` cannot be read or repositioned, when a frame
/// or skippable header/block/checksum is truncated before `Block_Last`, or when
/// invalid frame/block metadata or trailing non-frame bytes follow a frame.
#[cfg(feature = "codex-zstd")]
pub fn compressed_rollout_declared_len<R: Read + Seek>(source: &mut R) -> io::Result<Option<u64>> {
    let start = source.stream_position()?;
    let result = walk_zstd_declared_len(source, start);
    source.seek(SeekFrom::Start(start))?;
    result
}

#[cfg(feature = "codex-zstd")]
fn is_zstd_magic_prefix(prefix: &[u8]) -> bool {
    const ZSTD_MAGIC_LE: [u8; 4] = ZSTD_FRAME_MAGIC.to_le_bytes();
    if prefix.is_empty() || prefix.len() >= 4 {
        return false;
    }
    if prefix == &ZSTD_MAGIC_LE[..prefix.len()] {
        return true;
    }
    (0x50..=0x5F).contains(&prefix[0]) && prefix[1..] == [0x2A, 0x4D][..prefix.len() - 1]
}

#[cfg(feature = "codex-zstd")]
fn parse_frame_content_size(fcs_flag: u8, single_segment: bool, fcs_bytes: &[u8]) -> Option<u64> {
    match fcs_flag {
        0 => single_segment.then(|| u64::from(fcs_bytes[0])),
        1 => Some(u64::from(u16::from_le_bytes([fcs_bytes[0], fcs_bytes[1]])) + 256),
        2 => Some(u64::from(u32::from_le_bytes([
            fcs_bytes[0],
            fcs_bytes[1],
            fcs_bytes[2],
            fcs_bytes[3],
        ]))),
        3 => Some(u64::from_le_bytes(
            fcs_bytes.try_into().expect("8-byte frame content size"),
        )),
        _ => unreachable!(),
    }
}

#[cfg(feature = "codex-zstd")]
fn walk_zstd_data_frame<R: Read + Seek>(
    source: &mut R,
    pos: &mut u64,
    end: u64,
) -> io::Result<Option<u64>> {
    if end - *pos < 1 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated zstd frame header descriptor",
        ));
    }
    let mut fhd_buf = [0u8; 1];
    source.read_exact(&mut fhd_buf)?;
    *pos += 1;
    let fhd = fhd_buf[0];
    if (fhd & 0x18) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reserved bit set in zstd frame header descriptor",
        ));
    }
    let fcs_flag = fhd >> 6;
    let single_segment = (fhd & 0x20) != 0;
    let has_checksum = (fhd & 0x04) != 0;
    let did_bytes = match fhd & 0x03 {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        _ => unreachable!(),
    };
    let window_bytes = usize::from(!single_segment);
    let fcs_bytes = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        3 => 8,
        _ => unreachable!(),
    };
    let rest_len = window_bytes + did_bytes + fcs_bytes;
    if (rest_len as u64) > end - *pos {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated zstd frame header",
        ));
    }
    let mut rest_buf = [0u8; 13];
    source.read_exact(&mut rest_buf[..rest_len])?;
    *pos += rest_len as u64;
    let frame_fcs = parse_frame_content_size(
        fcs_flag,
        single_segment,
        &rest_buf[window_bytes + did_bytes..rest_len],
    );

    loop {
        if end - *pos < 3 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated zstd block header",
            ));
        }
        let mut bh = [0u8; 3];
        source.read_exact(&mut bh)?;
        *pos += 3;
        let header = u32::from(bh[0]) | (u32::from(bh[1]) << 8) | (u32::from(bh[2]) << 16);
        let last_block = (header & 1) != 0;
        let block_type = (header >> 1) & 0x03;
        if block_type == 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reserved zstd block type",
            ));
        }
        let payload = if block_type == 1 {
            // RLE blocks carry a single byte on the wire regardless of regenerated size.
            1u64
        } else {
            u64::from(header >> 3)
        };
        if payload > end - *pos {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated zstd block payload",
            ));
        }
        *pos += payload;
        if payload > 0 {
            source.seek(SeekFrom::Start(*pos))?;
        }
        if last_block {
            break;
        }
    }

    if has_checksum {
        if end - *pos < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated zstd frame checksum",
            ));
        }
        let mut checksum = [0u8; 4];
        source.read_exact(&mut checksum)?;
        *pos += 4;
    }

    Ok(frame_fcs)
}

#[cfg(feature = "codex-zstd")]
fn walk_zstd_declared_len<R: Read + Seek>(source: &mut R, start: u64) -> io::Result<Option<u64>> {
    let end = source.seek(SeekFrom::End(0))?;
    source.seek(SeekFrom::Start(start))?;
    let mut pos = start;
    if pos >= end {
        return Ok(None);
    }

    let mut saw_any_frame = false;
    let mut saw_data_frame = false;
    let mut all_declared = true;
    let mut total_declared: u64 = 0;

    while pos < end {
        if end - pos < 4 {
            if !saw_any_frame {
                let mut prefix = [0u8; 4];
                let rem = usize::try_from(end - pos).expect("remaining bytes < 4 fit in usize");
                source.read_exact(&mut prefix[..rem])?;
                if !is_zstd_magic_prefix(&prefix[..rem]) {
                    return Ok(None);
                }
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated zstd frame magic",
            ));
        }

        let mut magic_bytes = [0u8; 4];
        source.read_exact(&mut magic_bytes)?;
        pos += 4;
        let magic = u32::from_le_bytes(magic_bytes);

        if (ZSTD_SKIPPABLE_MAGIC_MIN..=ZSTD_SKIPPABLE_MAGIC_MAX).contains(&magic) {
            saw_any_frame = true;
            if end - pos < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated zstd skippable frame header",
                ));
            }
            let mut size_bytes = [0u8; 4];
            source.read_exact(&mut size_bytes)?;
            pos += 4;
            let payload = u64::from(u32::from_le_bytes(size_bytes));
            if payload > end - pos {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated zstd skippable frame payload",
                ));
            }
            pos += payload;
            if payload > 0 {
                source.seek(SeekFrom::Start(pos))?;
            }
        } else if magic == ZSTD_FRAME_MAGIC {
            saw_any_frame = true;
            saw_data_frame = true;
            match walk_zstd_data_frame(source, &mut pos, end)? {
                Some(fcs) if all_declared => {
                    total_declared = total_declared.checked_add(fcs).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "declared zstd content size overflows u64",
                        )
                    })?;
                }
                _ => {
                    all_declared = false;
                }
            }
        } else if !saw_any_frame {
            return Ok(None);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid zstd frame magic",
            ));
        }
    }

    if saw_data_frame && all_declared {
        Ok(Some(total_declared))
    } else {
        Ok(None)
    }
}

/// A rollout's path without its format suffix (`.jsonl`, `.json` or
/// `.jsonl.zst`). It names the session in every form, so a session Codex
/// compresses after it was indexed keeps its id.
#[must_use]
pub fn rollout_session_path(path: &Path) -> std::path::PathBuf {
    let path = if is_compressed_rollout(path) {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    path.with_extension("")
}

/// True for a rollout Codex compressed: `rollout-*.jsonl.zst`.
#[must_use]
pub fn is_compressed_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("rollout-") && name.to_ascii_lowercase().ends_with(".jsonl.zst")
        })
}

#[cfg(feature = "codex-zstd")]
fn compressed_input(mut compressed: Take<File>, budget: u64) -> io::Result<impl BufRead> {
    // A length the header declares past the budget is refused before any
    // decoding, with the same error a plain rollout that long gets.
    if let Some(declared) = compressed_rollout_declared_len(compressed.get_mut())? {
        admit_rollout_len(declared, budget)?;
    }
    decompressed_rollout(compressed, budget)
}

#[cfg(not(feature = "codex-zstd"))]
fn compressed_input(_compressed: Take<File>, _budget: u64) -> io::Result<io::Empty> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "reading a compressed Codex rollout needs the `codex-zstd` feature",
    ))
}

/// Fails once the decoded text passes the budget: an early end would read
/// as a complete, shorter session.
#[cfg(feature = "codex-zstd")]
struct DecodedBudget<R> {
    inner: R,
    decoded: u64,
    budget: u64,
}

#[cfg(feature = "codex-zstd")]
impl<R: Read> Read for DecodedBudget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.decoded = self.decoded.saturating_add(count as u64);
        if self.decoded > self.budget {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "decompressed Codex rollout exceeds the {}-byte scan budget",
                    self.budget
                ),
            ));
        }
        Ok(count)
    }
}

// Observable snapshot consistency, not protection against malicious edits
// that preserve metadata. Unix additionally checks the opened object's ID.
fn same_snapshot(before: &Metadata, after: &Metadata) -> io::Result<bool> {
    let same =
        after.is_file() && before.len() == after.len() && before.modified()? == after.modified()?;
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        same && before.dev() == after.dev() && before.ino() == after.ino()
    };
    Ok(same)
}

struct Records<'a, R> {
    input: R,
    line_buf: Vec<u8>,
    line_number: usize,
    bytes_read: u64,
    progress_tick: Option<&'a (dyn Fn() + Send + Sync)>,
}

impl<'a, R: BufRead> Records<'a, R> {
    const fn new(input: R, progress_tick: Option<&'a (dyn Fn() + Send + Sync)>) -> Self {
        Self {
            input,
            line_buf: Vec::new(),
            line_number: 0,
            bytes_read: 0,
            progress_tick,
        }
    }

    fn shrink_scratch_if_large(&mut self) {
        self.line_buf.clear();
        if self.line_buf.capacity() > MAX_REUSABLE_LINE_CAPACITY {
            self.line_buf.shrink_to(MAX_REUSABLE_LINE_CAPACITY);
        }
    }

    /// Reads bytes up to `\n` or EOF into `self.line_buf`, capping buffered
    /// storage at `MAX_RECORD_LINE_BYTES` and draining any remainder to `\n`
    /// without growing memory further.
    fn read_bounded_line(&mut self) -> io::Result<(usize, bool, bool)> {
        self.line_buf.clear();
        let mut total_bytes: usize = 0;
        let mut exceeded = false;
        loop {
            let available = match self.input.fill_buf() {
                Ok(buf) => buf,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            if available.is_empty() {
                return Ok((total_bytes, false, exceeded));
            }
            let (take_len, found_nl) = available
                .iter()
                .position(|&b| b == b'\n')
                .map_or((available.len(), false), |idx| (idx + 1, true));
            total_bytes = total_bytes.saturating_add(take_len);
            if !exceeded {
                if self.line_buf.len().saturating_add(take_len) <= MAX_RECORD_LINE_BYTES {
                    self.line_buf.extend_from_slice(&available[..take_len]);
                } else {
                    exceeded = true;
                    self.line_buf.clear();
                }
            }
            self.input.consume(take_len);
            if found_nl {
                return Ok((total_bytes, true, exceeded));
            }
        }
    }

    fn next_record(&mut self) -> Result<Option<(usize, Value)>> {
        loop {
            if self.line_number % PROGRESS_LINE_STRIDE == 0 {
                if let Some(tick) = self.progress_tick {
                    tick();
                }
            }
            let (count, terminated, exceeded) = self
                .read_bounded_line()
                .with_context(|| format!("read Codex JSONL line {}", self.line_number + 1))?;
            if count == 0 {
                self.shrink_scratch_if_large();
                return Ok(None);
            }
            self.bytes_read += count as u64;
            let index = self.line_number;
            self.line_number += 1;
            if exceeded {
                self.shrink_scratch_if_large();
                continue;
            }
            let (decoded, had_invalid_utf8) = match std::str::from_utf8(&self.line_buf) {
                Ok(valid) => (std::borrow::Cow::Borrowed(valid), false),
                Err(_) => (String::from_utf8_lossy(&self.line_buf), true),
            };
            let text = decoded.trim_start_matches('\u{feff}').trim();
            if text.is_empty() {
                self.shrink_scratch_if_large();
                continue;
            }
            let parsed = serde_json::from_str(text);
            self.shrink_scratch_if_large();
            match parsed {
                Ok(value) => return Ok(Some((index, value))),
                Err(error) if !terminated && error.is_eof() => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "unfinished Codex JSONL record at line {}; retry this source",
                            self.line_number
                        ),
                    )
                    .into());
                }
                Err(_) if had_invalid_utf8 => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid UTF-8 in Codex JSONL line {}; retry this source",
                            self.line_number
                        ),
                    )
                    .into());
                }
                // Keep the existing malformed-historical-record policy. Only
                // an unfinished final record, an unrecoverable invalid-UTF-8
                // line, or an actual read failure aborts. Never include the
                // record's potentially private text in errors.
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn rollout_admission_is_bounded_by_the_configured_budget() {
        assert_eq!(
            codex_rollout_byte_budget(),
            MAX_SCAN_FILE_BYTES,
            "the default budget is the shared scan budget"
        );
        assert!(admit_rollout_len(MAX_SCAN_FILE_BYTES, MAX_SCAN_FILE_BYTES).is_ok());
        let over = admit_rollout_len(MAX_SCAN_FILE_BYTES + 1, MAX_SCAN_FILE_BYTES)
            .expect_err("one byte over the budget is refused");
        assert_eq!(over.kind(), io::ErrorKind::InvalidData);
        // A raised budget admits a rollout the default refuses.
        assert!(admit_rollout_len(MAX_SCAN_FILE_BYTES + 1, 4 * MAX_SCAN_FILE_BYTES).is_ok());
    }

    #[test]
    fn records_keep_physical_indices_bom_crlf_and_complete_eof() {
        let bytes = "\u{feff}{\"first\":true}\r\n\nnot-json\n{\"last\":true}";
        let mut records = Records::new(Cursor::new(bytes), None);
        assert_eq!(
            records.next_record().unwrap(),
            Some((0, serde_json::json!({"first":true})))
        );
        assert_eq!(
            records.next_record().unwrap(),
            Some((3, serde_json::json!({"last":true})))
        );
        assert!(records.next_record().unwrap().is_none());
        assert_eq!(records.bytes_read, bytes.len() as u64);
    }

    #[test]
    fn unfinished_tail_is_not_successful_eof_or_a_content_disclosure() {
        let mut records = Records::new(Cursor::new("{}\n{\"private_marker\":"), None);
        assert!(records.next_record().unwrap().is_some());
        let error = records.next_record().unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(!error.to_string().contains("private_marker"));
    }

    #[test]
    fn invalid_utf8_propagates_instead_of_skipping_a_record() {
        let mut records = Records::new(Cursor::new(&b"{}\n\xff\n{}\n"[..]), None);
        assert!(records.next_record().unwrap().is_some());
        let error = records.next_record().unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn invalid_utf8_inside_json_string_is_recovered_lossily() {
        let line0 = b"{\"type\":\"response_item\",\"payload\":{\"output\":\"ok-\xff\xfe-tail\"}}\n";
        let line1 = b"{\"ok\":true}\n";
        let mut bytes = Vec::with_capacity(line0.len() + line1.len());
        bytes.extend_from_slice(line0);
        bytes.extend_from_slice(line1);
        let mut records = Records::new(Cursor::new(bytes.as_slice()), None);
        assert_eq!(
            records.next_record().unwrap(),
            Some((
                0,
                serde_json::json!({
                    "type": "response_item",
                    "payload": { "output": "ok-\u{FFFD}\u{FFFD}-tail" }
                })
            ))
        );
        assert_eq!(
            records.next_record().unwrap(),
            Some((1, serde_json::json!({"ok": true})))
        );
        assert!(records.next_record().unwrap().is_none());
        assert_eq!(records.bytes_read, bytes.len() as u64);
    }

    #[test]
    fn large_valid_line_shrinks_reusable_scratch_buffer() {
        let big_payload = "x".repeat(512 * 1024);
        let input = format!("{{\"big\":\"{big_payload}\"}}\n{{\"small\":true}}\n");
        let mut records = Records::new(Cursor::new(input.as_bytes()), None);
        assert_eq!(
            records.next_record().unwrap(),
            Some((0, serde_json::json!({"big": big_payload})))
        );
        assert!(
            records.line_buf.capacity() <= MAX_REUSABLE_LINE_CAPACITY,
            "scratch buffer capacity {} should shrink to <= {}",
            records.line_buf.capacity(),
            MAX_REUSABLE_LINE_CAPACITY
        );
        assert_eq!(
            records.next_record().unwrap(),
            Some((1, serde_json::json!({"small": true})))
        );
        assert!(records.next_record().unwrap().is_none());
        assert_eq!(records.bytes_read, input.len() as u64);
    }

    #[test]
    fn oversized_single_line_is_drained_without_oom_and_scratch_shrinks() {
        struct OversizedLineReader {
            prefix: &'static [u8],
            prefix_pos: usize,
            filler_remaining: usize,
            suffix: &'static [u8],
            suffix_pos: usize,
        }
        impl Read for OversizedLineReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.prefix_pos < self.prefix.len() {
                    let n = buf.len().min(self.prefix.len() - self.prefix_pos);
                    buf[..n].copy_from_slice(&self.prefix[self.prefix_pos..self.prefix_pos + n]);
                    self.prefix_pos += n;
                    return Ok(n);
                }
                if self.filler_remaining > 0 {
                    let n = buf.len().min(self.filler_remaining);
                    buf[..n].fill(b'a');
                    self.filler_remaining -= n;
                    return Ok(n);
                }
                if self.suffix_pos < self.suffix.len() {
                    let n = buf.len().min(self.suffix.len() - self.suffix_pos);
                    buf[..n].copy_from_slice(&self.suffix[self.suffix_pos..self.suffix_pos + n]);
                    self.suffix_pos += n;
                    return Ok(n);
                }
                Ok(0)
            }
        }

        let filler_len = MAX_RECORD_LINE_BYTES + 4096;
        let prefix = b"{\"oversized\":\"";
        let suffix = b"\"}\n{\"after\":true}\n";
        let expected_total = (prefix.len() + filler_len + suffix.len()) as u64;
        let reader = OversizedLineReader {
            prefix,
            prefix_pos: 0,
            filler_remaining: filler_len,
            suffix,
            suffix_pos: 0,
        };
        let mut records = Records::new(BufReader::new(reader), None);
        assert_eq!(
            records.next_record().unwrap(),
            Some((1, serde_json::json!({"after": true})))
        );
        assert!(
            records.line_buf.capacity() <= MAX_REUSABLE_LINE_CAPACITY,
            "scratch buffer capacity {} should shrink to <= {}",
            records.line_buf.capacity(),
            MAX_REUSABLE_LINE_CAPACITY
        );
        assert!(records.next_record().unwrap().is_none());
        assert_eq!(records.bytes_read, expected_total);
    }

    #[test]
    fn read_failure_after_a_valid_prefix_is_not_successful_eof() {
        struct Failing(Cursor<Vec<u8>>);
        impl Read for Failing {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    Err(io::Error::other("injected read failure"))
                } else {
                    self.0.read(buffer)
                }
            }
        }
        let mut records =
            Records::new(BufReader::new(Failing(Cursor::new(b"{}\n".to_vec()))), None);
        assert!(records.next_record().unwrap().is_some());
        let error = records.next_record().unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn every_rollout_form_names_the_same_session() {
        for name in [
            "d/rollout-x.jsonl",
            "d/rollout-x.json",
            "d/rollout-x.jsonl.zst",
            "d/rollout-x.JSONL.ZST",
        ] {
            assert_eq!(
                rollout_session_path(Path::new(name)),
                Path::new("d/rollout-x"),
                "{name}"
            );
        }
        // Only `.jsonl.zst` loses two suffixes.
        assert_eq!(
            rollout_session_path(Path::new("d/rollout-x.json.zst")),
            Path::new("d/rollout-x.json")
        );
    }

    #[cfg(feature = "codex-zstd")]
    mod compressed {
        use super::*;
        use std::io::{Seek, Write};

        const TEXT: &str = "\u{feff}{\"first\":true}\r\n\nnot-json\n{\"last\":true}\n";

        /// Compressed the way Codex compresses a rollout: one frame that
        /// records the decoded length.
        fn declared(text: &[u8]) -> Vec<u8> {
            zstd::bulk::compress(text, 3).unwrap()
        }

        /// One frame that does not record the decoded length.
        fn undeclared(text: &[u8]) -> Vec<u8> {
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
            encoder.include_contentsize(false).unwrap();
            encoder.write_all(text).unwrap();
            encoder.finish().unwrap()
        }

        fn records<R: BufRead>(input: R) -> Result<Vec<(usize, Value)>> {
            let mut records = Records::new(input, None);
            let mut out = Vec::new();
            while let Some(record) = records.next_record()? {
                out.push(record);
            }
            Ok(out)
        }

        fn read_file(path: &Path) -> Result<Vec<(usize, Value)>> {
            let mut reader = RolloutReader::open(path, true, None)?;
            let mut out = Vec::new();
            while let Some(record) = reader.next_record()? {
                out.push(record);
            }
            Ok(out)
        }

        fn io_kind(error: &anyhow::Error) -> Option<io::ErrorKind> {
            error.downcast_ref::<io::Error>().map(io::Error::kind)
        }

        #[test]
        fn a_compressed_rollout_reads_as_the_records_of_its_text() {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-x.jsonl.zst");
            fs::write(&path, declared(TEXT.as_bytes())).unwrap();
            let plain = records(Cursor::new(TEXT)).unwrap();
            assert_eq!(plain.len(), 2);
            assert_eq!(read_file(&path).unwrap(), plain);
            fs::write(&path, undeclared(TEXT.as_bytes())).unwrap();
            assert_eq!(read_file(&path).unwrap(), plain);
        }

        #[test]
        fn the_declared_length_is_read_without_moving_the_source() {
            let mut source = Cursor::new(declared(TEXT.as_bytes()));
            assert_eq!(
                compressed_rollout_declared_len(&mut source).unwrap(),
                Some(TEXT.len() as u64)
            );
            assert_eq!(source.stream_position().unwrap(), 0);
            let mut source = Cursor::new(undeclared(TEXT.as_bytes()));
            assert_eq!(compressed_rollout_declared_len(&mut source).unwrap(), None);
            let mut source = Cursor::new(TEXT.as_bytes().to_vec());
            assert_eq!(compressed_rollout_declared_len(&mut source).unwrap(), None);
        }

        #[test]
        fn a_cut_short_compressed_rollout_fails_as_unfinished() {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-x.jsonl.zst");
            let packed = declared("{\"a\":1}\n".repeat(20_000).as_bytes());
            fs::write(&path, &packed[..packed.len() / 2]).unwrap();
            let error = read_file(&path).unwrap_err();
            assert_eq!(
                io_kind(&error),
                Some(io::ErrorKind::UnexpectedEof),
                "{error:#}"
            );
            // An empty file is a compressed rollout not yet written.
            fs::write(&path, b"").unwrap();
            let error = read_file(&path).unwrap_err();
            assert_eq!(
                io_kind(&error),
                Some(io::ErrorKind::UnexpectedEof),
                "{error:#}"
            );
        }

        #[test]
        fn bytes_that_are_not_one_whole_zstd_stream_fail() {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-x.jsonl.zst");
            fs::write(&path, TEXT).unwrap();
            assert!(read_file(&path).is_err(), "plain text named .jsonl.zst");
            let mut trailing = declared(TEXT.as_bytes());
            trailing.extend_from_slice(b"not a frame");
            fs::write(&path, trailing).unwrap();
            assert!(read_file(&path).is_err(), "bytes after the frame");
        }

        #[test]
        fn concatenated_frames_read_as_one_text_and_a_cut_later_frame_fails() {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-x.jsonl.zst");
            let rest = "{\"a\":1}\n".repeat(20_000);
            let later = undeclared(rest.as_bytes());
            let mut packed = declared(TEXT.as_bytes());
            packed.extend_from_slice(&later);
            fs::write(&path, &packed).unwrap();
            let joined = format!("{TEXT}{rest}");
            assert_eq!(
                read_file(&path).unwrap(),
                records(Cursor::new(joined)).unwrap()
            );

            // Cut inside the second frame: unfinished, never the first frame's
            // records passed off as the whole session.
            fs::write(&path, &packed[..packed.len() - later.len() / 2]).unwrap();
            let error = read_file(&path).unwrap_err();
            assert_eq!(
                io_kind(&error),
                Some(io::ErrorKind::UnexpectedEof),
                "{error:#}"
            );
        }

        #[test]
        fn decoded_text_is_held_to_the_budget() {
            let text = "{\"a\":1}\n".repeat(1_000);
            let len = text.len() as u64;
            let mut decoded = Vec::new();
            decompressed_rollout(Cursor::new(undeclared(text.as_bytes())), len)
                .unwrap()
                .read_to_end(&mut decoded)
                .expect("text exactly at the budget is admitted");
            assert_eq!(decoded, text.as_bytes());

            let error = decompressed_rollout(Cursor::new(undeclared(text.as_bytes())), len - 1)
                .unwrap()
                .read_to_end(&mut Vec::new())
                .expect_err("one byte past the budget fails, never a shorter text");
            assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        }

        #[test]
        fn a_declared_length_past_the_budget_is_refused_before_decoding() {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-x.jsonl.zst");
            let text = "{\"a\":1}\n".repeat(1_000);
            fs::write(&path, declared(text.as_bytes())).unwrap();
            let file = File::open(&path).unwrap();
            let len = file.metadata().unwrap().len();
            let Err(error) = compressed_input(file.take(len), text.len() as u64 - 1) else {
                panic!("a declared length past the budget is refused");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }

        fn skippable_frame(magic: u32, payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(8 + payload.len());
            out.extend_from_slice(&magic.to_le_bytes());
            out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
            out.extend_from_slice(payload);
            out
        }

        /// Builds a minimal RFC 8878 single-raw-block zstd frame with explicit
        /// `Window_Descriptor` exponent (`windowLog = 10 + exponent`), optional
        /// 4-byte checksum flag, and no `Frame_Content_Size`.
        fn raw_frame_with_window_log(window_log: u8, payload: &[u8], checksum: bool) -> Vec<u8> {
            assert!(window_log >= 10);
            assert!(payload.len() < (1 << 21));
            let exponent = window_log - 10;
            let fhd = if checksum { 0x04 } else { 0x00 };
            let window_desc = exponent << 3;
            let block_header = 1u32 | (u32::try_from(payload.len()).unwrap() << 3);
            let mut out = Vec::with_capacity(9 + payload.len() + if checksum { 4 } else { 0 });
            out.extend_from_slice(&ZSTD_FRAME_MAGIC.to_le_bytes());
            out.push(fhd);
            out.push(window_desc);
            out.extend_from_slice(&block_header.to_le_bytes()[..3]);
            out.extend_from_slice(payload);
            if checksum {
                out.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
            }
            out
        }

        #[test]
        fn skippable_frame_prefix_and_multi_frame_sum_declared_lengths() {
            let part1 = TEXT.to_string();
            // >256 bytes exercises the 2-byte Frame_Content_Size (+256 offset) path.
            let part2 = "{\"second\":true}\n".repeat(30);
            let mut stream = Vec::new();
            stream.extend_from_slice(&skippable_frame(ZSTD_SKIPPABLE_MAGIC_MIN, b"meta-header!"));
            stream.extend_from_slice(&declared(part1.as_bytes()));
            stream.extend_from_slice(&skippable_frame(ZSTD_SKIPPABLE_MAGIC_MAX, b""));
            stream.extend_from_slice(&declared(part2.as_bytes()));

            let expected_len = (part1.len() + part2.len()) as u64;
            let mut source = Cursor::new(stream.clone());
            assert_eq!(
                compressed_rollout_declared_len(&mut source).unwrap(),
                Some(expected_len)
            );
            assert_eq!(source.stream_position().unwrap(), 0);

            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-multi.jsonl.zst");
            fs::write(&path, &stream).unwrap();
            let joined = format!("{part1}{part2}");
            assert_eq!(
                read_file(&path).unwrap(),
                records(Cursor::new(joined)).unwrap()
            );

            // Summed declared length over budget is refused before decoding.
            let file = File::open(&path).unwrap();
            let len = file.metadata().unwrap().len();
            let Err(error) = compressed_input(file.take(len), expected_len - 1) else {
                panic!("multi-frame declared length past budget must be refused");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }

        #[test]
        fn multi_frame_omitted_content_size_returns_none_and_enforces_streaming_budget() {
            let part1 = TEXT.as_bytes();
            let part2 = b"{\"second\":true}\n";

            let mut declared_then_undeclared = declared(part1);
            declared_then_undeclared.extend_from_slice(&undeclared(part2));
            let mut source = Cursor::new(declared_then_undeclared.clone());
            assert_eq!(compressed_rollout_declared_len(&mut source).unwrap(), None);

            let mut undeclared_then_declared = undeclared(part1);
            undeclared_then_declared.extend_from_slice(&declared(part2));
            let mut source = Cursor::new(undeclared_then_declared);
            assert_eq!(compressed_rollout_declared_len(&mut source).unwrap(), None);

            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("rollout-mixed.jsonl.zst");
            fs::write(&path, &declared_then_undeclared).unwrap();
            let file = File::open(&path).unwrap();
            let len = file.metadata().unwrap().len();
            let total_text_len = (part1.len() + part2.len()) as u64;
            let mut reader = compressed_input(file.take(len), total_text_len - 1).unwrap();
            let err = reader.read_to_end(&mut Vec::new()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        }

        #[test]
        fn truncated_or_invalid_skippable_frame_and_block_headers_fail() {
            // Truncated skippable header (6 bytes) -> UnexpectedEof.
            let skip = skippable_frame(ZSTD_SKIPPABLE_MAGIC_MIN, b"payload");
            let mut source = Cursor::new(&skip[..6]);
            assert_eq!(
                compressed_rollout_declared_len(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );

            // Truncated skippable payload -> UnexpectedEof.
            let mut source = Cursor::new(&skip[..skip.len() - 2]);
            assert_eq!(
                compressed_rollout_declared_len(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );

            // Trailing non-zstd bytes after a skippable frame -> InvalidData.
            let mut skip_then_garbage = skippable_frame(ZSTD_SKIPPABLE_MAGIC_MIN, b"ok");
            skip_then_garbage.extend_from_slice(b"junk");
            let mut source = Cursor::new(skip_then_garbage);
            assert_eq!(
                compressed_rollout_declared_len(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );

            // Truncated block payload before Block_Last -> UnexpectedEof.
            let raw = raw_frame_with_window_log(16, b"hello world", false);
            let mut source = Cursor::new(&raw[..raw.len() - 3]);
            assert_eq!(
                compressed_rollout_declared_len(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );

            // Truncated 4-byte frame checksum after Block_Last -> UnexpectedEof.
            let with_csum = raw_frame_with_window_log(16, b"hello", true);
            let mut source = Cursor::new(&with_csum[..with_csum.len() - 2]);
            assert_eq!(
                compressed_rollout_declared_len(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );

            // Reserved block type (block_type == 3 -> bits 2..=1 == 0b11 -> 0x07 with last_block=1).
            let mut reserved_block = Vec::new();
            reserved_block.extend_from_slice(&ZSTD_FRAME_MAGIC.to_le_bytes());
            reserved_block.push(0x00); // fhd: window descriptor present
            reserved_block.push(0x00); // window descriptor
            reserved_block.extend_from_slice(&[0x07, 0x00, 0x00]); // last=1, type=3 (reserved)
            let mut source = Cursor::new(reserved_block);
            assert_eq!(
                compressed_rollout_declared_len(&mut source)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }

        #[test]
        fn window_log_max_rejects_windows_above_64_mib() {
            // windowLog = 26 (64 MiB) is within the cap and decodes cleanly.
            let ok_frame = raw_frame_with_window_log(26, b"{}\n", false);
            let mut decoded = Vec::new();
            decompressed_rollout(Cursor::new(&ok_frame), 1024)
                .unwrap()
                .read_to_end(&mut decoded)
                .unwrap();
            assert_eq!(decoded, b"{}\n");

            // windowLog = 27 (128 MiB, which libzstd's default 27 allows) and
            // windowLog = 28 (256 MiB) must both be rejected by window_log_max(26).
            for window_log in [27u8, 28u8] {
                let oversized = raw_frame_with_window_log(window_log, b"{}\n", false);
                let err = decompressed_rollout(Cursor::new(&oversized), 1024)
                    .unwrap()
                    .read_to_end(&mut Vec::new())
                    .expect_err("window above 64 MiB must be rejected");
                assert_ne!(
                    err.kind(),
                    io::ErrorKind::FileTooLarge,
                    "windowLog={window_log} should fail on decoder window cap, not budget"
                );
            }
        }

        #[test]
        fn only_rollout_jsonl_zst_names_are_compressed_rollouts() {
            for (name, expected) in [
                ("rollout-x.jsonl.zst", true),
                ("rollout-x.JSONL.ZST", true),
                ("rollout-x.jsonl", false),
                ("rollout-x.zst", false),
                ("rollout-x.json.zst", false),
                ("other-x.jsonl.zst", false),
            ] {
                assert_eq!(is_compressed_rollout(Path::new(name)), expected, "{name}");
            }
        }
    }

    #[test]
    fn liveness_ticks_include_skipped_lines_and_only_the_owning_scan() {
        let ticks = AtomicUsize::new(0);
        let tick = || {
            ticks.fetch_add(1, Ordering::Relaxed);
        };
        let bytes = "\n".repeat(PROGRESS_LINE_STRIDE * 2 + 1);
        let mut records = Records::new(Cursor::new(bytes), Some(&tick));
        assert!(records.next_record().unwrap().is_none());
        assert_eq!(ticks.load(Ordering::Relaxed), 3);
    }
}
