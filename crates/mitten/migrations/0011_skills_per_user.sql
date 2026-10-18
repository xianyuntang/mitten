-- Skills belong to one person, like memory: a Discord user ID, or NULL for the terminal. Existing
-- skills were saved before skills were told apart and stay with the terminal. A name is unique per
-- person; 0 stands in for the terminal's NULL, since Discord never hands out user ID 0.
CREATE TABLE skills_per_user (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
INSERT INTO skills_per_user (id, name, description, content, updated_at)
SELECT id, name, description, content, updated_at FROM skills;
DROP TABLE skills;
ALTER TABLE skills_per_user RENAME TO skills;
CREATE UNIQUE INDEX skills_by_user ON skills (ifnull(user_id, 0), name);
