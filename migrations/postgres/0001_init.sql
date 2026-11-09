-- Baseline schema for Postgres. Same tables as the SQLite baseline.
CREATE TABLE tenants (
    id              TEXT PRIMARY KEY,
    api_key_hash    TEXT NOT NULL UNIQUE,
    secret          TEXT NOT NULL,
    prev_secret     TEXT,
    prev_expires_at BIGINT,
    created_at      BIGINT NOT NULL
);
CREATE TABLE events (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_hash    TEXT NOT NULL,
    payload         TEXT NOT NULL,
    trace_id        TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        BIGINT NOT NULL DEFAULT 0,
    next_attempt_at BIGINT NOT NULL,
    locked_until    BIGINT NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      BIGINT NOT NULL,
    updated_at      BIGINT NOT NULL,
    UNIQUE (tenant_id, idempotency_key)
);
CREATE INDEX events_pending_idx ON events (next_attempt_at) WHERE status = 'pending';
CREATE INDEX events_processing_idx ON events (locked_until) WHERE status = 'processing';
CREATE INDEX events_tenant_idx ON events (tenant_id, created_at);
CREATE TABLE dead_letters (
    event_id    TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    payload     TEXT NOT NULL,
    attempts    BIGINT NOT NULL,
    last_error  TEXT NOT NULL,
    died_at     BIGINT NOT NULL,
    replayed_at BIGINT
);
