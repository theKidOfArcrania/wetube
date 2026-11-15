ALTER TABLE diagnostic_jobs ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1));
ALTER TABLE scan_jobs ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1));
ALTER TABLE probe_jobs ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1));
ALTER TABLE comparison_jobs ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1));
ALTER TABLE conversion_jobs ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1));
CREATE TABLE processing_settings (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL DEFAULT 0,
    paused INTEGER NOT NULL DEFAULT 0 CHECK(paused IN (0,1)),
    maximum INTEGER NOT NULL DEFAULT 1 CHECK(maximum BETWEEN 1 AND 16)
);
INSERT INTO processing_settings(singleton) VALUES(1);
CREATE TABLE processing_queue (
    key INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK(kind IN ('diagnostic','scan','probe','comparison','conversion','publication')),
    job_id TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    finish_state TEXT CHECK(finish_state IN ('failed','needs_review','succeeded')),
    state TEXT NOT NULL DEFAULT 'waiting' CHECK(state IN ('waiting','running','cancelling','cancelled','completed')),
    reason TEXT NOT NULL DEFAULT 'Waiting for worker'
);
CREATE UNIQUE INDEX processing_live_job ON processing_queue(kind,job_id)
    WHERE state IN ('waiting','running','cancelling');
CREATE TRIGGER processing_insert AFTER INSERT ON processing_queue BEGIN
    UPDATE processing_queue SET position=NEW.key WHERE key=NEW.key;
    UPDATE processing_settings SET revision=revision+1;
END;
CREATE TRIGGER processing_change AFTER UPDATE OF position,state,reason ON processing_queue BEGIN
    UPDATE processing_settings SET revision=revision+1;
END;
CREATE VIEW processing_details AS
SELECT q.*, COALESCE(s.name,p.name,cp.name,vp.name,'Tool diagnostics') AS name,
    COALESCE(s.name,p.name,cp.name,vp.name) AS source,
    COALESCE(s.attempt_id,p.attempt_id,cp.attempt_id,v.attempt_id) AS resource
FROM processing_queue q
LEFT JOIN scan_jobs s ON q.kind='scan' AND s.id=q.job_id
LEFT JOIN probe_jobs p ON q.kind='probe' AND p.id=q.job_id
LEFT JOIN comparison_jobs c ON q.kind='comparison' AND c.id=q.job_id
LEFT JOIN probe_jobs cp ON cp.id=c.probe_job_id
LEFT JOIN conversion_jobs v ON q.kind IN ('conversion','publication') AND v.id=q.job_id
LEFT JOIN probe_jobs vp ON vp.id=v.probe_job_id;
INSERT INTO processing_queue(kind,job_id,state,reason)
SELECT kind,id,CASE WHEN state IN ('queued','queued_publication') THEN 'waiting' ELSE 'running' END,
    CASE WHEN state IN ('queued','queued_publication') THEN 'Waiting for worker' ELSE 'Worker recovery required' END
FROM (
    SELECT 'diagnostic' AS kind,id,state,created_at FROM diagnostic_jobs WHERE state IN ('queued','running')
    UNION ALL SELECT 'scan',id,state,created_at FROM scan_jobs WHERE state IN ('queued','staging','scanning')
    UNION ALL SELECT 'probe',id,state,created_at FROM probe_jobs WHERE state IN ('queued','staging','probing')
    UNION ALL SELECT 'comparison',id,state,created_at FROM comparison_jobs WHERE state IN ('queued','running')
    UNION ALL SELECT CASE WHEN state IN ('queued_publication','publishing') THEN 'publication' ELSE 'conversion' END,
        id,state,created_at FROM conversion_jobs WHERE state IN ('queued','converting','queued_publication','publishing')
) ORDER BY created_at,kind,id;
CREATE TRIGGER diagnostic_enqueue AFTER INSERT ON diagnostic_jobs WHEN NEW.state='queued' BEGIN
    INSERT INTO processing_queue(kind,job_id) VALUES('diagnostic',NEW.id);
END;
CREATE TRIGGER scan_enqueue AFTER INSERT ON scan_jobs WHEN NEW.state='queued' BEGIN
    INSERT INTO processing_queue(kind,job_id) VALUES('scan',NEW.id);
END;
CREATE TRIGGER probe_enqueue AFTER INSERT ON probe_jobs WHEN NEW.state='queued' BEGIN
    INSERT INTO processing_queue(kind,job_id) VALUES('probe',NEW.id);
END;
CREATE TRIGGER comparison_enqueue AFTER INSERT ON comparison_jobs WHEN NEW.state='queued' BEGIN
    INSERT INTO processing_queue(kind,job_id) VALUES('comparison',NEW.id);
END;
CREATE TRIGGER conversion_enqueue AFTER INSERT ON conversion_jobs WHEN NEW.state='queued' BEGIN
    INSERT INTO processing_queue(kind,job_id) VALUES('conversion',NEW.id);
END;
CREATE TRIGGER publication_enqueue AFTER UPDATE OF state ON conversion_jobs
WHEN NEW.state='queued_publication' AND OLD.state!='queued_publication' BEGIN
    INSERT INTO processing_queue(kind,job_id) VALUES('publication',NEW.id);
END;
CREATE TRIGGER diagnostic_cancel_wins BEFORE UPDATE OF state ON diagnostic_jobs
WHEN NEW.state='succeeded' AND EXISTS(SELECT 1 FROM processing_queue WHERE kind='diagnostic' AND job_id=NEW.id AND state IN ('cancelling','cancelled'))
BEGIN SELECT RAISE(IGNORE); END;
CREATE TRIGGER scan_cancel_wins BEFORE UPDATE OF state ON scan_jobs
WHEN NEW.state='needs_review' AND EXISTS(SELECT 1 FROM processing_queue WHERE kind='scan' AND job_id=NEW.id AND state IN ('cancelling','cancelled'))
BEGIN SELECT RAISE(IGNORE); END;
CREATE TRIGGER probe_cancel_wins BEFORE UPDATE OF state ON probe_jobs
WHEN NEW.state='needs_review' AND EXISTS(SELECT 1 FROM processing_queue WHERE kind='probe' AND job_id=NEW.id AND state IN ('cancelling','cancelled'))
BEGIN SELECT RAISE(IGNORE); END;
CREATE TRIGGER comparison_cancel_wins BEFORE UPDATE OF state ON comparison_jobs
WHEN NEW.state='succeeded' AND EXISTS(SELECT 1 FROM processing_queue WHERE kind='comparison' AND job_id=NEW.id AND state IN ('cancelling','cancelled'))
BEGIN SELECT RAISE(IGNORE); END;
CREATE TRIGGER conversion_cancel_wins BEFORE UPDATE OF state ON conversion_jobs
WHEN NEW.state IN ('needs_review','published') AND EXISTS(SELECT 1 FROM processing_queue WHERE kind IN ('conversion','publication') AND job_id=NEW.id AND state IN ('cancelling','cancelled'))
BEGIN SELECT RAISE(IGNORE); END;
UPDATE app_metadata SET schema_version=14 WHERE singleton=1;
