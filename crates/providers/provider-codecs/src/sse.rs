use crate::protocol;
use codex_provider_api::Result;

/// SSE framing is independent of HTTP chunk boundaries and UTF-8 codepoints.
#[derive(Default)]
pub struct SseDecoder {
    bytes: Vec<u8>,
    data: Vec<String>,
    event: String,
    frame_bytes: usize,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<(String, String)>> {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > 8 * 1024 * 1024 {
            return Err(protocol("SSE line exceeds 8 MiB"));
        }
        let mut frames = Vec::new();
        while let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.bytes.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.frame_bytes += line.len();
            if self.frame_bytes > 8 * 1024 * 1024 {
                return Err(protocol("SSE event exceeds 8 MiB"));
            }
            let line = std::str::from_utf8(&line).map_err(|_| protocol("invalid SSE UTF-8"))?;
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push((std::mem::take(&mut self.event), self.data.join("\n")));
                }
                self.data.clear();
                self.event.clear();
                self.frame_bytes = 0;
            } else if let Some(value) = line.strip_prefix("data:") {
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            } else if let Some(value) = line.strip_prefix("event:") {
                self.event = value.trim_start().to_owned();
            }
        }
        Ok(frames)
    }
    pub fn finish(&self) -> Result<()> {
        if !self.bytes.is_empty() || !self.data.is_empty() {
            return Err(protocol("truncated SSE event"));
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_unicode_crlf_and_multiline_data() {
        let input = "event: delta\r\ndata: é\r\ndata: next\r\n\r\n".as_bytes();
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        for byte in input {
            frames.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(frames, vec![("delta".into(), "é\nnext".into())]);
        decoder.finish().unwrap();
    }
    #[test]
    fn truncated_event_is_error() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: {}\n").unwrap();
        assert!(decoder.finish().is_err());
    }
}
