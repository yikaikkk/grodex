//! LineBuffer — accumulates partial SSE lines across chunk boundaries.
//!
//! HTTP chunks may split an SSE `data:` line mid-stream. The buffer
//! holds incomplete trailing bytes and prepends them to the next chunk.
//!
//! **Important**: this buffer operates on raw bytes and only converts to
//! `String` *after* a complete line (terminated by `\n`) has been
//! accumulated. This avoids `String::from_utf8_lossy` being called on a
//! byte slice that ends mid-multibyte UTF-8 sequence (e.g. a CJK
//! character split across two HTTP chunks), which would permanently
//! replace the trailing bytes with U+FFFD and corrupt the stream.

pub struct LineBuffer {
    pending: Vec<u8>,
}

impl Default for LineBuffer { fn default() -> Self { Self::new() } }

impl LineBuffer {
    pub fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Feed a chunk, returns complete lines (without trailing newline).
    /// The last incomplete line is held in the buffer for the next call.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut data = Vec::new();
        data.extend_from_slice(&self.pending);
        data.extend_from_slice(chunk);
        self.pending.clear();

        // Find the last `\n` in the accumulated data. Everything up to
        // and including it consists of complete lines; any bytes after
        // it are an incomplete line held for the next feed.
        let last_nl = data.iter().rposition(|&b| b == b'\n');

        let mut lines: Vec<String> = Vec::new();
        match last_nl {
            Some(nl) => {
                // Slice the complete portion [0..=nl] and split on `\n`.
                // `\n` is a single ASCII byte that never appears inside a
                // multibyte UTF-8 sequence, so splitting on it at the byte
                // level is always safe and never cuts a character in half.
                let complete = &data[..=nl];
                for line in complete.split(|&b| b == b'\n') {
                    // Each `line` slice is complete UTF-8 in normal operation
                    // (model JSON is UTF-8). `from_utf8_lossy` is only a safety
                    // net for a misbehaving provider — it never sees a partial
                    // multibyte sequence because we split on `\n` boundaries.
                    lines.push(String::from_utf8_lossy(line).to_string());
                }
                // split() on "a\n" yields ["a", ""] — drop the trailing empty
                // produced by the final `\n`. But keep genuine blank lines in
                // the middle (e.g. "a\n\nb\n" → ["a", "", "b"]).
                // The trailing empty (if data ends with `\n`) is always the
                // last element and should be removed.
                if data[nl] == b'\n' && lines.last().map(|s| s.is_empty()).unwrap_or(false) {
                    lines.pop();
                }
                // Save trailing bytes after the last `\n` as pending.
                if nl + 1 < data.len() {
                    self.pending = data[nl + 1..].to_vec();
                }
            }
            None => {
                // No newline — entire accumulated data is an incomplete line.
                self.pending = data;
            }
        }

        lines
    }

    /// Flush any remaining incomplete line (called on stream end).
    pub fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            let remaining = std::mem::take(&mut self.pending);
            Some(String::from_utf8_lossy(&remaining).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_lines_pass_through() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed(b"data: {\"a\":1}\ndata: {\"b\":2}\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r#"data: {"a":1}"#);
        assert_eq!(lines[1], r#"data: {"b":2}"#);
    }

    #[test]
    fn split_line_reassembled() {
        let mut buf = LineBuffer::new();
        let lines1 = buf.feed(b"data: {\"a\":");
        assert!(lines1.is_empty());

        let lines2 = buf.feed(b"1}\ndata: {\"b\":2}\n");
        assert_eq!(lines2.len(), 2);
        assert_eq!(lines2[0], r#"data: {"a":1}"#);
        assert_eq!(lines2[1], r#"data: {"b":2}"#);
    }

    #[test]
    fn flush_incomplete() {
        let mut buf = LineBuffer::new();
        buf.feed(b"data: {\"a\":");
        let remaining = buf.flush().unwrap();
        assert!(remaining.contains(r#"data: {"a":"#));
    }

    #[test]
    fn split_utf8_multibyte_across_chunks() {
        // "高优先级" = E9 AB 98 E4 BC 98 E5 85 88 E7 BA A7 (12 bytes)
        // Split after byte 4 (mid-character of "优"):
        //   chunk1: ...E9 AB 98 E4   (valid "高" + start of "优")
        //   chunk2: BC 98 E5 85 88 E7 BA A7  (rest of "优" + "先" + "级")
        let mut buf = LineBuffer::new();
        // Simulate an SSE line containing "高优先级" split across chunks
        let line1 = b"data: {\"text\":\"";
        let line2_bytes_of_cjk: &[u8] = &[0xE9, 0xAB, 0x98, 0xE4]; // "高" + first byte of "优"
        let chunk1: Vec<u8> = [line1, line2_bytes_of_cjk].concat();
        let lines1 = buf.feed(&chunk1);
        assert!(lines1.is_empty(), "no complete line yet, got {:?}", lines1);

        let line2_rest: &[u8] = &[0xBC, 0x98, 0xE5, 0x85, 0x88, 0xE7, 0xBA, 0xA7]; // rest of 优先级
        let line_suffix = b"\"}\n";
        let chunk2: Vec<u8> = [line2_rest, line_suffix].concat();
        let lines2 = buf.feed(&chunk2);
        assert_eq!(lines2.len(), 1, "expected one complete line, got {:?}", lines2);
        assert!(lines2[0].contains("高优先级"), "line should contain 高优先级, got: {}", lines2[0]);
    }

    #[test]
    fn blank_line_preserved() {
        let mut buf = LineBuffer::new();
        let lines = buf.feed(b"data: {\"a\":1}\n\n");
        // Two newlines → one data line + one blank line
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], r#"data: {"a":1}"#);
        assert_eq!(lines[1], "");
    }
}
