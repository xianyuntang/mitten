-- Skills: reusable how-to instructions for recurring tasks, shared by everyone. The system prompt
-- lists each name and description; the content loads only when the model asks for it.
CREATE TABLE skills (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
