//! Original implementation of the observed v1 wire contract. Inherited delta
//! headers are shared across channels; accumulated values are channel-local.

use super::StreamError;
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_CONTENT: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 8192;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy)]
enum Operation {
    Add,
    Remove,
    Replace,
    Append,
    Truncate,
    Patch,
}

impl Operation {
    fn parse(value: &Value) -> Result<Self, StreamError> {
        Ok(match value.as_str() {
            Some("add") => Self::Add,
            Some("remove") => Self::Remove,
            Some("replace") => Self::Replace,
            Some("append") => Self::Append,
            Some("truncate") => Self::Truncate,
            Some("patch") => Self::Patch,
            _ => return Err(StreamError::InvalidDelta),
        })
    }
}

struct Delta {
    path: String,
    operation: Operation,
    value: Option<Value>,
}

/// Content-bearing state deliberately has no Debug or Serialize implementation.
pub struct DeltaDecoder {
    channels: BTreeMap<u32, (Option<Value>, usize)>,
    previous_channel: u32,
    previous_path: String,
    previous_operation: Operation,
    retained: usize,
    failed: bool,
}

impl Default for DeltaDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DeltaDecoder {
    pub fn new() -> Self {
        Self {
            channels: BTreeMap::new(),
            previous_channel: 0,
            previous_path: String::new(),
            previous_operation: Operation::Add,
            retained: 0,
            failed: false,
        }
    }

