CREATE TABLE scan_jobs (
    id TEXT PRIMARY KEY,
    requested_by TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    source_identity TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('queued','staging','scanning','needs_review','failed')),
    created_at INTEGER NOT NULL,
    attempt_id TEXT UNIQUE,
    sha256 TEXT,
    size_bytes INTEGER,
    report TEXT,
    error TEXT
);
CREATE TABLE probe_jobs_new (
    id TEXT PRIMARY KEY,
    requested_by TEXT NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    source_identity TEXT NOT NULL,
    title INTEGER NOT NULL CHECK(title BETWEEN 1 AND 99),
    angle INTEGER NOT NULL CHECK(angle BETWEEN 1 AND 9),
    state TEXT NOT NULL CHECK(state IN ('queued','staging','probing','needs_review','failed')),
    created_at INTEGER NOT NULL,
    attempt_id TEXT,
    report TEXT,
    error TEXT,
    scan_job_id TEXT REFERENCES scan_jobs(id)
);
INSERT INTO probe_jobs_new(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report,error)
SELECT id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report,error FROM probe_jobs;
DROP TABLE probe_jobs;
ALTER TABLE probe_jobs_new RENAME TO probe_jobs;
CREATE INDEX probe_queue ON probe_jobs(state,created_at);
CREATE UNIQUE INDEX probe_scan_selection ON probe_jobs(scan_job_id,title,angle)
WHERE scan_job_id IS NOT NULL AND state!='failed';
CREATE TABLE conversion_jobs (
    id TEXT PRIMARY KEY,
    probe_job_id TEXT NOT NULL REFERENCES probe_jobs(id),
    requested_by TEXT NOT NULL REFERENCES users(id),
    state TEXT NOT NULL CHECK(state IN ('queued','converting','needs_review','queued_publication','publishing','published','failed')),
    created_at INTEGER NOT NULL,
    attempt_id TEXT,
    video_treatment TEXT NOT NULL CHECK(video_treatment IN ('preserve','yadif')),
    phase TEXT,
    report TEXT,
    error TEXT,
    movie_sha256 TEXT,
    movie_id TEXT UNIQUE REFERENCES movies(id),
    publication TEXT
);
CREATE UNIQUE INDEX conversion_active_probe ON conversion_jobs(probe_job_id)
WHERE state != 'failed';
UPDATE app_metadata SET schema_version = 4 WHERE singleton = 1;
