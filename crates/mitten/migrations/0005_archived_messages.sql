-- Compaction replaces a conversation's live history with a summary plus its latest turns.
-- Replaced rows are archived instead of deleted, so the full transcript stays on disk.
ALTER TABLE messages ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1));
