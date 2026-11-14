CREATE TABLE tmdb_metadata (
    movie_id TEXT PRIMARY KEY REFERENCES movies(id),
    tmdb_id INTEGER NOT NULL CHECK(tmdb_id > 0),
    title TEXT,
    year INTEGER,
    description TEXT,
    fetched_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
UPDATE app_metadata SET schema_version = 9 WHERE singleton = 1;
