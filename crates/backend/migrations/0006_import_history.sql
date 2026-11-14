CREATE TABLE import_preferences (
    user_id TEXT NOT NULL REFERENCES users(id),
    kind TEXT NOT NULL CHECK(kind IN ('scan','probe','conversion')),
    job_id TEXT NOT NULL,
    archived INTEGER NOT NULL CHECK(archived IN (0,1)),
    PRIMARY KEY(user_id,kind,job_id)
);
CREATE INDEX probe_scan_history ON probe_jobs(scan_job_id,state);
CREATE INDEX scan_history_order ON scan_jobs(created_at DESC,id);
CREATE INDEX probe_history_order ON probe_jobs(created_at DESC,id);
CREATE INDEX conversion_history_order ON conversion_jobs(created_at DESC,id);

-- A failed child returns its parent to attention so another selection/retry is possible.
CREATE VIEW import_lifecycle AS
SELECT 'scan' AS kind,id,created_at,
    state='failed' OR (state='needs_review' AND EXISTS(
        SELECT 1 FROM probe_jobs p WHERE p.scan_job_id=s.id AND p.state!='failed'
    )) AS can_archive,
    state='needs_review' AND EXISTS(
        SELECT 1 FROM probe_jobs p WHERE p.scan_job_id=s.id AND p.state!='failed'
    ) AS default_history
FROM scan_jobs s
UNION ALL
SELECT 'probe',id,created_at,
    state='failed' OR (state='needs_review' AND EXISTS(
        SELECT 1 FROM conversion_jobs c WHERE c.probe_job_id=p.id AND c.state!='failed'
    )),
    state='needs_review' AND EXISTS(
        SELECT 1 FROM conversion_jobs c WHERE c.probe_job_id=p.id AND c.state!='failed'
    )
FROM probe_jobs p
UNION ALL
SELECT 'conversion',id,created_at,state IN ('failed','published'),state='published'
FROM conversion_jobs;

UPDATE app_metadata SET schema_version=6 WHERE singleton=1;
