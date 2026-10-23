//! Private harness recovery records. Temporary conversations must never create
//! this object. Only this typed schema reaches disk: no cookies, signed URLs,
//! authentication, preparation tokens, raw wire frames, or attachment bytes.

use crate::stream::Acceptance;
use crate::stream::GenerationOutcome;
use crate::stream::Message;
use crate::stream::TurnState;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const MAX_JOURNAL: u64 = 16 * 1024 * 1024;
const PROFILE: &str = crate::session::NATIVE_PROFILE_VERSION;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error("private saved-run recovery storage is unavailable")]
    Unavailable,
    #[error("this saved run is already owned by another local process")]
    InUse,
    #[error("this saved run belongs to a different authenticated principal or workspace")]
    ScopeMismatch,
    #[error("this saved run has an unsupported or malformed recovery record")]
    InvalidRecord,
    #[error("a previous saved submission requires reconciliation; it cannot be resent")]
    PendingSubmission,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunScope {
    pub principal: String,
    pub account: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedAttempt {
    pub submission_id: String,
    pub client_message_id: String,
    pub parent_message_id: String,
    pub conversation_id: Option<String>,
    pub text: String,
    pub requested_model: String,
    pub resolved_model: Option<String>,
    pub created_at: f64,
    pub transmission_started: bool,
    pub acceptance: Acceptance,
    pub outcome: GenerationOutcome,
    pub authoritative: bool,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub attachments: Vec<SavedAttachment>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedAttachment {
    pub server_file_id: String,
    pub metadata: crate::attachment::AttachmentMetadata,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    run_id: String,
    scope: RunScope,
    profile: String,
    device: String,
    attempts: Vec<SavedAttempt>,
}

/// Holds an exclusive lock on a separate, stable inode across journal renames.
/// A crashed owner releases the OS lock; acquiring it only permits reading and
/// reconciliation. It never grants permission to replay a pending submission.
pub struct RunJournal {
    directory: PathBuf,
    _lock: File,
    record: Record,
    poisoned: bool,
}

impl RunJournal {
    pub fn create(home: &Path, scope: RunScope) -> Result<Self, JournalError> {
        validate_scope(&scope)?;
        let root = home.join("chat-probe-runs");
        private_directory(&root, true)?;
        let run_id = Uuid::new_v4().to_string();
        let directory = root.join(&run_id);
        // Run IDs are generated here; an existing directory is never reused.
        create_directory(&directory)?;
        let lock = lock(&directory, true)?;
        let mut journal = Self {
            directory,
            _lock: lock,
            poisoned: false,
            record: Record {
                version: 1,
                run_id,
                scope,
                profile: PROFILE.into(),
                device: Uuid::new_v4().to_string(),
                attempts: Vec::new(),
            },
        };
        journal.persist()?;
        File::open(root)
            .and_then(|file| file.sync_all())
            .map_err(|_| JournalError::Unavailable)?;
        Ok(journal)
    }

    pub fn resume(home: &Path, run_id: &str, scope: &RunScope) -> Result<Self, JournalError> {
        validate_scope(scope)?;
        if !canonical_uuid(run_id) {
            return Err(JournalError::InvalidRecord);
        }
        let root = home.join("chat-probe-runs");
        private_directory(&root, false)?;
        let directory = root.join(run_id);
        private_directory(&directory, false)?;
        let lock = lock(&directory, false)?;
        let path = directory.join("journal.json");
        private_file(&path)?;
        let file = File::open(path).map_err(|_| JournalError::Unavailable)?;
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| JournalError::Unavailable)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(JournalError::InvalidRecord);
        }
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|_| JournalError::InvalidRecord)?;
        if &record.scope != scope {
            return Err(JournalError::ScopeMismatch);
        }
        if record.version != 1
            || record.run_id != run_id
            || record.profile != PROFILE
            || !canonical_uuid(&record.device)
            || record.attempts.len() > 128
        {
            return Err(JournalError::InvalidRecord);
        }
        for attempt in &record.attempts {
            if !canonical_uuid(&attempt.submission_id)
                || !canonical_uuid(&attempt.client_message_id)
                || !valid_id(&attempt.parent_message_id)
                || attempt
                    .conversation_id
                    .as_deref()
                    .is_some_and(|id| !valid_id(id))
                || attempt.text.len() > 1024 * 1024
                || attempt.requested_model.len() > 256
                || !attempt.created_at.is_finite()
                || !valid_attachments(&attempt.attachments)
            {
                return Err(JournalError::InvalidRecord);
            }
        }
        Ok(Self {
            directory,
            _lock: lock,
            record,
            poisoned: false,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.record.run_id
    }
    pub fn device(&self) -> &str {
        &self.record.device
    }
    pub fn scope(&self) -> &RunScope {
        &self.record.scope
    }
    pub fn current(&self) -> Option<&SavedAttempt> {
        self.record.attempts.last()
    }

    pub fn begin(
        &mut self,
        conversation: Option<String>,
        parent: String,
        text: String,
    ) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Unavailable);
        }
        if self.current().is_some_and(|attempt| {
            !attempt.authoritative
                || attempt.acceptance != Acceptance::Accepted
                || attempt.outcome != GenerationOutcome::Completed
        }) {
            return Err(JournalError::PendingSubmission);
        }
        if self.record.attempts.len() >= 128
            || !valid_id(&parent)
            || conversation.as_deref().is_some_and(|id| !valid_id(id))
            || text.is_empty()
            || text.len() > 1024 * 1024
        {
            return Err(JournalError::InvalidRecord);
        }
        self.record.attempts.push(SavedAttempt {
            submission_id: Uuid::new_v4().to_string(),
            client_message_id: Uuid::new_v4().to_string(),
            parent_message_id: parent,
            conversation_id: conversation,
            text,
            requested_model: "auto".into(),
            resolved_model: None,
            created_at: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            transmission_started: false,
            acceptance: Acceptance::Unknown,
            outcome: GenerationOutcome::Incomplete,
            authoritative: false,
            messages: Vec::new(),
            attachments: Vec::new(),
        });
        self.persist()
    }

    pub(crate) fn attach(&mut self, attachments: Vec<SavedAttachment>) -> Result<(), JournalError> {
        if !valid_attachments(&attachments) {
            return Err(JournalError::InvalidRecord);
        }
        let attempt = self
            .record
            .attempts
            .last_mut()
            .ok_or(JournalError::InvalidRecord)?;
        if attempt.transmission_started {
            return Err(JournalError::PendingSubmission);
        }
        if !attempt.attachments.is_empty() && attempt.attachments != attachments {
            return Err(JournalError::InvalidRecord);
        }
        attempt.attachments = attachments;
        self.persist()
    }

    /// The durable uncertainty boundary. After this succeeds, every interruption
    /// requires reconciliation, even if the socket was never actually written.
    pub fn mark_transmitting(&mut self, requested_model: &str) -> Result<(), JournalError> {
        let attempt = self
            .record
            .attempts
            .last_mut()
            .ok_or(JournalError::InvalidRecord)?;
        if attempt.transmission_started {
            return Err(JournalError::PendingSubmission);
        }
        if requested_model.is_empty() || requested_model.len() > 256 {
            return Err(JournalError::InvalidRecord);
        }
        attempt.requested_model = requested_model.into();
        attempt.transmission_started = true;
        self.persist()
    }

    /// Durably capture verified identifiers as soon as the stream exposes them.
    /// Contents are checkpointed at stream termination, not on every token.
    pub fn observe(
        &mut self,
        state: &TurnState,
        retain_contents: bool,
    ) -> Result<(), JournalError> {
        let attempt = self
            .record
            .attempts
            .last_mut()
            .ok_or(JournalError::InvalidRecord)?;
        if !attempt.transmission_started {
            return Err(JournalError::InvalidRecord);
        }
        let mut changed = retain_contents;
        if let Some(id) = &state.conversation_id {
            if attempt
                .conversation_id
                .as_ref()
                .is_some_and(|old| old != id)
            {
                return Err(JournalError::InvalidRecord);
            }
            if attempt.conversation_id.is_none() {
                attempt.conversation_id = Some(id.clone());
                changed = true;
            }
        }
        if state.acceptance == Acceptance::Accepted && attempt.acceptance != Acceptance::Accepted {
            attempt.acceptance = Acceptance::Accepted;
            changed = true;
        }
        if retain_contents {
            attempt.messages = state.messages.clone();
            attempt.resolved_model.clone_from(&state.resolved_model);
            attempt.outcome = state.outcome();
        }
        if changed {
            self.persist()?;
        }
        Ok(())
    }

    pub(crate) fn reconciled(
        &mut self,
        messages: Vec<Message>,
        outcome: GenerationOutcome,
    ) -> Result<(), JournalError> {
        let attempt = self
            .record
            .attempts
            .last_mut()
            .ok_or(JournalError::InvalidRecord)?;
        attempt.acceptance = Acceptance::Accepted;
        attempt.outcome = outcome;
        attempt.authoritative = true;
        attempt.resolved_model = messages
            .iter()
            .rev()
            .find_map(|message| message.model.clone());
        attempt.messages = messages;
        self.persist()
    }

    fn persist(&mut self) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Unavailable);
        }
        let result = self.write_atomic();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn write_atomic(&self) -> Result<(), JournalError> {
        let bytes = serde_json::to_vec(&self.record).map_err(|_| JournalError::InvalidRecord)?;
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(JournalError::InvalidRecord);
        }
        let path = self.directory.join(format!("{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            set_private_mode(&mut options)?;
            let mut file = options.open(&path).map_err(|_| JournalError::Unavailable)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| JournalError::Unavailable)?;
            std::fs::rename(&path, self.directory.join("journal.json"))
                .map_err(|_| JournalError::Unavailable)?;
            File::open(&self.directory)
                .and_then(|file| file.sync_all())
                .map_err(|_| JournalError::Unavailable)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }
}

