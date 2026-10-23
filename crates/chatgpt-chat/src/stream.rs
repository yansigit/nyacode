//! Bounded wire decoding for native Chat streams. Wire payloads are private:
//! errors never include response contents, and EOF is not a turn outcome.

mod delta;
mod turn;
pub use delta::DeltaDecoder;
pub use turn::Acceptance;
pub use turn::GenerationOutcome;
pub use turn::Message;
pub use turn::MessagePart;
pub use turn::TurnDecoder;
pub use turn::TurnState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum StreamError {
    #[error("the stream does not match the supported turn protocol")]
    InvalidProtocol,
    #[error("the stream delta does not match the supported protocol")]
    InvalidDelta,
    #[error("the accumulated stream content exceeded its limit")]
    ContentLimit,
    #[error("the stream contains invalid UTF-8")]
    InvalidUtf8,
    #[error("a stream frame exceeded its size limit")]
    FrameLimit,
    #[error("the stream ended inside an unfinished frame")]
    IncompleteFrame,
    #[error("the stream decoder has already failed")]
    DecoderFailed,
}

/// No Debug or Serialize implementation: this can contain conversation text.
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

/// Incremental SSE framing. The callback consumes each complete event before
/// another is decoded, avoiding an unbounded queue from a large input chunk.
/// The turn protocol must separately verify its terminal marker and outcome.
pub struct SseDecoder {
    line: Vec<u8>,
    data: String,
    event: String,
    last_id: Option<String>,
    skip_lf: bool,
    first_line: bool,
    failed: bool,
    limit: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            event: String::new(),
            last_id: None,
            skip_lf: false,
            first_line: true,
            failed: false,
            limit: 1024 * 1024,
        }
    }

    pub fn push(
        &mut self,
        chunk: &[u8],
        mut on_event: impl FnMut(SseEvent) -> Result<(), StreamError>,
    ) -> Result<(), StreamError> {
        if self.failed {
            return Err(StreamError::DecoderFailed);
        }
        let result = self.push_inner(chunk, &mut on_event);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(
        &mut self,
        chunk: &[u8],
        on_event: &mut impl FnMut(SseEvent) -> Result<(), StreamError>,
    ) -> Result<(), StreamError> {
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' | b'\n' => {
                    self.skip_lf = byte == b'\r';
                    self.end_line(on_event)?;
                }
                _ => {
                    if self.line.len() == self.limit {
                        return Err(StreamError::FrameLimit);
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(())
    }

    fn end_line(
        &mut self,
        on_event: &mut impl FnMut(SseEvent) -> Result<(), StreamError>,
    ) -> Result<(), StreamError> {
        let raw = std::mem::take(&mut self.line);
        let raw = if std::mem::replace(&mut self.first_line, false) {
            raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&raw)
        } else {
            &raw
        };
        let line = std::str::from_utf8(raw).map_err(|_| StreamError::InvalidUtf8)?;
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            if !self.data.is_empty() {
                // Every data line contributes one trailing LF, including an
                // empty data line. Only that final LF is removed for dispatch.
                self.data.pop();
                on_event(SseEvent {
                    event: if event.is_empty() {
                        "message".into()
                    } else {
                        event
                    },
                    data: std::mem::take(&mut self.data),
                    id: self.last_id.clone(),
                })?;
            }
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if self
                    .data
                    .len()
                    .saturating_add(value.len())
                    .saturating_add(1)
                    > self.limit
                {
                    return Err(StreamError::FrameLimit);
                }
                self.data.push_str(value);
                self.data.push('\n');
            }
            "event" => {
                if value.len() > 128 {
                    return Err(StreamError::FrameLimit);
                }
                self.event = value.into();
            }
            "id" if !value.contains('\0') => {
                if value.len() > 1024 {
                    return Err(StreamError::FrameLimit);
                }
                self.last_id = Some(value.into());
            }
            // retry fields never authorize a replay or automatic reconnect.
            _ => {}
        }
        Ok(())
    }

    /// Checks framing only. A clean boundary without the turn's completion
    /// marker is still an incomplete turn, even when this returns Ok.
    pub fn finish(&mut self) -> Result<(), StreamError> {
        if self.failed {
            return Err(StreamError::DecoderFailed);
        }
        if !self.line.is_empty() || !self.data.is_empty() || !self.event.is_empty() {
            self.failed = true;
            return Err(StreamError::IncompleteFrame);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn collect(chunks: &[&[u8]]) -> Result<Vec<(String, String, Option<String>)>, StreamError> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            decoder.push(chunk, |event| {
                events.push((event.event, event.data, event.id));
                Ok(())
            })?;
        }
        decoder.finish()?;
        Ok(events)
    }

    #[test]
    fn all_byte_boundaries_preserve_utf8_crlf_multiline_and_bom() {
        let input = "\u{feff}: comment\r\nid: fixture\revent: delta\r\ndata: é😀\r\ndata: second\r\n\r\ndata:\n\n".as_bytes();
        let expected = vec![
            ("delta".into(), "é😀\nsecond".into(), Some("fixture".into())),
            ("message".into(), "".into(), Some("fixture".into())),
        ];
        for boundary in 0..=input.len() {
            assert_eq!(
                collect(&[&input[..boundary], &input[boundary..]]),
                Ok(expected.clone())
            );
        }
        assert_eq!(collect(&input.chunks(1).collect::<Vec<_>>()), Ok(expected));
    }

    #[test]
    fn field_rules_preserve_spaces_and_ignore_invalid_ids_and_retry() {
        assert_eq!(
            collect(&[b"event: unused\n\nid: first\nid: ignored\0id\nretry: 1\nunknown: value\ndata:  x\n\nid:\ndata\n\n"]),
            Ok(vec![
                ("message".into(), " x".into(), Some("first".into())),
                ("message".into(), "".into(), Some("".into())),
            ])
        );
    }

    #[test]
    fn malformed_utf8_limits_and_incomplete_frames_stop_the_decoder() {
        assert_eq!(collect(&[b"data: \xff\n\n"]), Err(StreamError::InvalidUtf8));
        for input in [b"data: text".as_slice(), b"data: text\n", b"event: delta\n"] {
            assert_eq!(collect(&[input]), Err(StreamError::IncompleteFrame));
        }
        let mut decoder = SseDecoder::new();
        decoder.limit = 16;
        assert_eq!(
            decoder.push(b"data: 12345678\ndata: 12345678\n", |_| Ok(())),
            Err(StreamError::FrameLimit)
        );
        assert_eq!(
            decoder.push(b"\n", |_| Ok(())),
            Err(StreamError::DecoderFailed)
        );
        let mut decoder = SseDecoder::new();
        decoder.limit = 4;
        assert_eq!(
            decoder.push(b"12345", |_| Ok(())),
            Err(StreamError::FrameLimit)
        );
    }

    #[test]
    fn callback_failure_stops_before_the_next_event() {
        let mut decoder = SseDecoder::new();
        let mut calls = 0;
        assert_eq!(
            decoder.push(b"data: first\n\ndata: second\n\n", |_| {
                calls += 1;
                Err(StreamError::FrameLimit)
            }),
            Err(StreamError::FrameLimit)
        );
        assert_eq!(calls, 1);
        assert_eq!(decoder.finish(), Err(StreamError::DecoderFailed));
    }
}
