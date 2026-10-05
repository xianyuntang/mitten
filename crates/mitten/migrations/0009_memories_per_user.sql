-- Memory belongs to one person: a Discord user ID, or NULL for the terminal. Existing entries
-- were saved before users were told apart and stay with the terminal.
ALTER TABLE memories ADD COLUMN user_id INTEGER;
-- Scheduled jobs remember who made them, so a run loads that person's memory. NULL: the terminal.
ALTER TABLE jobs ADD COLUMN user_id INTEGER;
