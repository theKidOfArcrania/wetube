CREATE TABLE movie_trash (
    movie_id TEXT PRIMARY KEY REFERENCES movies(id) ON DELETE CASCADE,
    deleted_at INTEGER NOT NULL,
    purge_after INTEGER NOT NULL,
    purge_started_at INTEGER,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    provenance_complete INTEGER NOT NULL DEFAULT 0 CHECK(provenance_complete IN (0,1)),
    provenance_manifest TEXT,
    retention_note TEXT
);
-- Retain login/movie scope when inactive activity leases are pruned.
CREATE TABLE playback_lease_bindings (
    id TEXT PRIMARY KEY,
    movie_id TEXT NOT NULL REFERENCES movies(id) ON DELETE CASCADE,
    session_hash TEXT NOT NULL REFERENCES sessions(token_hash) ON DELETE CASCADE
);
CREATE TABLE playback_leases (
    id TEXT PRIMARY KEY REFERENCES playback_lease_bindings(id) ON DELETE CASCADE,
    movie_id TEXT NOT NULL REFERENCES movies(id) ON DELETE CASCADE,
    session_hash TEXT NOT NULL REFERENCES sessions(token_hash) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX playback_lease_movie ON playback_leases(movie_id,expires_at);
-- Durable, fixed-name provenance. No source ISO, log, report or directory is a deletion target.
CREATE TABLE movie_outputs (
    movie_id TEXT NOT NULL REFERENCES movies(id) ON DELETE CASCADE,
    root TEXT NOT NULL CHECK(root IN ('media','scratch')),
    origin TEXT NOT NULL DEFAULT 'manual' CHECK(origin IN ('manual','worker')),
    root_path BLOB NOT NULL,
    root_identity TEXT NOT NULL,
    directory TEXT NOT NULL,
    directory_identity TEXT NOT NULL,
    name TEXT NOT NULL CHECK(name IN ('movie.mp4','original-tracks.mkv')),
    identity TEXT NOT NULL,
    stable_identity TEXT NOT NULL,
    sha256 TEXT,
    removed INTEGER NOT NULL DEFAULT 0 CHECK(removed IN (0,1)),
    PRIMARY KEY(movie_id,root,directory,name)
);
CREATE TABLE movie_purge_history (
    movie_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    deleted_at INTEGER NOT NULL,
    purged_at INTEGER NOT NULL,
    retention_note TEXT
);
UPDATE app_metadata SET schema_version=12 WHERE singleton=1;
