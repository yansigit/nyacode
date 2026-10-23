use std::io::Read;
use std::pin::Pin;

use bytes::Buf;
use bytes::Bytes;
use bytes::BytesMut;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use futures::Stream;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_FRAME: usize = 16 * 1024 * 1024;
pub(crate) const MAX_UNARY: usize = 4 * 1024 * 1024;

pub(crate) fn encode(payload: &[u8]) -> Result<Bytes> {
    if payload.len() > MAX_FRAME {
        return Err(ProviderError::Protocol("Cursor frame exceeds limit".into()));
    }
    let mut bytes = Vec::with_capacity(payload.len() + 5);
    bytes.push(0);
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes.into())
}

#[derive(Default)]
pub(crate) struct Decoder {
    buffer: BytesMut,
    ended: bool,
}

pub(crate) enum Frame {
    Message(Bytes),
    End,
}

impl Decoder {
    pub fn push(&mut self, data: &[u8]) -> Result<()> {
        if self.ended {
            return Err(ProviderError::Protocol(
                "data after Cursor end stream".into(),
            ));
        }
        // Backpressure bounds retained data independently of the announced size.
        if data.len() + self.buffer.len() > MAX_FRAME + 5 {
            return Err(ProviderError::Protocol(
                "Cursor frame buffer exceeds limit".into(),
            ));
        }
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    pub fn next(&mut self) -> Result<Option<Frame>> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }
        let flags = self.buffer[0];
        if flags & !3 != 0 {
            return Err(ProviderError::Protocol(
                "unknown Cursor Connect flags".into(),
            ));
        }
        let length = u32::from_be_bytes([
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
            self.buffer[4],
        ]) as usize;
        let limit = if flags & 2 != 0 { 64 * 1024 } else { MAX_FRAME };
        if length > limit {
            return Err(ProviderError::Protocol("Cursor frame exceeds limit".into()));
        }
        if self.buffer.len() < 5 + length {
            return Ok(None);
        }
        self.buffer.advance(5);
        let mut payload = self.buffer.split_to(length).freeze();
        if flags & 1 != 0 {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(payload.as_ref())
                .take((limit + 1) as u64)
                .read_to_end(&mut decoded)
                .map_err(|_| ProviderError::Protocol("invalid Cursor gzip frame".into()))?;
            if decoded.len() > limit {
                return Err(ProviderError::Protocol(
                    "decompressed Cursor frame exceeds limit".into(),
                ));
            }
            payload = decoded.into();
        }
        if flags & 2 != 0 {
            let end: serde_json::Value = serde_json::from_slice(&payload)
                .map_err(|_| ProviderError::Protocol("invalid Cursor Connect end frame".into()))?;
            if let Some(error) = end.get("error") {
                let code = error
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let status = match code {
                    "unauthenticated" => 401,
                    "resource_exhausted" => 429,
                    "unavailable" => 503,
                    "invalid_argument" => 400,
                    _ => 0,
                };
                return Err(ProviderError::Classified(crate::failure::classify(
                    codex_provider_api::FailureResponse {
                        status,
                        body: &payload,
                        retry_after_seconds: None,
                        model: "",
                        now: 0,
                        // Connect may already have consumed inference; no automatic replay.
                        outcome: codex_provider_api::RequestOutcome::PotentiallyProcessed,
                    },
                )));
            }
            self.ended = true;
            if !self.buffer.is_empty() {
                return Err(ProviderError::Protocol(
                    "data after Cursor end stream".into(),
                ));
            }
            Ok(Some(Frame::End))
        } else {
            Ok(Some(Frame::Message(payload)))
        }
    }

    pub fn finish(&self) -> Result<()> {
        if !self.buffer.is_empty() {
            return Err(ProviderError::Protocol(
                "truncated Cursor Connect frame".into(),
            ));
        }
        if !self.ended {
            return Err(ProviderError::Protocol(
                "Cursor stream ended without Connect end frame".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct Reader {
    stream: Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    decoder: Decoder,
}

impl Reader {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            decoder: Decoder::default(),
        }
    }

    pub fn check_buffer_complete(&self) -> Result<()> {
        if !self.decoder.buffer.is_empty() {
            return Err(ProviderError::Protocol(
                "truncated Cursor Connect frame at turn completion".into(),
            ));
        }
        Ok(())
    }

    pub async fn next(&mut self, cancellation: &CancellationToken) -> Result<Option<Bytes>> {
        loop {
            match self.decoder.next()? {
                Some(Frame::Message(payload)) => return Ok(Some(payload)),
                Some(Frame::End) => return Ok(None),
                None => {}
            }
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                chunk = self.stream.next() => chunk,
            };
            match chunk {
                Some(Ok(bytes)) => self.decoder.push(&bytes)?,
                Some(Err(_)) => {
                    return Err(ProviderError::Transport("Cursor stream interrupted".into()));
                }
                None => {
                    self.decoder.finish()?;
                    return Ok(None);
                }
            }
        }
    }
}

pub(crate) async fn bounded_body(
    response: reqwest::Response,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_UNARY as u64)
    {
        return Err(ProviderError::Protocol(
            "Cursor response exceeds limit".into(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
            chunk = stream.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => {
                if body.len() + bytes.len() > MAX_UNARY {
                    return Err(ProviderError::Protocol(
                        "Cursor response exceeds limit".into(),
                    ));
                }
                body.extend_from_slice(&bytes);
            }
            Some(Err(_)) => {
                return Err(ProviderError::Transport(
                    "Cursor response interrupted".into(),
                ));
            }
            None => return Ok(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_frames_and_truncation() {
        let frame = encode(b"hello").unwrap();
        let mut decoder = Decoder::default();
        for byte in &frame[..frame.len() - 1] {
            decoder.push(&[*byte]).unwrap();
            assert!(decoder.next().unwrap().is_none());
        }
        assert!(decoder.finish().is_err());
        decoder.push(&frame[frame.len() - 1..]).unwrap();
        assert!(
            matches!(decoder.next().unwrap(), Some(Frame::Message(payload)) if payload == b"hello"[..])
        );
        decoder.push(&[2, 0, 0, 0, 2, b'{', b'}']).unwrap();
        assert!(matches!(decoder.next().unwrap(), Some(Frame::End)));
        decoder.finish().unwrap();
    }

    #[test]
    fn rejects_oversized_announced_frames_and_unknown_flags() {
        let mut decoder = Decoder::default();
        decoder.push(&[0, 255, 255, 255, 255]).unwrap();
        assert!(decoder.next().is_err());
        let mut decoder = Decoder::default();
        decoder.push(&[4, 0, 0, 0, 0]).unwrap();
        assert!(decoder.next().is_err());
    }

    #[test]
    fn connect_error_body_is_bounded_before_json_or_gzip_expansion() {
        use std::io::Write;
        for compressed in [false, true] {
            let large = vec![b' '; 64 * 1024 + 1];
            let payload = if compressed {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&large).unwrap();
                encoder.finish().unwrap()
            } else {
                large
            };
            let mut frame = encode(&payload).unwrap().to_vec();
            frame[0] = if compressed { 3 } else { 2 };
            let mut decoder = Decoder::default();
            decoder.push(&frame).unwrap();
            assert!(
                matches!(decoder.next(), Err(ProviderError::Protocol(message)) if message.contains("exceeds limit"))
            );
        }
    }

    #[test]
    fn connect_authentication_error_is_typed() {
        let payload = br#"{"error":{"code":"unauthenticated","message":"secret echo"}}"#;
        let mut frame = encode(payload).unwrap().to_vec();
        frame[0] = 2;
        let mut decoder = Decoder::default();
        decoder.push(&frame).unwrap();
        assert!(matches!(
            decoder.next(),
            Err(ProviderError::Classified(failure)) if failure.category == codex_provider_api::FailureCategory::Authentication && !failure.can_fail_over()
        ));
    }
    #[test]
    fn turn_completion_rejects_an_incomplete_buffered_frame() {
        let mut reader = Reader {
            stream: Box::pin(futures::stream::pending()),
            decoder: Decoder::default(),
        };
        reader.decoder.push(&[0, 0]).unwrap();
        assert!(reader.check_buffer_complete().is_err());
        let reader = Reader {
            stream: Box::pin(futures::stream::pending()),
            decoder: Decoder::default(),
        };
        reader.check_buffer_complete().unwrap();
    }
}
