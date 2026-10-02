use std::borrow::Cow;

/// Incremental byte-level line splitter for streaming HTTP bodies.
///
/// Network chunks are appended as raw bytes and complete `\n`-terminated
/// lines are handed back one at a time. Decoding to UTF-8 happens per
/// complete line, so a multi-byte codepoint split across two chunks is
/// reassembled correctly instead of being mangled into U+FFFD (the failure
/// mode of decoding each chunk independently with `from_utf8_lossy`).
///
/// Draining is linear: consumed bytes are tracked with a start offset and
/// compacted once per appended chunk, rather than reallocating the entire
/// remaining buffer for every line (which made parsing O(L²) in the
/// response length).
pub(crate) struct LineBuffer {
    buf: Vec<u8>,
    /// Byte offset of the first unconsumed byte in `buf`.
    start: usize,
}

impl LineBuffer {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
        }
    }

    /// Number of unconsumed bytes currently buffered.
    pub(crate) fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Append a chunk of raw bytes from the network.
    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) {
        // Compact once per chunk so consumed bytes don't accumulate.
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// Pop the next complete line (excluding the trailing `\n`), if any.
    ///
    /// Trailing partial bytes (no newline yet) stay buffered for the next
    /// chunk. A line containing invalid UTF-8 is decoded lossily — that can
    /// only happen when the server sent genuinely invalid bytes, never from
    /// a codepoint spanning a chunk boundary.
    pub(crate) fn next_line(&mut self) -> Option<Cow<'_, str>> {
        let rest = &self.buf[self.start..];
        let pos = rest.iter().position(|&b| b == b'\n')?;
        let line_start = self.start;
        self.start += pos + 1;
        Some(String::from_utf8_lossy(
            &self.buf[line_start..line_start + pos],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drain all currently complete lines into owned strings.
    fn lines(buf: &mut LineBuffer) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(l) = buf.next_line() {
            out.push(l.into_owned());
        }
        out
    }

    #[test]
    fn multibyte_codepoint_split_across_chunks_is_not_corrupted() {
        // "é" is 0xC3 0xA9; the chunk boundary falls between the two bytes.
        let mut buf = LineBuffer::new();
        buf.push_chunk(b"caf\xC3");
        assert!(buf.next_line().is_none(), "no complete line yet");
        buf.push_chunk(b"\xA9\n");
        assert_eq!(lines(&mut buf), vec!["café"]);
    }

    #[test]
    fn four_byte_emoji_split_across_three_chunks() {
        let emoji = "🦀".as_bytes(); // 4 bytes
        let mut buf = LineBuffer::new();
        buf.push_chunk(&emoji[..1]);
        buf.push_chunk(&emoji[1..3]);
        assert!(buf.next_line().is_none());
        buf.push_chunk(&emoji[3..]);
        buf.push_chunk(b"\n");
        assert_eq!(lines(&mut buf), vec!["🦀"]);
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        let mut buf = LineBuffer::new();
        buf.push_chunk(b"event: ping\ndata: {}\n\npartial");
        assert_eq!(lines(&mut buf), vec!["event: ping", "data: {}", ""]);
        // The trailing partial line stays buffered for the next chunk.
        assert_eq!(buf.len(), "partial".len());
        buf.push_chunk(b" line\n");
        assert_eq!(lines(&mut buf), vec!["partial line"]);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn crlf_line_endings_leave_cr_for_caller_to_trim() {
        let mut buf = LineBuffer::new();
        buf.push_chunk(b"hello\r\n");
        assert_eq!(lines(&mut buf), vec!["hello\r"]);
    }

    #[test]
    fn invalid_utf8_within_a_line_is_decoded_lossily() {
        let mut buf = LineBuffer::new();
        buf.push_chunk(b"ok\n\xFF\xFE\nafter\n");
        assert_eq!(lines(&mut buf), vec!["ok", "\u{FFFD}\u{FFFD}", "after"]);
    }

    #[test]
    fn empty_buffer_yields_no_lines() {
        let mut buf = LineBuffer::new();
        assert!(buf.next_line().is_none());
        buf.push_chunk(b"");
        assert!(buf.next_line().is_none());
        assert_eq!(buf.len(), 0);
    }
}
