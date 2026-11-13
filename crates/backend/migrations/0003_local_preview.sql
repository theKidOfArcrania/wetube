-- Local operator-admitted previews only, not a production prepared-asset registry.
CREATE TABLE preview_media (
    movie_id TEXT PRIMARY KEY REFERENCES movies(id) ON DELETE CASCADE,
    package_id TEXT NOT NULL UNIQUE,
    sha256 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    duration_seconds REAL NOT NULL CHECK (duration_seconds > 0),
    file_identity TEXT NOT NULL,
    admitted_at INTEGER NOT NULL
);
CREATE TABLE playback_progress (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    movie_id TEXT NOT NULL REFERENCES movies(id) ON DELETE CASCADE,
    position_seconds REAL NOT NULL CHECK (position_seconds >= 0),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, movie_id)
);
UPDATE app_metadata SET schema_version = 3 WHERE singleton = 1;
