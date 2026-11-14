CREATE TABLE tmdb_posters (
    tmdb_id INTEGER PRIMARY KEY CHECK(tmdb_id > 0),
    image BLOB NOT NULL CHECK(length(image) BETWEEN 1 AND 1048576),
    mime TEXT NOT NULL CHECK(mime IN ('image/jpeg','image/png')),
    fetched_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
UPDATE app_metadata SET schema_version = 11 WHERE singleton = 1;
