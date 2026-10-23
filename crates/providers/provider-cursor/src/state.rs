use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_provider_api::Checkpoint;
use codex_provider_api::Content;
use codex_provider_api::Message;
use codex_provider_api::Namespace;
use codex_provider_api::ProviderError;
use codex_provider_api::Request;
use codex_provider_api::Result;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolResult;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;

const MAX_STATE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Execution {
    pub call: ToolCall,
    pub result: Option<ToolResult>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub(crate) struct State {
    pub conversation: String,
    pub checkpoint: String,
    pub message_count: usize,
    pub system_hash: String,
    pub blobs: BTreeMap<String, String>,
    pub executions: BTreeMap<String, Execution>,
}

pub(crate) fn hash<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| ProviderError::Checkpoint("cannot hash Cursor context".into()))?;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

// Preserve typed Message/Content field order from the original serialization;
// only embedded JSON object maps depended on serde_json's feature selection.
pub(crate) fn canonical_messages(messages: &[Message]) -> Vec<Message> {
    let mut messages = messages.to_vec();
    for content in messages.iter_mut().flat_map(|message| &mut message.content) {
        match content {
            Content::ToolCall(call) => call.arguments.sort_all_objects(),
            Content::ToolResult(result) => result.output.sort_all_objects(),
            Content::Opaque { value, .. } => value.sort_all_objects(),
            _ => {}
        }
    }
    messages
}

pub(crate) fn hash_messages(messages: &[Message]) -> Result<String> {
    hash(&canonical_messages(messages))
}

impl State {
    pub fn restore(request: &Request) -> Result<Self> {
        let Some(checkpoint) = &request.checkpoint else {
            return Ok(Self {
                conversation: uuid::Uuid::new_v4().to_string(),
                message_count: request.messages.len(),
                system_hash: hash(&request.system)?,
                ..Default::default()
            });
        };
        if checkpoint.version != 1 || checkpoint.namespace != request.namespace {
            return Err(ProviderError::Checkpoint(
                "Cursor checkpoint identity or version mismatch".into(),
            ));
        }
        let state: Self = serde_json::from_value(checkpoint.state.clone())
            .map_err(|_| ProviderError::Checkpoint("invalid Cursor checkpoint state".into()))?;
        if state.message_count > request.messages.len()
            || state.system_hash != hash(&request.system)?
            || checkpoint.context_hash != hash_messages(&request.messages[..state.message_count])?
        {
            return Err(ProviderError::Checkpoint(
                "Cursor checkpoint context prefix mismatch".into(),
            ));
        }
        if state
            .executions
            .values()
            .any(|entry| entry.result.is_none())
        {
            return Err(ProviderError::Checkpoint("Cursor checkpoint has an execution with unknown outcome; refusing to execute it again".into()));
        }
        state.validate()?;
        Ok(state)
    }

    pub fn validate(&self) -> Result<()> {
        let mut total = STANDARD
            .decode(&self.checkpoint)
            .map_err(|_| ProviderError::Checkpoint("invalid Cursor checkpoint encoding".into()))?
            .len();
        for (id, data) in &self.blobs {
            let id = STANDARD
                .decode(id)
                .map_err(|_| ProviderError::Checkpoint("invalid Cursor blob identifier".into()))?;
            let data = STANDARD
                .decode(data)
                .map_err(|_| ProviderError::Checkpoint("invalid Cursor blob encoding".into()))?;
            if id.is_empty()
                || id.len() > 4096
                || (id.len() == 32 && sha2::Sha256::digest(&data).as_slice() != id)
            {
                return Err(ProviderError::Checkpoint(
                    "Cursor blob hash mismatch".into(),
                ));
            }
            total = total.saturating_add(data.len());
        }
        if total > MAX_STATE_BYTES {
            return Err(ProviderError::Checkpoint(
                "Cursor checkpoint exceeds storage limit".into(),
            ));
        }
        Ok(())
    }

    pub fn put_json_blob(&mut self, mut value: serde_json::Value) -> Result<Vec<u8>> {
        value.sort_all_objects();
        let bytes = serde_json::to_vec(&value).map_err(|_| {
            ProviderError::Checkpoint("cannot encode Cursor bootstrap history".into())
        })?;
        self.put_blob(&bytes)
    }

