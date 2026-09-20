CREATE TABLE schema_version (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at INTEGER NOT NULL
            );
CREATE TABLE chat_sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    title TEXT,
    model TEXT,
    entire_checkpoint_id TEXT,
    entire_session_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    metadata TEXT
);
CREATE INDEX idx_chat_sessions_source ON chat_sessions(source);
CREATE INDEX idx_chat_sessions_entire_session_id ON chat_sessions(entire_session_id);
CREATE INDEX idx_chat_sessions_created_at_desc ON chat_sessions(created_at DESC);
CREATE TABLE chat_messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    sequence_num INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    metadata TEXT
);
CREATE INDEX idx_chat_messages_created_at ON chat_messages(created_at DESC);
CREATE TABLE session_scopes (
    session_id TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    scope_kind TEXT NOT NULL,
    scope_ref TEXT NOT NULL,
    is_primary INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE(session_id, scope_kind, scope_ref)
);
CREATE INDEX idx_session_scopes_kind_ref_created
ON session_scopes(scope_kind, scope_ref, created_at DESC);
CREATE INDEX idx_session_scopes_session ON session_scopes(session_id);
CREATE TABLE import_checkpoints (
    source TEXT NOT NULL,
    scope_ref TEXT NOT NULL,
    cursor TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (source, scope_ref)
);
CREATE TABLE embedding_refs (
    rowid INTEGER PRIMARY KEY,
    ref_type TEXT NOT NULL,
    ref_id TEXT NOT NULL,
    chunk_text TEXT,
    context_header TEXT,
    model_id TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_embedding_refs_ref ON embedding_refs(ref_type, ref_id);
CREATE UNIQUE INDEX idx_chat_messages_session_seq ON chat_messages(session_id, sequence_num);
CREATE TABLE conversations (
    conversation_id TEXT PRIMARY KEY,
    provider_key TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    metadata TEXT
, root_conversation_id TEXT, parent_conversation_id TEXT, branch_kind TEXT, runtime_session_id TEXT, history_incarnation TEXT);
CREATE INDEX idx_conversations_provider_updated_desc
ON conversations(provider_key, updated_at DESC);
CREATE TABLE conversation_aliases (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    provider_key TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_ref TEXT NOT NULL,
    alias_value TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider_key, scope_kind, scope_ref, alias_value)
);
CREATE INDEX idx_conversation_aliases_conversation_updated
ON conversation_aliases(conversation_id, updated_at DESC);
CREATE INDEX idx_conversation_aliases_lookup
ON conversation_aliases(provider_key, scope_kind, scope_ref, alias_value, updated_at DESC);
CREATE INDEX idx_conversations_root_updated_desc
ON conversations(root_conversation_id, updated_at DESC);
CREATE INDEX idx_conversations_parent_updated_desc
ON conversations(parent_conversation_id, updated_at DESC);
CREATE TABLE conversation_messages (
    message_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    sequence_num INTEGER NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_message_key TEXT,
    created_at INTEGER NOT NULL,
    metadata TEXT,
    UNIQUE (conversation_id, sequence_num)
);
CREATE INDEX idx_conversation_messages_created_at_desc
ON conversation_messages(created_at DESC);
CREATE TABLE conversation_state_cache (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    cached_title TEXT,
    preview_text TEXT,
    first_paint_messages_json TEXT,
    last_resume_provider_session_id TEXT,
    last_seen_model_id TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE command_executions (
    id INTEGER PRIMARY KEY,
    command TEXT NOT NULL,
    query TEXT NOT NULL,
    executed_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;
CREATE INDEX idx_conversation_aliases_scope_conversation
ON conversation_aliases(scope_kind, scope_ref, alias_value, conversation_id, updated_at DESC);
CREATE TABLE conversation_turns (
    turn_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    sequence_num INTEGER NOT NULL,
    origin TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER,
    completed_at INTEGER,
    metadata TEXT, source_kind TEXT, source_key TEXT,
    UNIQUE (conversation_id, sequence_num)
);
CREATE INDEX idx_conversation_turns_conversation_sequence
ON conversation_turns(conversation_id, sequence_num);
CREATE TABLE conversation_transcript_items (
    item_id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL REFERENCES conversation_turns(turn_id) ON DELETE CASCADE,
    sequence_num INTEGER NOT NULL,
    kind TEXT NOT NULL,
    provider_message_id TEXT,
    provider_tool_call_id TEXT,
    body TEXT,
    metadata TEXT,
    asset_id TEXT, source_kind TEXT, source_key TEXT, history_content_revision INTEGER NOT NULL DEFAULT 0, history_presentation_revision INTEGER NOT NULL DEFAULT 0, history_preview_json TEXT,
    UNIQUE (turn_id, sequence_num)
);
CREATE INDEX idx_conversation_transcript_items_turn_sequence
ON conversation_transcript_items(turn_id, sequence_num);
CREATE INDEX idx_conversation_transcript_items_tool_call
ON conversation_transcript_items(provider_tool_call_id)
WHERE provider_tool_call_id IS NOT NULL;
CREATE TABLE conversation_assets (
    asset_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    mime_type TEXT,
    file_name TEXT,
    byte_len INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    bytes BLOB,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_conversation_assets_conversation_created
ON conversation_assets(conversation_id, created_at);
CREATE TABLE conversation_transcript_state (
    conversation_id TEXT PRIMARY KEY REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
, history_epoch INTEGER NOT NULL DEFAULT 0, history_revision INTEGER NOT NULL DEFAULT 0);
CREATE INDEX idx_conversation_transcript_state_updated
ON conversation_transcript_state(updated_at);
CREATE TABLE "conversation_bindings" (
    conversation_id TEXT NOT NULL REFERENCES conversations(conversation_id) ON DELETE CASCADE,
    source_id TEXT NOT NULL CHECK (
        source_id IN ('local', 'legacy')
        OR (source_id GLOB 'remote:*' AND length(source_id) > 7)
    ),
    provider_key TEXT NOT NULL,
    binding_kind TEXT NOT NULL,
    binding_value TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (source_id, provider_key, binding_kind, binding_value)
);
CREATE INDEX idx_conversation_bindings_conversation_updated
ON conversation_bindings(conversation_id, updated_at DESC);
CREATE VIRTUAL TABLE fts_conversation_titles USING fts5(
    cached_title,
    preview_text,
    content='conversation_state_cache',
    content_rowid='rowid',
    tokenize='porter unicode61 tokenchars ''_+#'''
)
/* fts_conversation_titles(cached_title,preview_text) */;
CREATE TABLE 'fts_conversation_titles_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE 'fts_conversation_titles_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE 'fts_conversation_titles_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE 'fts_conversation_titles_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TRIGGER fts_conversation_titles_ai AFTER INSERT ON conversation_state_cache BEGIN
    INSERT INTO fts_conversation_titles(rowid, cached_title, preview_text)
    VALUES (new.rowid, new.cached_title, new.preview_text);
END;
CREATE TRIGGER fts_conversation_titles_ad AFTER DELETE ON conversation_state_cache BEGIN
    INSERT INTO fts_conversation_titles(
        fts_conversation_titles, rowid, cached_title, preview_text
    ) VALUES ('delete', old.rowid, old.cached_title, old.preview_text);
END;
CREATE TRIGGER fts_conversation_titles_au AFTER UPDATE ON conversation_state_cache BEGIN
    INSERT INTO fts_conversation_titles(
        fts_conversation_titles, rowid, cached_title, preview_text
    ) VALUES ('delete', old.rowid, old.cached_title, old.preview_text);
    INSERT INTO fts_conversation_titles(rowid, cached_title, preview_text)
    VALUES (new.rowid, new.cached_title, new.preview_text);
END;
CREATE VIRTUAL TABLE fts_conversation_transcript USING fts5(
    body,
    content='conversation_transcript_items',
    content_rowid='rowid',
    tokenize='porter unicode61 tokenchars ''_+#'''
)
/* fts_conversation_transcript(body) */;
CREATE TABLE 'fts_conversation_transcript_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE 'fts_conversation_transcript_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE 'fts_conversation_transcript_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE 'fts_conversation_transcript_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE TRIGGER fts_conversation_transcript_ai
AFTER INSERT ON conversation_transcript_items
WHEN new.kind IN ('user_message', 'assistant_text')
 AND new.body IS NOT NULL
 AND trim(new.body) != ''
BEGIN
    INSERT INTO fts_conversation_transcript(rowid, body)
    VALUES (new.rowid, new.body);
END;
CREATE TRIGGER fts_conversation_transcript_ad
AFTER DELETE ON conversation_transcript_items
WHEN old.kind IN ('user_message', 'assistant_text')
 AND old.body IS NOT NULL
 AND trim(old.body) != ''
BEGIN
    INSERT INTO fts_conversation_transcript(fts_conversation_transcript, rowid, body)
    VALUES ('delete', old.rowid, old.body);
END;
CREATE TABLE automation_runs (
    id TEXT PRIMARY KEY,
    automation_id TEXT NOT NULL,
    record TEXT NOT NULL,
    queued_at INTEGER NOT NULL,
    terminal INTEGER NOT NULL DEFAULT 0,
    unread INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX idx_automation_runs_automation
ON automation_runs(automation_id, queued_at DESC);
CREATE TABLE usage_sessions (
    provider_key TEXT NOT NULL,
    session_key TEXT NOT NULL,
    -- Provider-native secondary identity (e.g. the Codex thread id embedded in
    -- a rollout file name). Used to resolve tabs/bindings to session rows.
    session_alias TEXT,
    source_path TEXT NOT NULL,
    transcript_cwd TEXT,
    inferred_worktree TEXT,
    -- 'none' | 'cwd_prefix' — how inferred_worktree was derived.
    worktree_provenance TEXT NOT NULL DEFAULT 'none',
    -- 'transcript' | 'native_turns' — where the session's facts come from.
    provenance TEXT NOT NULL DEFAULT 'transcript',
    parser_version INTEGER NOT NULL DEFAULT 0,
    cursor_bytes INTEGER NOT NULL DEFAULT 0,
    source_size INTEGER NOT NULL DEFAULT 0,
    source_mtime_ms INTEGER NOT NULL DEFAULT 0,
    -- Provider-specific incremental parse state (JSON), e.g. the last
    -- cumulative Codex token totals needed to delta the next event.
    parser_state TEXT,
    -- 'present' | 'deleted'
    source_state TEXT NOT NULL DEFAULT 'present',
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider_key, session_key)
);
CREATE TABLE usage_facts (
    provider_key TEXT NOT NULL,
    -- Globally stable per-provider fact identity (provider-native ids where
    -- possible). The primary key is the global dedup boundary: forked or
    -- replayed transcript lines that reuse provider message ids collapse here.
    fact_key TEXT NOT NULL,
    session_key TEXT NOT NULL,
    occurred_at_ms INTEGER,
    model_id TEXT NOT NULL DEFAULT '',
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    -- Dollar figure observed in the source itself, with the evidence kind
    -- recorded at ingest time ('provider_billed' | 'provider_estimate' |
    -- 'harness_estimate'). Local catalog estimates are NEVER persisted here —
    -- they are derived at query time from raw tokens.
    source_cost_usd REAL,
    source_cost_kind TEXT,
    PRIMARY KEY (provider_key, fact_key),
    CHECK ((source_cost_usd IS NULL) = (source_cost_kind IS NULL))
);
CREATE INDEX idx_usage_facts_session
    ON usage_facts(provider_key, session_key);
CREATE INDEX idx_usage_facts_occurred_at
    ON usage_facts(occurred_at_ms);
CREATE TABLE usage_fact_claims (
    provider_key TEXT NOT NULL,
    fact_key TEXT NOT NULL,
    session_key TEXT NOT NULL,
    PRIMARY KEY (provider_key, fact_key, session_key)
);
CREATE INDEX idx_usage_fact_claims_session
    ON usage_fact_claims(provider_key, session_key);
CREATE TABLE usage_replace_stage (
    provider_key TEXT NOT NULL,
    session_key TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    parser_version INTEGER NOT NULL DEFAULT 0,
    cursor_bytes INTEGER NOT NULL DEFAULT 0,
    source_size INTEGER NOT NULL DEFAULT 0,
    source_mtime_ms INTEGER NOT NULL DEFAULT 0,
    parser_state TEXT,
    transcript_cwd TEXT,
    session_alias TEXT,
    facts_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (provider_key, session_key, chunk_index)
);
CREATE TABLE conversation_runtime_turn_ids (
    runtime_session_id TEXT NOT NULL,
    durable_turn_id TEXT NOT NULL,
    runtime_turn_id INTEGER NOT NULL CHECK(runtime_turn_id >= 0),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(runtime_session_id, durable_turn_id),
    UNIQUE(runtime_session_id, runtime_turn_id)
);
CREATE TABLE conversation_runtime_commands (
    runtime_session_id TEXT NOT NULL,
    command_id TEXT NOT NULL,
    idempotency_key TEXT,
    kind TEXT NOT NULL,
    request_json TEXT NOT NULL,
    state TEXT NOT NULL,
    response_json TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(runtime_session_id, command_id),
    UNIQUE(runtime_session_id, idempotency_key)
);
CREATE TRIGGER fts_conversation_transcript_au
AFTER UPDATE OF body, kind, rowid ON conversation_transcript_items
BEGIN
    INSERT INTO fts_conversation_transcript(fts_conversation_transcript, rowid, body)
    SELECT 'delete', old.rowid, old.body
    WHERE old.kind IN ('user_message', 'assistant_text')
      AND old.body IS NOT NULL
      AND trim(old.body) != '';
    INSERT INTO fts_conversation_transcript(rowid, body)
    SELECT new.rowid, new.body
    WHERE new.kind IN ('user_message', 'assistant_text')
      AND new.body IS NOT NULL
      AND trim(new.body) != '';
END;
CREATE INDEX idx_conversation_history_user_turn ON conversation_turns(conversation_id, sequence_num) WHERE origin='user';
CREATE INDEX idx_conversation_history_user_item ON conversation_transcript_items(turn_id, sequence_num) WHERE kind='user_message';
CREATE TRIGGER conversation_history_incarnation_insert AFTER INSERT ON conversations
BEGIN
    UPDATE conversations SET history_incarnation = lower(hex(randomblob(16)))
    WHERE conversation_id = NEW.conversation_id;
END;
CREATE UNIQUE INDEX idx_conversations_runtime_session_id
             ON conversations(runtime_session_id)
             WHERE runtime_session_id IS NOT NULL;
CREATE INDEX idx_runtime_commands_recovery
             ON conversation_runtime_commands(runtime_session_id, state, created_at);
