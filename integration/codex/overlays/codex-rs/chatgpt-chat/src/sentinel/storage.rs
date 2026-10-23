//! Native session storage starts empty. Nothing is imported from a browser or
//! written to disk; only values written by this session can subsequently exist.

use super::Result;
use super::SentinelFailure;

const MAX_BYTES: usize = 64 * 1024;
const MAX_ENTRIES: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StorageKind {
    Local,
    Session,
}

#[derive(Default)]
pub(super) struct MemoryStorage {
    entries: Vec<(String, String)>,
    bytes: usize,
}

impl MemoryStorage {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn estimate(&self) -> (usize, usize) {
        (self.bytes, MAX_BYTES)
    }

    pub fn set(&mut self, key: String, value: String) -> Result<()> {
        let existing = self.entries.iter().position(|(k, _)| k == &key);
        let old = existing.map_or(0, |i| self.entries[i].0.len() + self.entries[i].1.len());
        let bytes = self
            .bytes
            .saturating_sub(old)
            .saturating_add(key.len())
            .saturating_add(value.len());
        if bytes > MAX_BYTES || (existing.is_none() && self.len() >= MAX_ENTRIES) {
            return Err(SentinelFailure::AllocationLimit);
        }
        if let Some(i) = existing {
            self.entries[i].1 = value;
        } else {
            self.entries.push((key, value));
        }
        self.bytes = bytes;
        Ok(())
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(i) = self.entries.iter().position(|(k, _)| k == key) {
            let (key, value) = self.entries.remove(i);
            self.bytes -= key.len() + value.len();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.entries.shrink_to_fit();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_session_writes_exist_and_updates_preserve_key_order() {
        let mut storage = MemoryStorage::default();
        assert_eq!(storage.get("browser-token"), None);
        storage.set("a".into(), "one".into()).unwrap();
        storage.set("b".into(), "two".into()).unwrap();
        storage.set("a".into(), "updated".into()).unwrap();
        assert_eq!(storage.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(storage.get("a"), Some("updated"));
        storage.remove("a");
        assert_eq!(storage.get("a"), None);
        storage.clear();
        assert_eq!(storage.len(), 0);
        assert_eq!(storage.bytes, 0);
    }

    #[test]
    fn quota_failures_leave_previous_values_intact() {
        let mut storage = MemoryStorage::default();
        storage.set("key".into(), "value".into()).unwrap();
        assert_eq!(
            storage.set("key".into(), "x".repeat(MAX_BYTES)),
            Err(SentinelFailure::AllocationLimit)
        );
        assert_eq!(storage.get("key"), Some("value"));
        for i in 1..MAX_ENTRIES {
            storage.set(i.to_string(), String::new()).unwrap();
        }
        assert_eq!(
            storage.set("extra".into(), String::new()),
            Err(SentinelFailure::AllocationLimit)
        );
        storage.clear();
        storage.set("key".into(), "available".into()).unwrap();
    }
}
