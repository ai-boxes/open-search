CREATE TABLE application_metadata (
    key TEXT PRIMARY KEY NOT NULL CHECK (length(key) > 0),
    value TEXT NOT NULL CHECK (length(value) > 0),
    updated_at INTEGER NOT NULL
);
