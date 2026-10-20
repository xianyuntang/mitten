-- Long-term facts the agent saves with its memory tool; shared by every conversation.
CREATE TABLE memories (
    id         INTEGER PRIMARY KEY,
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
