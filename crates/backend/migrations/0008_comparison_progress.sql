ALTER TABLE comparison_jobs ADD COLUMN progress TEXT;
UPDATE app_metadata SET schema_version=8 WHERE singleton=1;