fn valid_attachments(attachments: &[SavedAttachment]) -> bool {
    attachments.len() <= 20
        && attachments.iter().all(|a| {
            valid_id(&a.server_file_id)
                && !a.metadata.name.is_empty()
                && a.metadata.name.len() <= 255
                && a.metadata.sha256.len() == 64
                && a.metadata.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        })
        && attachments
            .iter()
            .try_fold(0u64, |sum, a| sum.checked_add(a.metadata.size))
            .is_some_and(|sum| sum <= crate::attachment::MAX_SUBMISSION_BYTES as u64)
}

fn validate_scope(scope: &RunScope) -> Result<(), JournalError> {
    if !valid_id(&scope.principal) || !valid_id(&scope.account) {
        return Err(JournalError::ScopeMismatch);
    }
    Ok(())
}

fn canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn lock(directory: &Path, create: bool) -> Result<File, JournalError> {
    let path = directory.join("owner.lock");
    if !create {
        private_file(&path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create);
    set_private_mode(&mut options)?;
    let file = options.open(path).map_err(|_| JournalError::Unavailable)?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => JournalError::InUse,
        std::fs::TryLockError::Error(_) => JournalError::Unavailable,
    })?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_mode(options: &mut OpenOptions) -> Result<(), JournalError> {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
    Ok(())
}

