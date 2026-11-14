ALTER TABLE scan_jobs ADD COLUMN simple_state TEXT
    CHECK(simple_state IN ('scan','inspection','conversion','review','advanced','manual'));
ALTER TABLE scan_jobs ADD COLUMN simple_message TEXT;
ALTER TABLE scan_jobs ADD COLUMN simple_requested_by TEXT REFERENCES users(id);
ALTER TABLE scan_jobs ADD COLUMN simple_probe_id TEXT REFERENCES probe_jobs(id);
ALTER TABLE scan_jobs ADD COLUMN simple_conversion_id TEXT REFERENCES conversion_jobs(id);
CREATE UNIQUE INDEX simple_source ON scan_jobs(name,source_identity) WHERE simple_state IS NOT NULL;
DROP VIEW import_lifecycle;
CREATE VIEW import_lifecycle AS
SELECT 'scan' AS kind,id,created_at,
    state='failed' OR (COALESCE(simple_state NOT IN ('scan','inspection','conversion','advanced'),1) AND
    (state='needs_review' AND EXISTS(
        SELECT 1 FROM probe_jobs p WHERE p.scan_job_id=s.id AND p.state!='failed'
    ))) AS can_archive,
    COALESCE(simple_state NOT IN ('scan','inspection','conversion','advanced'),1) AND
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
UPDATE app_metadata SET schema_version=13 WHERE singleton=1;
