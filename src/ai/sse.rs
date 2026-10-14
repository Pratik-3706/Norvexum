// ===========================================================================
// SSE line decoding shared by all streaming providers
//
// Network chunks can end in the middle of a multi-byte UTF-8 character, so
// bytes are buffered and only complete lines are decoded. Decoding each chunk
// on its own (the old approach) turned split characters into U+FFFD.
// ===========================================================================

#[derive(Default)]
pub struct SseLineBuffer {
    buf: Vec<u8>,
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk and return every complete line it finished, without the
    /// trailing `\n` / `\r\n`.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            let line = &self.buf[start..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            lines.push(String::from_utf8_lossy(line).into_owned());
            start = end + 1;
        }
        self.buf.drain(..start);
        lines
    }

    /// Whatever is left after the stream ended (a final line without `\n`).
    pub fn finish(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let rest = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        Some(rest)
    }
}

/// Extract the payload of an SSE `data:` line (the space after the colon is optional).
pub fn data_payload(line: &str) -> Option<&str> {
    line.strip_prefix("data:")
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_characters_split_across_chunks() {
        let text = "data: {\"t\":\"héllo ✅\"}\n";
        let bytes = text.as_bytes();
        // Split inside the 3-byte check mark.
        let split = text.find('✅').unwrap() + 1;
        let mut buf = SseLineBuffer::new();
        assert!(buf.push(&bytes[..split]).is_empty());
        let lines = buf.push(&bytes[split..]);
        assert_eq!(lines, vec!["data: {\"t\":\"héllo ✅\"}"]);
    }

    #[test]
    fn handles_crlf_and_multiple_lines() {
        let mut buf = SseLineBuffer::new();
        let lines = buf.push(b"event: a\r\ndata: 1\r\n\r\ndata: 2");
        assert_eq!(lines, vec!["event: a", "data: 1", ""]);
        assert_eq!(buf.finish().as_deref(), Some("data: 2"));
        assert_eq!(data_payload("data:{}"), Some("{}"));
    }
}