#[cfg(not(unix))]
fn set_private_mode(_: &mut OpenOptions) -> Result<(), JournalError> {
    Err(JournalError::Unavailable)
}

#[cfg(unix)]
fn create_directory(path: &Path) -> Result<(), JournalError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| JournalError::Unavailable)
}

#[cfg(not(unix))]
fn create_directory(_: &Path) -> Result<(), JournalError> {
    Err(JournalError::Unavailable)
}

fn private_directory(path: &Path, create: bool) -> Result<(), JournalError> {
    if create && !path.try_exists().map_err(|_| JournalError::Unavailable)? {
        // A concurrent creator may win; validate its result below.
        let _ = create_directory(path);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| JournalError::Unavailable)?;
    if !metadata.is_dir() || !private_permissions(&metadata) {
        return Err(JournalError::Unavailable);
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<(), JournalError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| JournalError::Unavailable)?;
    if !metadata.is_file() || !private_permissions(&metadata) {
        return Err(JournalError::Unavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(JournalError::Unavailable);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn private_permissions(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn private_permissions(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    struct Home(PathBuf);
    impl Home {
        fn new() -> Result<Self, JournalError> {
            let path = std::env::temp_dir().join(format!("chat-journal-test-{}", Uuid::new_v4()));
            create_directory(&path)?;
            Ok(Self(path))
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn scope() -> RunScope {
        RunScope {
            principal: "fixture-user".into(),
            account: "fixture-workspace".into(),
        }
    }

    #[test]
    fn crash_recovery_preserves_pending_identity_and_never_allows_resend() {
        let home = Home::new().unwrap();
        let mut run = RunJournal::create(&home.0, scope()).unwrap();
        let id = run.run_id().to_owned();
        let device = run.device().to_owned();
        run.begin(None, Uuid::new_v4().to_string(), "harmless fixture".into())
            .unwrap();
        let message = run.current().unwrap().client_message_id.clone();
        run.mark_transmitting("fixture-model").unwrap();
        drop(run);
        let mut recovered = RunJournal::resume(&home.0, &id, &scope()).unwrap();
        assert_eq!(recovered.device(), device);
        assert_eq!(recovered.current().unwrap().client_message_id, message);
        assert_eq!(recovered.current().unwrap().acceptance, Acceptance::Unknown);
        assert_eq!(
            recovered.mark_transmitting("fixture-model"),
            Err(JournalError::PendingSubmission)
        );
        assert_eq!(
            recovered.begin(None, "parent".into(), "duplicate".into()),
            Err(JournalError::PendingSubmission)
        );
    }

    #[test]
    fn stable_lock_and_scope_isolate_clients_and_principals() {
        let home = Home::new().unwrap();
        let mut run = RunJournal::create(&home.0, scope()).unwrap();
        let id = run.run_id().to_owned();
        run.begin(None, "parent".into(), "fixture".into()).unwrap();
        assert!(matches!(
            RunJournal::resume(&home.0, &id, &scope()),
            Err(JournalError::InUse)
        ));
        run.mark_transmitting("fixture-model").unwrap();
        assert!(matches!(
            RunJournal::resume(&home.0, &id, &scope()),
            Err(JournalError::InUse)
        ));
        drop(run);
        for other in [
            RunScope {
                principal: "other".into(),
                ..scope()
            },
            RunScope {
                account: "other".into(),
                ..scope()
            },
        ] {
            assert!(matches!(
                RunJournal::resume(&home.0, &id, &other),
                Err(JournalError::ScopeMismatch)
            ));
        }
        assert!(matches!(
            RunJournal::resume(&home.0, "../escape", &scope()),
            Err(JournalError::InvalidRecord)
        ));
    }

    #[test]
    fn journal_failures_poison_the_writer_and_unsafe_permissions_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let home = Home::new().unwrap();
        let mut run = RunJournal::create(&home.0, scope()).unwrap();
        let root = home.0.join("chat-probe-runs");
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(run.directory.join("journal.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        run.begin(None, "parent".into(), "fixture".into()).unwrap();
        std::fs::rename(&run.directory, root.join("moved")).unwrap();
        assert_eq!(
            run.mark_transmitting("fixture"),
            Err(JournalError::Unavailable)
        );
        assert_eq!(run.persist(), Err(JournalError::Unavailable));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            RunJournal::create(&home.0, scope()),
            Err(JournalError::Unavailable)
        ));
    }
}
