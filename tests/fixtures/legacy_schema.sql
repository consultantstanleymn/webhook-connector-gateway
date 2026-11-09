
CREATE TABLE IF NOT EXISTS tenants (
    id              TEXT PRIMARY KEY,
    api_key_hash    TEXT NOT NULL UNIQUE,
    secret          TEXT NOT NULL,
    prev_secret     TEXT,
    prev_expires_at INTEGER,
    created_at      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_hash    TEXT NOT NULL,
    payload         TEXT NOT NULL,
    trace_id        TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    locked_until    INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (tenant_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS events_pending_idx ON events (next_attempt_at) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS events_processing_idx ON events (locked_until) WHERE status = 'processing';
CREATE INDEX IF NOT EXISTS events_tenant_idx ON events (tenant_id, created_at);
CREATE TABLE IF NOT EXISTS dead_letters (
    event_id    TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    payload     TEXT NOT NULL,
    attempts    INTEGER NOT NULL,
    last_error  TEXT NOT NULL,
    died_at     INTEGER NOT NULL,
    replayed_at INTEGER
);
