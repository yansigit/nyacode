-- Display caches only. Neither acceptance reconciliation nor continuation may
-- use these tables as authority. Temporary conversations never enter them.
CREATE TABLE chat_conversations (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    title TEXT NOT NULL,
    updated_at_ms INTEGER,
    observed_at_ms INTEGER NOT NULL,
    default_model TEXT,
    continuation_blocker TEXT,
    content_bytes INTEGER NOT NULL CHECK (content_bytes BETWEEN 0 AND 8388608),
    PRIMARY KEY (principal, account, workspace, conversation_id)
);
CREATE INDEX chat_conversations_recency ON chat_conversations (
    principal, account, workspace, observed_at_ms DESC, conversation_id
);
CREATE TABLE chat_messages (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    parent_id TEXT,
    position INTEGER NOT NULL CHECK (position >= 0),
    message TEXT CHECK (message IS NULL OR json_valid(message)),
    PRIMARY KEY (principal, account, workspace, conversation_id, node_id),
    UNIQUE (principal, account, workspace, conversation_id, position),
    FOREIGN KEY (principal, account, workspace, conversation_id)
        REFERENCES chat_conversations (principal, account, workspace, conversation_id) ON DELETE CASCADE
);
CREATE TABLE chat_branches (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    current INTEGER NOT NULL CHECK (current IN (0, 1)),
    PRIMARY KEY (principal, account, workspace, conversation_id, node_id),
    FOREIGN KEY (principal, account, workspace, conversation_id)
        REFERENCES chat_conversations (principal, account, workspace, conversation_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX chat_branches_current ON chat_branches (
    principal, account, workspace, conversation_id
) WHERE current = 1;
CREATE TABLE chat_attachments (
    principal TEXT NOT NULL,
    account TEXT NOT NULL,
    workspace TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    metadata TEXT NOT NULL CHECK (json_valid(metadata)),
    PRIMARY KEY (principal, account, workspace, conversation_id, node_id, position),
    FOREIGN KEY (principal, account, workspace, conversation_id, node_id)
        REFERENCES chat_messages (principal, account, workspace, conversation_id, node_id) ON DELETE CASCADE
);