    /// Applies one already parsed `event: delta` payload. The caller must first
    /// validate `delta_encoding: v1`. Missing values never inherit. A failure
    /// poisons the decoder, preventing use of any partly applied batch.
    pub fn apply(&mut self, input: Value) -> Result<Option<&Value>, StreamError> {
        if self.failed {
            return Err(StreamError::DecoderFailed);
        }
        match self.apply_inner(input) {
            Ok(channel) => Ok(self
                .channels
                .get(&channel)
                .and_then(|(value, _)| value.as_ref())),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn apply_inner(&mut self, input: Value) -> Result<u32, StreamError> {
        weight(&input, 0)?;
        let Value::Object(mut fields) = input else {
            return Err(StreamError::InvalidDelta);
        };
        let channel = match fields.remove("c") {
            Some(value) => value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(StreamError::InvalidDelta)?,
            None => self.previous_channel,
        };
        let delta = parse_delta(fields, &self.previous_path, self.previous_operation)?;
        if !self.channels.contains_key(&channel) && self.channels.len() >= 32 {
            return Err(StreamError::ContentLimit);
        }
        self.previous_channel = channel;
        self.previous_path.clone_from(&delta.path);
        self.previous_operation = delta.operation;
        let (mut current, old_weight) = self.channels.remove(&channel).unwrap_or((None, 0));
        apply_delta(&mut current, delta, 0)?;
        let new_weight = current.as_ref().map_or(Ok(0), |value| weight(value, 0))?;
        self.retained = self.retained - old_weight + new_weight;
        if self.retained > MAX_CONTENT {
            return Err(StreamError::ContentLimit);
        }
        self.channels.insert(channel, (current, new_weight));
        Ok(channel)
    }
}

fn parse_delta(
    mut fields: serde_json::Map<String, Value>,
    previous_path: &str,
    previous_operation: Operation,
) -> Result<Delta, StreamError> {
    let path = match fields.remove("p") {
        Some(Value::String(path)) => path,
        Some(_) => return Err(StreamError::InvalidDelta),
        None => previous_path.to_owned(),
    };
    if path.len() > 2048 {
        return Err(StreamError::ContentLimit);
    }
    let operation = fields
        .remove("o")
        .as_ref()
        .map(Operation::parse)
        .transpose()?
        .unwrap_or(previous_operation);
    let value = fields.remove("v");
    if !fields.is_empty() || (value.is_none() && !matches!(operation, Operation::Remove)) {
        return Err(StreamError::InvalidDelta);
    }
    Ok(Delta {
        path,
        operation,
        value,
    })
}

fn path_parts(path: &str) -> Result<Vec<String>, StreamError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<_> = path
        .strip_prefix('/')
        .unwrap_or(path)
        .split('/')
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .collect();
    if parts.len() > MAX_DEPTH {
        return Err(StreamError::ContentLimit);
    }
    Ok(parts)
}

fn index(key: &str) -> Option<usize> {
    key.parse().ok().filter(|n: &usize| n.to_string() == key)
}

fn container(key: &str) -> Value {
    if index(key).is_some() {
        Value::Array(Vec::new())
    } else {
        Value::Object(serde_json::Map::new())
    }
}

fn apply_delta(current: &mut Option<Value>, delta: Delta, depth: usize) -> Result<(), StreamError> {
    if depth > MAX_DEPTH {
        return Err(StreamError::ContentLimit);
    }
    let path = path_parts(&delta.path)?;
    if path.is_empty() {
        return apply_value(current, delta.operation, delta.value, depth + 1);
    }
    let root = current.get_or_insert_with(|| container(&path[0]));
    apply_path(root, &path, delta.operation, delta.value, depth + 1)
}

fn apply_path(
    current: &mut Value,
    path: &[String],
    operation: Operation,
    value: Option<Value>,
    depth: usize,
) -> Result<(), StreamError> {
    if depth > MAX_DEPTH {
        return Err(StreamError::ContentLimit);
    }
    let (key, tail) = path.split_first().ok_or(StreamError::InvalidDelta)?;
    match current {
        Value::Object(fields) => {
            let mut child = fields.remove(key);
            if tail.is_empty() {
                apply_value(&mut child, operation, value, depth + 1)?;
            } else {
                apply_path(
                    child.get_or_insert_with(|| container(&tail[0])),
                    tail,
                    operation,
                    value,
                    depth + 1,
                )?;
            }
            if let Some(child) = child {
                if fields.len() >= MAX_ITEMS {
                    return Err(StreamError::ContentLimit);
                }
                fields.insert(key.clone(), child);
            }
            Ok(())
        }
        Value::Array(items) => {
            let i = index(key).ok_or(StreamError::InvalidDelta)?;
            // Sparse expansion is not observed and is explicitly unsupported;
            // never allocate attacker-selected gaps or reinterpret null holes.
            if i > items.len() || i >= MAX_ITEMS {
                return Err(StreamError::ContentLimit);
            }
            if tail.is_empty() && matches!(operation, Operation::Add) {
                if items.len() >= MAX_ITEMS {
                    return Err(StreamError::ContentLimit);
                }
                items.insert(i, value.ok_or(StreamError::InvalidDelta)?);
                return Ok(());
            }
            if tail.is_empty() && matches!(operation, Operation::Remove) {
                if i < items.len() {
                    items.remove(i);
                }
                return Ok(());
            }
            let mut child = if i < items.len() {
                Some(std::mem::take(&mut items[i]))
            } else {
                None
            };
            if tail.is_empty() {
                apply_value(&mut child, operation, value, depth + 1)?;
            } else {
                apply_path(
                    child.get_or_insert_with(|| container(&tail[0])),
                    tail,
                    operation,
                    value,
                    depth + 1,
                )?;
            }
            let child = child.ok_or(StreamError::InvalidDelta)?;
            if i == items.len() {
                items.push(child);
            } else {
                items[i] = child;
            }
            Ok(())
        }
        _ => Err(StreamError::InvalidDelta),
    }
}

fn apply_value(
    current: &mut Option<Value>,
    operation: Operation,
    value: Option<Value>,
    depth: usize,
) -> Result<(), StreamError> {
    if depth > MAX_DEPTH {
        return Err(StreamError::ContentLimit);
    }
    if matches!(operation, Operation::Remove) {
        *current = None;
        return Ok(());
    }
    let value = value.ok_or(StreamError::InvalidDelta)?;
    match operation {
        Operation::Add | Operation::Replace => *current = Some(value),
        Operation::Append => match (current.as_mut(), value) {
            (Some(Value::String(text)), Value::String(suffix)) => {
                if text.len().saturating_add(suffix.len()) > MAX_CONTENT {
                    return Err(StreamError::ContentLimit);
                }
                text.push_str(&suffix);
            }
            (Some(Value::String(_)), _) => return Err(StreamError::InvalidDelta),
            (Some(Value::Array(items)), value) => {
                let extra = match value {
                    Value::Array(items) => items,
                    item => vec![item],
                };
                if items.len().saturating_add(extra.len()) > MAX_ITEMS {
                    return Err(StreamError::ContentLimit);
                }
                items.extend(extra);
            }
            (Some(Value::Object(fields)), Value::Object(extra)) => {
                fields.extend(extra);
                if fields.len() > MAX_ITEMS {
                    return Err(StreamError::ContentLimit);
                }
            }
            (_, value) => *current = Some(value),
        },
        Operation::Patch => {
            let Value::Array(operations) = value else {
                return Err(StreamError::InvalidDelta);
            };
            if operations.len() > MAX_ITEMS {
                return Err(StreamError::ContentLimit);
            }
            for item in operations {
                let Value::Object(fields) = item else {
                    return Err(StreamError::InvalidDelta);
                };
                // Nested entries do not inherit the preceding top-level delta.
                if !fields.contains_key("o") {
                    return Err(StreamError::InvalidDelta);
                }
                apply_delta(current, parse_delta(fields, "", Operation::Add)?, depth + 1)?;
            }
        }
        Operation::Truncate => {
            let length = value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or(StreamError::InvalidDelta)?;
            match current.as_mut() {
                Some(Value::String(text)) => {
                    let mut units = 0;
                    let mut boundary = text.len();
                    for (offset, character) in text.char_indices() {
                        if units == length {
                            boundary = offset;
                            break;
                        }
                        units += character.len_utf16();
                        if units > length {
                            return Err(StreamError::InvalidDelta);
                        }
                    }
                    text.truncate(boundary);
                }
                Some(Value::Array(items)) if length <= items.len() => items.truncate(length),
                Some(Value::Array(_)) => return Err(StreamError::ContentLimit),
                _ => {}
            }
        }
        Operation::Remove => return Err(StreamError::InvalidDelta),
    }
    Ok(())
}

fn weight(value: &Value, depth: usize) -> Result<usize, StreamError> {
    if depth > MAX_DEPTH {
        return Err(StreamError::ContentLimit);
    }
    let mut size = 64;
    match value {
        Value::String(text) => size += text.len(),
        Value::Array(items) => {
            if items.len() > MAX_ITEMS {
                return Err(StreamError::ContentLimit);
            }
            for value in items {
                size += weight(value, depth + 1)?;
                if size > MAX_CONTENT {
                    return Err(StreamError::ContentLimit);
                }
            }
        }
        Value::Object(fields) => {
            if fields.len() > MAX_ITEMS {
                return Err(StreamError::ContentLimit);
            }
            for (key, value) in fields {
                size += 64 + key.len() + weight(value, depth + 1)?;
                if size > MAX_CONTENT {
                    return Err(StreamError::ContentLimit);
                }
            }
        }
        _ => {}
    }
    if size > MAX_CONTENT {
        Err(StreamError::ContentLimit)
    } else {
        Ok(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn header_inheritance_is_global_while_values_are_channel_local() {
        let mut decoder = DeltaDecoder::new();
        assert_eq!(
            decoder.apply(json!({"v":{"text":"one"}})).unwrap(),
            Some(&json!({"text":"one"}))
        );
        decoder.apply(json!({"c":1,"v":{"text":"two"}})).unwrap();
        decoder
            .apply(json!({"p":"/text","o":"append","v":"!"}))
            .unwrap();
        assert_eq!(
            decoder.apply(json!({"c":0,"v":"?"})).unwrap(),
            Some(&json!({"text":"one?"}))
        );
        assert_eq!(
            decoder.apply(json!({"c":1,"v":"?"})).unwrap(),
            Some(&json!({"text":"two!?"}))
        );
        assert!(matches!(
            decoder.apply(json!({})),
            Err(StreamError::InvalidDelta)
        ));
        assert!(matches!(
            decoder.apply(json!({"v":"retry"})),
            Err(StreamError::DecoderFailed)
        ));
    }

    #[test]
    fn relative_patch_batches_insert_remove_replace_and_append() {
        let mut decoder = DeltaDecoder::new();
        decoder
            .apply(json!({"v":{"message":{"parts":["a","c"],"metadata":{"old":1}}}}))
            .unwrap();
        let result = decoder
            .apply(json!({"p":"/message","o":"patch","v":[
                {"p":"/parts/1","o":"add","v":"b"},
                {"p":"/parts/0","o":"remove"},
                {"p":"/parts/1","o":"replace","v":"d"},
                {"p":"/parts","o":"append","v":["e","f"]},
                {"p":"/metadata","o":"append","v":{"new":2}},
                {"p":"/metadata/old","o":"remove"}
            ]}))
            .unwrap();
        assert_eq!(
            result,
            Some(&json!({"message":{"parts":["b","d","e","f"],"metadata":{"new":2}}}))
        );
    }

    #[test]
    fn pointers_missing_containers_and_utf16_truncation_are_explicit() {
        let mut decoder = DeltaDecoder::new();
        decoder
            .apply(json!({"p":"/a~1b/~0key/0","v":"é😀x"}))
            .unwrap();
        assert_eq!(
            decoder.apply(json!({"o":"truncate","v":3})).unwrap(),
            Some(&json!({"a/b":{"~key":["é😀"]}}))
        );
        assert!(matches!(
            decoder.apply(json!({"v":2})),
            Err(StreamError::InvalidDelta)
        ));
        let mut decoder = DeltaDecoder::new();
        assert_eq!(
            decoder.apply(json!({"v":true})).unwrap(),
            Some(&json!(true))
        );
        assert_eq!(decoder.apply(json!({"o":"remove"})).unwrap(), None);
        assert_eq!(
            decoder
                .apply(json!({"p":"/fresh","o":"add","v":1}))
                .unwrap(),
            Some(&json!({"fresh":1}))
        );
    }

    #[test]
    fn malformed_operations_and_expansion_limits_never_return_partial_batches() {
        for input in [
            json!({"c":-1,"v":0}),
            json!({"o":"unknown","v":0}),
            json!({"v":0,"unexpected":true}),
            json!({"o":"patch","v":[{"v":1}]}),
            json!({"p":"/parts/999999999","v":"x"}),
        ] {
            let mut decoder = DeltaDecoder::new();
            assert!(decoder.apply(input).is_err());
            assert!(matches!(
                decoder.apply(json!({"v":null})),
                Err(StreamError::DecoderFailed)
            ));
        }
        let mut decoder = DeltaDecoder::new();
        for channel in 0..32 {
            decoder.apply(json!({"c":channel,"v":null})).unwrap();
        }
        assert!(matches!(
            decoder.apply(json!({"c":32,"v":null})),
            Err(StreamError::ContentLimit)
        ));
    }
}
