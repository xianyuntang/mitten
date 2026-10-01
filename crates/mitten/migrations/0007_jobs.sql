-- Scheduled jobs: a prompt that runs on its own in a fresh conversation and posts its result to
-- `target`, the conversation it was made in. `schedule` is a cron expression in local time; NULL
-- means the job runs once and is deleted. `next_run` is in Unix seconds.
CREATE TABLE jobs (
    id         INTEGER PRIMARY KEY,
    target     TEXT    NOT NULL,
    schedule   TEXT,
    prompt     TEXT    NOT NULL,
    next_run   INTEGER NOT NULL,
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