    pub fn put_blob(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let id = sha2::Sha256::digest(data).to_vec();
        self.set_blob(&id, data)?;
        Ok(id)
    }

    pub fn set_blob(&mut self, id: &[u8], data: &[u8]) -> Result<()> {
        if id.is_empty()
            || id.len() > 4096
            || (id.len() == 32 && sha2::Sha256::digest(data).as_slice() != id)
        {
            return Err(ProviderError::Checkpoint(
                "Cursor blob hash mismatch".into(),
            ));
        }
        let key = STANDARD.encode(id);
        let value = STANDARD.encode(data);
        if let Some(existing) = self.blobs.get(&key) {
            if existing != &value {
                return Err(ProviderError::Checkpoint(
                    "Cursor blob identity collision".into(),
                ));
            }
            return Ok(());
        }
        let total = self.blobs.values().map(String::len).sum::<usize>() + value.len();
        if total > MAX_STATE_BYTES {
            return Err(ProviderError::Checkpoint(
                "Cursor blob store exceeds limit".into(),
            ));
        }
        self.blobs.insert(key, value);
        Ok(())
    }

    pub fn blob(&self, id: &[u8]) -> Result<Option<Vec<u8>>> {
        self.blobs
            .get(&STANDARD.encode(id))
            .map(|data| {
                STANDARD
                    .decode(data)
                    .map_err(|_| ProviderError::Checkpoint("invalid Cursor blob data".into()))
            })
            .transpose()
    }

    pub fn snapshot(&self, namespace: &Namespace, context_hash: &str) -> Result<Checkpoint> {
        let encoded = serde_json::to_vec(self)
            .map_err(|_| ProviderError::Checkpoint("cannot serialize Cursor checkpoint".into()))?;
        if encoded.len() > MAX_STATE_BYTES {
            return Err(ProviderError::Checkpoint(
                "Cursor checkpoint exceeds storage limit".into(),
            ));
        }
        Ok(Checkpoint {
            version: 1,
            namespace: namespace.clone(),
            context_hash: context_hash.to_string(),
            state: serde_json::to_value(self).map_err(|_| {
                ProviderError::Checkpoint("cannot serialize Cursor checkpoint".into())
            })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_hashes_keep_baseline_bytes_across_json_map_features() {
        let history: Vec<Message> = serde_json::from_str(r#"[{"role":"assistant","content":[{"type":"tool_call","id":"call","name":"lookup","arguments":{"z":{"b":2,"a":1},"a":"first"}}]},{"role":"tool","content":[{"type":"tool_result","call_id":"call","output":{"z":[{"b":2,"a":1}],"a":1},"is_error":true}]}]"#).unwrap();
        assert_eq!(
            hash_messages(&history).unwrap(),
            "69fc38b1385bcdef4d83bf67a33e44f00d58dd185d0a7543cc6f08e24a6042c5"
        );
        let mut state = State::default();
        let id = state
            .put_json_blob(serde_json::json!({"role":"system","content":"rules"}))
            .unwrap();
        assert_eq!(
            hex::encode(sha2::Sha256::digest(state.blob(&id).unwrap().unwrap())),
            "7503473182c40907507eff3c258c9bbb245392d2b44eea3743948fbe85e4b02d"
        );
        assert_eq!(
            state.blob(&id).unwrap().unwrap(),
            br#"{"content":"rules","role":"system"}"#
        );
    }

    #[test]
    fn opaque_history_maps_are_sorted_without_reordering_typed_fields() {
        let history = vec![Message {
            role: codex_provider_api::Role::Assistant,
            content: vec![Content::Opaque {
                namespace: Namespace {
                    provider: "cursor".into(),
                    account: "a".into(),
                    model: "m".into(),
                    thread: "t".into(),
                },
                value: serde_json::from_str(r#"{"z":{"b":2,"a":1},"a":0}"#).unwrap(),
            }],
        }];
        assert_eq!(
            serde_json::to_string(&canonical_messages(&history)).unwrap(),
            r#"[{"role":"assistant","content":[{"type":"opaque","namespace":{"provider":"cursor","account":"a","model":"m","thread":"t"},"value":{"a":0,"z":{"a":1,"b":2}}}]}]"#
        );
    }
}
