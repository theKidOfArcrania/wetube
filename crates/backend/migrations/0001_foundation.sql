CREATE TABLE app_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    app_version TEXT NOT NULL
);
INSERT INTO app_metadata VALUES (1, 1, '0.1.0');

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'viewer'))
);

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expiry ON sessions(expires_at);

CREATE TABLE movies (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    year INTEGER,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK (status IN ('ready', 'needs_reimport', 'trashed'))
);

CREATE TABLE diagnostic_jobs (
    id TEXT PRIMARY KEY,
    requested_by TEXT NOT NULL REFERENCES users(id),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed')),
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    report TEXT,
    error TEXT
);
CREATE INDEX diagnostic_queue ON diagnostic_jobs(state, created_at);

CREATE TABLE worker_status (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_seen INTEGER NOT NULL
);
