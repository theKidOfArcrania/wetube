CREATE TABLE labels (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN ('category','tag')),
    name TEXT NOT NULL,
    canonical TEXT NOT NULL,
    UNIQUE(kind,canonical)
);
CREATE TABLE movie_labels (
    movie_id TEXT NOT NULL REFERENCES movies(id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY(movie_id,label_id)
);
CREATE INDEX movie_labels_by_label ON movie_labels(label_id,movie_id);
UPDATE app_metadata SET schema_version = 10 WHERE singleton = 1;
