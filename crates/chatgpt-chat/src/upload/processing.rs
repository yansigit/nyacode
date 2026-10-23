//! Bounded UTF-8 NDJSON processing events. EOF never establishes readiness.

use crate::ChatError;
use serde_json::Value;

const LINE_LIMIT: usize = 64 * 1024;
const STREAM_LIMIT: usize = 2 * 1024 * 1024;
const EVENT_LIMIT: usize = 4096;

pub(super) struct Processing {
    file_id: String,
    line: Vec<u8>,
    bytes: usize,
    events: usize,
    ready: bool,
    completed: bool,
    failed: bool,
    pub progress: Option<u8>,
    pub total_tokens: Option<u64>,
}

impl Processing {
    pub fn new(file_id: String) -> Self {
        Self {
            file_id,
            line: Vec::new(),
            bytes: 0,
            events: 0,
            ready: false,
            completed: false,
            failed: false,
            progress: None,
            total_tokens: None,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ChatError> {
        let result = self.push_inner(bytes);
        self.failed |= result.is_err();
        result
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<(), ChatError> {
        if self.failed {
            return Err(ChatError::ProcessingIncomplete);
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > STREAM_LIMIT {
            return Err(ChatError::ResponseTooLarge);
        }
        for &byte in bytes {
            if byte == b'\n' {
                self.event()?;
                self.line.clear();
            } else {
                if self.line.len() >= LINE_LIMIT {
                    return Err(ChatError::ResponseTooLarge);
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }

    fn event(&mut self) -> Result<(), ChatError> {
        if self.line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        self.events += 1;
        if self.events > EVENT_LIMIT {
            return Err(ChatError::ResponseTooLarge);
        }
        let value: Value =
            serde_json::from_slice(&self.line).map_err(|_| ChatError::ProtocolChanged)?;
        let event = value
            .get("event")
            .and_then(Value::as_str)
            .ok_or(ChatError::ProtocolChanged)?;
        if event.len() > 128 || value.get("file_id").and_then(Value::as_str) != Some(&self.file_id)
        {
            return Err(ChatError::ProtocolChanged);
        }
        // Never inherit the web client's optional raw-file fallback following
        // indexing failure. A failed processing operation cannot issue a handle.
        if ["error", "cancelled", "failed", "unknown"]
            .iter()
            .any(|suffix| event.ends_with(suffix))
        {
            return Err(ChatError::UploadFailed);
        }
        match event {
            "file.processing.file_ready" if !self.completed => self.ready = true,
            "file.processing.completed" if self.ready && !self.completed => self.completed = true,
            "file.processing.completed" => return Err(ChatError::ProtocolChanged),
            _ if self.completed => return Err(ChatError::ProtocolChanged),
            _ => {}
        }
        if let Some(progress) = value.get("progress").filter(|v| !v.is_null()) {
            let progress = progress
                .as_f64()
                .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))
                .ok_or(ChatError::ProtocolChanged)?;
            self.progress = Some(progress as u8);
        }
        if let Some(tokens) = value
            .pointer("/extra/total_tokens")
            .filter(|v| !v.is_null())
        {
            self.total_tokens = Some(tokens.as_u64().ok_or(ChatError::ProtocolChanged)?);
        }
        // The caller explicitly requested no Library persistence. Reject a
        // response indicating a different operation, especially for temporary Chat.
        if let Some(mode) = value
            .pointer("/extra/library_persistence_result")
            .and_then(Value::as_str)
            && mode != "temporary"
        {
            return Err(ChatError::ProtocolChanged);
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), ChatError> {
        if self.failed {
            return Err(ChatError::ProcessingIncomplete);
        }
        if !self.line.is_empty() {
            if let Err(error) = self.event() {
                self.failed = true;
                return Err(error);
            }
            self.line.clear();
        }
        if self.ready && self.completed {
            Ok(())
        } else {
            Err(ChatError::ProcessingIncomplete)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn arbitrary_utf8_and_line_boundaries_require_both_terminal_events() {
        let body = concat!(
            "{\"event\":\"file.processing.started\",\"file_id\":\"file-fixture\",\"extra\":{\"ignored\":\"é😀\"}}\r\n",
            "{\"event\":\"file.processing.file_ready\",\"file_id\":\"file-fixture\",\"progress\":50}\n",
            "{\"event\":\"file.processing.completed\",\"file_id\":\"file-fixture\",\"extra\":{\"total_tokens\":42,\"library_persistence_result\":\"temporary\"}}"
        ).as_bytes();
        for split in 0..=body.len() {
            let mut decoder = Processing::new("file-fixture".into());
            decoder.push(&body[..split]).unwrap();
            decoder.push(&body[split..]).unwrap();
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!(decoder.total_tokens, Some(42));
        }
        for event in ["file.processing.started", "file.processing.file_ready"] {
            let mut decoder = Processing::new("file-fixture".into());
            decoder
                .push(
                    serde_json::json!({"event":event,"file_id":"file-fixture"})
                        .to_string()
                        .as_bytes(),
                )
                .unwrap();
            assert_eq!(decoder.finish(), Err(ChatError::ProcessingIncomplete));
        }
    }

    #[test]
    fn failed_mismatched_malformed_and_oversized_events_never_issue_readiness() {
        for value in [
            serde_json::json!({"event":"file.processing.completed","file_id":"file-fixture"}),
            serde_json::json!({"event":"file.processing.file_ready","file_id":"another-file"}),
            serde_json::json!({"event":"file.indexing.error","file_id":"file-fixture","extra":{"raw_file_fallback_available":true}}),
            serde_json::json!({"event":"file.processing.started","file_id":"file-fixture","progress":101}),
            serde_json::json!({"event":"file.processing.started","file_id":"file-fixture","extra":{"library_persistence_result":"library"}}),
        ] {
            let mut decoder = Processing::new("file-fixture".into());
            assert!(decoder.push(format!("{value}\n").as_bytes()).is_err());
            assert!(decoder.finish().is_err());
        }
        for bytes in [
            b"{bad}\n".to_vec(),
            b"\xff\n".to_vec(),
            vec![b'x'; LINE_LIMIT + 1],
        ] {
            let mut decoder = Processing::new("file-fixture".into());
            assert!(decoder.push(&bytes).is_err());
            assert!(decoder.finish().is_err());
        }
    }
}
