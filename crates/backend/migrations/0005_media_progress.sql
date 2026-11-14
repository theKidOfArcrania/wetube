ALTER TABLE scan_jobs ADD COLUMN progress TEXT;
ALTER TABLE probe_jobs ADD COLUMN progress TEXT;
ALTER TABLE conversion_jobs ADD COLUMN progress TEXT;
UPDATE app_metadata SET schema_version = 5 WHERE singleton = 1;
