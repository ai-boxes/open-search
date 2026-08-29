CREATE TABLE grok_credentials (
    account_id TEXT PRIMARY KEY NOT NULL CHECK (account_id = 'default'),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    format_version INTEGER NOT NULL DEFAULT 1 CHECK (format_version = 1),
    credential_json TEXT NOT NULL CHECK (
        length(credential_json) > 3
        AND json_valid(credential_json)
    ),
    expires_at INTEGER,
    last_refreshed_at INTEGER,
    updated_at INTEGER NOT NULL
);
