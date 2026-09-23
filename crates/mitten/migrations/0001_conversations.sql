-- One conversation per chat surface, e.g. "terminal" or "discord:<channel id>".
CREATE TABLE conversations (
    id         INTEGER PRIMARY KEY,
    key        TEXT    NOT NULL UNIQUE,
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- Messages exactly as sent to the Messages API; `content` is JSON (string or block array).
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    role            TEXT    NOT NULL CHECK (role IN ('user', 'assistant')),
    content         TEXT    NOT NULL CHECK (json_valid(content)),
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX messages_by_conversation ON messages (conversation_id, id);
