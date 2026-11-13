CREATE TABLE probe_jobs (
    id TEXT PRIMARY KEY,
    requested_by TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    source_identity TEXT NOT NULL,
    title INTEGER NOT NULL CHECK (title BETWEEN 1 AND 99),
    angle INTEGER NOT NULL CHECK (angle BETWEEN 1 AND 9),
    state TEXT NOT NULL CHECK (state IN ('queued', 'staging', 'probing', 'needs_review', 'failed')),
    created_at INTEGER NOT NULL,
    attempt_id TEXT UNIQUE,
    report TEXT,
    error TEXT
);
CREATE INDEX probe_queue ON probe_jobs(state, created_at);
UPDATE app_metadata SET schema_version = 2 WHERE singleton = 1;
