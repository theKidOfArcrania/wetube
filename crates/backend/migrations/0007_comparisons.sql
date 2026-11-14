CREATE TABLE comparison_jobs (
    id TEXT PRIMARY KEY,
    probe_job_id TEXT NOT NULL REFERENCES probe_jobs(id),
    requested_by TEXT NOT NULL REFERENCES users(id),
    state TEXT NOT NULL CHECK(state IN ('queued','running','succeeded','failed')),
    created_at INTEGER NOT NULL,
    probe_report TEXT NOT NULL,
    title INTEGER NOT NULL,
    angle INTEGER NOT NULL,
    attempt_id TEXT,
    report TEXT,
    error TEXT
);
CREATE UNIQUE INDEX comparison_active ON comparison_jobs(probe_job_id)
    WHERE state IN ('queued','running');
UPDATE app_metadata SET schema_version=7 WHERE singleton=1;
