-- Jobs get a short name to show in lists and run headers. Earlier jobs have none.
ALTER TABLE jobs ADD COLUMN name TEXT NOT NULL DEFAULT '';
