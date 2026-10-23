-- Saved Chat state is separate from Codex threads. Temporary Chat never writes here.
-- Every key includes the authenticated principal, account, and routed workspace.
CREATE TABLE chat_profiles (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    device_id TEXT NOT NULL,
    profile_version TEXT NOT NULL,
    PRIMARY KEY (principal, account, workspace)
);

CREATE TABLE chat_drafts (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    draft_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    conversation_id TEXT,
    expected_parent_id TEXT,
    requested_model TEXT,
    text TEXT NOT NULL,
    selections TEXT NOT NULL CHECK (json_valid(selections)),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (principal, account, workspace, draft_id)
);
CREATE INDEX chat_drafts_recency ON chat_drafts (
    principal, account, workspace, updated_at_ms DESC, draft_id DESC
);

-- Keep stable submission IDs after terminal outcomes. Reusing an ID never sends again.
-- The snapshot contains only typed text, model, file references and file metadata.
-- It contains no file bytes, signed URLs, live upload handles, or authentication.
CREATE TABLE chat_submissions (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    client_message_id TEXT NOT NULL,
    draft_id TEXT NOT NULL,
    draft_revision TEXT NOT NULL,
    conversation_id TEXT,
    expected_parent_id TEXT NOT NULL,
    target_key TEXT NOT NULL,
    snapshot TEXT NOT NULL CHECK (json_valid(snapshot)),
    transmission_started INTEGER NOT NULL DEFAULT 0 CHECK (transmission_started IN (0, 1)),
    accepted INTEGER NOT NULL DEFAULT 0 CHECK (accepted IN (0, 1)),
    resolved_model TEXT,
    outcome TEXT NOT NULL DEFAULT 'in_progress'
        CHECK (outcome IN ('in_progress', 'completed', 'failed', 'local_stopped', 'unsupported_handoff')),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    owner_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    lease_until_ms INTEGER NOT NULL,
    may_transmit INTEGER NOT NULL DEFAULT 1 CHECK (may_transmit IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (principal, account, workspace, submission_id),
    UNIQUE (principal, account, workspace, client_message_id),
    CHECK (accepted = 0 OR (transmission_started = 1 AND conversation_id IS NOT NULL)),
    CHECK (outcome != 'completed' OR accepted = 1),
    CHECK (may_transmit = 0 OR (transmission_started = 0 AND active = 1)),
    CHECK (active = 1 OR (outcome != 'in_progress' AND (transmission_started = 0 OR accepted = 1)))
);
-- Both the draft and its authoritative conversation are serialized across processes.
CREATE UNIQUE INDEX chat_submissions_active_target ON chat_submissions (
    principal, account, workspace, target_key
) WHERE active = 1;
CREATE UNIQUE INDEX chat_submissions_active_draft ON chat_submissions (
    principal, account, workspace, draft_id
) WHERE active = 1;
CREATE INDEX chat_submissions_pending ON chat_submissions (
    principal, account, workspace, active, created_at_ms, submission_id
);
