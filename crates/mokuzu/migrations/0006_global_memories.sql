-- Memory is global again: facts about the user and this machine apply to every conversation.
-- The per-conversation copies collapse to one entry per distinct text, in first-saved order.
CREATE TABLE memories_global (
    id         INTEGER PRIMARY KEY,
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO memories_global (content, created_at)
SELECT content, min(created_at)
FROM memories
GROUP BY content
ORDER BY min(id);

DROP TABLE memories;

ALTER TABLE memories_global RENAME TO memories;
