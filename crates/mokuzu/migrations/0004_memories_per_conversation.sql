-- Memory now belongs to one conversation (the terminal, or one Discord channel) instead of
-- being shared. Earlier entries were global, so every existing conversation gets a copy.
ALTER TABLE memories RENAME TO memories_old;

CREATE TABLE memories (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    content         TEXT    NOT NULL,
    created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX memories_by_conversation ON memories (conversation_id, id);

INSERT INTO memories (conversation_id, content, created_at)
SELECT c.id, m.content, m.created_at
FROM conversations c CROSS JOIN memories_old m
ORDER BY c.id, m.id;

DROP TABLE memories_old;
