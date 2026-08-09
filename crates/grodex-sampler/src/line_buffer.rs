//! LineBuffer — accumulates partial SSE lines across chunk boundaries.
//!
//! HTTP chunks may split an SSE `data:` line mid-stream. The buffer
//! holds incomplete trailing bytes and prepends them to the next chunk.

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

        let text = String::from_utf8_lossy(&data);
        let mut lines: Vec<&str> = text.lines().collect();

        // If the original data doesn't end with a newline, the last "line"
        // is incomplete — hold it for the next feed.
        if !chunk.is_empty() && !chunk.ends_with(b"\n") {
            if let Some(incomplete) = lines.pop() {
                self.pending = incomplete.as_bytes().to_vec();
            }
        }

        lines.into_iter().map(|s| s.to_string()).collect()
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
}
