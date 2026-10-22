//! SQLite storage: hand-written migrations tracked by `PRAGMA user_version`, hand-written entities.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, Row, params};
use serde_json::Value;

use crate::cron::NewJob;
use crate::memory::Edit;

/// Applied in order; `user_version` records how many have run. Append only, never edit.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_conversations.sql"),
    include_str!("../migrations/0002_rig_messages.sql"),
    include_str!("../migrations/0003_memories.sql"),
    include_str!("../migrations/0004_memories_per_conversation.sql"),
    include_str!("../migrations/0005_archived_messages.sql"),
    include_str!("../migrations/0006_global_memories.sql"),
    include_str!("../migrations/0007_jobs.sql"),
    include_str!("../migrations/0008_job_names.sql"),
    include_str!("../migrations/0009_memories_per_user.sql"),
    include_str!("../migrations/0010_skills.sql"),
    include_str!("../migrations/0011_skills_per_user.sql"),
    include_str!("../migrations/0012_message_search.sql"),
];

/// Row of `conversations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: i64,
    pub key: String,
}

impl Conversation {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            key: row.get("key")?,
        })
    }
}

/// Row of `messages`; `content` is the whole message as JSON, `role` is copied out for queries.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: Value,
}

impl Message {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let content: String = row.get("content")?;
        Ok(Self {
            id: row.get("id")?,
            conversation_id: row.get("conversation_id")?,
            role: row.get("role")?,
            content: serde_json::from_str(&content).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
        })
    }
}

/// Row of `memories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: i64,
    pub content: String,
}

impl Memory {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            content: row.get("content")?,
        })
    }
}

/// Row of `skills`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// One line, listed in the system prompt.
    pub description: String,
    pub content: String,
}

impl Skill {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            description: row.get("description")?,
            content: row.get("content")?,
        })
    }
}

/// Row of `jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    /// Empty for jobs made before names existed.
    pub name: String,
    /// Conversation key the job was made in, where its runs post.
    pub target: String,
    /// Cron expression; `None` runs once.
    pub schedule: Option<String>,
    /// Discord user who made it, whose memory its runs load; `None` for the terminal.
    pub user: Option<u64>,
    pub prompt: String,
    /// Unix seconds.
    pub next_run: i64,
}

impl Job {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            target: row.get("target")?,
            schedule: row.get("schedule")?,
            user: row
                .get::<_, Option<i64>>("user_id")?
                .map(i64::cast_unsigned),
            prompt: row.get("prompt")?,
            next_run: row.get("next_run")?,
        })
    }
}

/// A Discord user ID as SQLite's signed integer; the cast keeps every bit, so it reads back exactly.
fn sql_user(user: Option<u64>) -> Option<i64> {
    user.map(u64::cast_signed)
}

/// Shared handle; every query runs on Tokio's blocking pool.
#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Opens (or creates) the database file and applies pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|_| anyhow!("database lock poisoned"))?;
            f(&mut conn)
        })
        .await
        .context("database task panicked")?
    }

    /// Returns the conversation for `key`, creating it on first use.
    pub async fn conversation(&self, key: &str) -> Result<Conversation> {
        let key = key.to_owned();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO conversations (key) VALUES (?1) ON CONFLICT (key) DO NOTHING",
                params![key],
            )?;
            Ok(conn.query_row(
                "SELECT id, key FROM conversations WHERE key = ?1",
                params![key],
                Conversation::from_row,
            )?)
        })
        .await
    }

    /// Whether a conversation for `key` was ever started.
    pub async fn has_conversation(&self, key: &str) -> Result<bool> {
        let key = key.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM conversations WHERE key = ?1)",
                params![key],
                |row| row.get(0),
            )?)
        })
        .await
    }

    pub async fn messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, conversation_id, role, content FROM messages
                 WHERE conversation_id = ?1 AND archived = 0 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![conversation_id], Message::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Appends messages (JSON objects with a `role`) in one transaction.
    pub async fn append(&self, conversation_id: i64, messages: Vec<Value>) -> Result<()> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            insert_messages(&tx, conversation_id, &messages)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Replaces the conversation's live history with `messages`, archiving the old rows.
    pub async fn rewrite(&self, conversation_id: i64, messages: Vec<Value>) -> Result<()> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE messages SET archived = 1 WHERE conversation_id = ?1",
                params![conversation_id],
            )?;
            insert_messages(&tx, conversation_id, &messages)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Archives every message in the conversation, so it starts empty but stays searchable.
    pub async fn clear(&self, conversation_id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE messages SET archived = 1 WHERE conversation_id = ?1",
                params![conversation_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Archived messages of the conversation whose text contains every one of `terms` (ignoring
    /// ASCII case), newest first: (sent at, role, text).
    pub async fn search(
        &self,
        conversation_id: i64,
        terms: Vec<String>,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        self.with_conn(move |conn| {
            let mut sql = "SELECT messages.created_at, messages.role, message_text.text
                 FROM message_text JOIN messages ON messages.id = message_text.rowid
                 WHERE messages.conversation_id = ? AND messages.archived = 1"
                .to_owned();
            // The trigram index serves LIKE for terms of three or more characters; shorter
            // ones scan the conversation's rows.
            let mut values = vec![rusqlite::types::Value::from(conversation_id)];
            for term in terms {
                sql.push_str(" AND message_text.text LIKE ?");
                let term = term.replace(['%', '_'], "");
                values.push(format!("%{term}%").into());
            }
            sql.push_str(" ORDER BY messages.id DESC LIMIT ?");
            values.push(i64::try_from(limit)?.into());
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}

impl Db {
    /// `user`'s entries: a Discord user ID, or `None` for the terminal.
    pub async fn memories(&self, user: Option<u64>) -> Result<Vec<Memory>> {
        self.with_conn(move |conn| {
            // `IS` matches NULL too, so the terminal's entries come back for `None`.
            let mut stmt =
                conn.prepare("SELECT id, content FROM memories WHERE user_id IS ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![sql_user(user)], Memory::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    // ponytail: edits are planned against a read taken just before; two processes editing
    // memory at the same instant could overshoot the budget slightly.
    /// Applies `edit` to `user`'s entries; ids of someone else's entries match nothing.
    pub async fn save_memory(&self, user: Option<u64>, edit: Edit) -> Result<()> {
        self.with_conn(move |conn| {
            match edit {
                Edit::Add(content) => conn.execute(
                    "INSERT INTO memories (content, user_id) VALUES (?1, ?2)",
                    params![content, sql_user(user)],
                )?,
                Edit::Replace(id, content) => conn.execute(
                    "UPDATE memories SET content = ?2 WHERE id = ?1 AND user_id IS ?3",
                    params![id, content, sql_user(user)],
                )?,
                Edit::Remove(id) => conn.execute(
                    "DELETE FROM memories WHERE id = ?1 AND user_id IS ?2",
                    params![id, sql_user(user)],
                )?,
            };
            Ok(())
        })
        .await
    }

    /// `user`'s skills by name: a Discord user ID, or `None` for the terminal.
    pub async fn skills(&self, user: Option<u64>) -> Result<Vec<Skill>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT name, description, content FROM skills WHERE user_id IS ?1 ORDER BY name",
            )?;
            let rows = stmt.query_map(params![sql_user(user)], Skill::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Creates `skill` for `user`, or replaces their one with its name.
    pub async fn save_skill(&self, user: Option<u64>, skill: Skill) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO skills (user_id, name, description, content) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (ifnull(user_id, 0), name) DO UPDATE SET description = excluded.description,
                     content = excluded.content,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                params![sql_user(user), skill.name, skill.description, skill.content],
            )?;
            Ok(())
        })
        .await
    }

    /// Deletes `user`'s skill named `name`; false if they had none.
    pub async fn remove_skill(&self, user: Option<u64>, name: String) -> Result<bool> {
        self.with_conn(move |conn| {
            let sql = "DELETE FROM skills WHERE name = ?1 AND user_id IS ?2";
            Ok(conn.execute(sql, params![name, sql_user(user)])? > 0)
        })
        .await
    }

    /// Every scheduled job, soonest first.
    pub async fn jobs(&self) -> Result<Vec<Job>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, target, schedule, prompt, next_run, user_id FROM jobs
                 ORDER BY next_run, id",
            )?;
            let rows = stmt.query_map([], Job::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Stores a job that posts to the conversation `target` and returns its id.
    pub async fn add_job(&self, target: String, user: Option<u64>, job: NewJob) -> Result<i64> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO jobs (name, target, schedule, prompt, next_run, user_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    job.name,
                    target,
                    job.schedule,
                    job.prompt,
                    job.next_run,
                    sql_user(user)
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn set_next_run(&self, id: i64, next_run: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE jobs SET next_run = ?2 WHERE id = ?1",
                params![id, next_run],
            )?;
            Ok(())
        })
        .await
    }

    /// Saves `job`'s prompt and timing; false if there was none with its id.
    pub async fn update_job(&self, job: &Job) -> Result<bool> {
        let job = job.clone();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE jobs SET prompt = ?2, schedule = ?3, next_run = ?4 WHERE id = ?1",
                params![job.id, job.prompt, job.schedule, job.next_run],
            )? > 0)
        })
        .await
    }

    /// Deletes a job; false if there was none with that id.
    pub async fn remove_job(&self, id: i64) -> Result<bool> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM jobs WHERE id = ?1", params![id])? > 0)
        })
        .await
    }
}

fn insert_messages(conn: &Connection, conversation_id: i64, messages: &[Value]) -> Result<()> {
    let mut stmt =
        conn.prepare("INSERT INTO messages (conversation_id, role, content) VALUES (?1, ?2, ?3)")?;
    for message in messages {
        let Some(role) = message["role"].as_str() else {
            bail!("message without a role: {message}");
        };
        stmt.execute(params![conversation_id, role, message.to_string()])?;
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let applied = usize::try_from(applied).context("negative user_version")?;
    if applied > MIGRATIONS.len() {
        bail!(
            "database is at migration {applied}, newer than this binary ({}); upgrade mokuzu",
            MIGRATIONS.len()
        );
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let version = index + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("migration {version} failed"))?;
        tx.pragma_update(None, "user_version", i64::try_from(version)?)?;
        tx.commit()?;
        tracing::info!(version, "applied migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn migrations_are_idempotent_and_messages_round_trip() {
        let db = Db::open_in_memory().expect("open");
        {
            let mut conn = db.conn.lock().expect("lock");
            migrate(&mut conn).expect("second migrate is a no-op");
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("version");
            assert_eq!(
                usize::try_from(version).expect("positive"),
                MIGRATIONS.len()
            );
        }

        let conversation = db.conversation("t").await.expect("create");
        assert_eq!(db.conversation("t").await.expect("reuse"), conversation);

        let turn = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "hello"}]}),
        ];
        db.append(conversation.id, turn.clone())
            .await
            .expect("append");
        let stored: Vec<Value> = db
            .messages(conversation.id)
            .await
            .expect("load")
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(stored, turn);

        db.clear(conversation.id).await.expect("clear");
        assert!(db.messages(conversation.id).await.expect("load").is_empty());
    }

    #[tokio::test]
    async fn search_finds_archived_text_only() {
        let db = Db::open_in_memory().expect("open");
        let conversation = db.conversation("terminal").await.expect("conversation");
        let other = db.conversation("discord:1").await.expect("conversation");
        let text = |role: &str, text: &str| json!({"role": role, "content": [{"type": "text", "text": text}]});
        let tool = json!({"role": "user", "content": [{"type": "toolresult", "content": [{"type": "text", "text": "貓咪 tool output"}]}]});
        db.append(
            conversation.id,
            vec![
                text("user", "我家的貓咪叫 Mochi"),
                tool,
                text("assistant", "記住了"),
            ],
        )
        .await
        .expect("append");
        db.append(other.id, vec![text("user", "別人的貓咪")])
            .await
            .expect("append");
        let search = |terms: &[&str]| {
            db.search(
                conversation.id,
                terms.iter().map(|t| (*t).to_owned()).collect(),
                10,
            )
        };
        assert!(
            search(&["貓咪"]).await.expect("search").is_empty(),
            "live history is already in context"
        );

        db.clear(conversation.id).await.expect("clear");
        let hits = search(&["貓咪", "mochi"]).await.expect("search");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            (hits[0].1.as_str(), hits[0].2.as_str()),
            ("user", "我家的貓咪叫 Mochi")
        );
        assert!(search(&["貓咪", "Luna"]).await.expect("search").is_empty());
        assert_eq!(
            search(&["記"]).await.expect("search").len(),
            1,
            "short terms scan"
        );
    }

    #[tokio::test]
    async fn memories_round_trip_per_user() {
        let db = Db::open_in_memory().expect("open");
        let contents = |user| {
            let db = db.clone();
            async move {
                db.memories(user)
                    .await
                    .expect("load")
                    .into_iter()
                    .map(|m| (m.id, m.content))
                    .collect::<Vec<_>>()
            }
        };
        for (user, text) in [(None, "a"), (None, "b"), (Some(7), "mine")] {
            db.save_memory(user, Edit::Add(text.to_owned()))
                .await
                .expect("add");
        }
        let terminal = contents(None).await;
        let mine = contents(Some(7)).await;
        assert_eq!(mine.len(), 1);
        assert!(contents(Some(8)).await.is_empty());

        db.save_memory(None, Edit::Replace(terminal[0].0, "a2".to_owned()))
            .await
            .expect("replace");
        db.save_memory(None, Edit::Remove(terminal[1].0))
            .await
            .expect("remove");
        // Someone else's entry id changes nothing.
        db.save_memory(Some(8), Edit::Remove(mine[0].0))
            .await
            .expect("remove");
        let texts =
            |entries: Vec<(i64, String)>| entries.into_iter().map(|e| e.1).collect::<Vec<_>>();
        assert_eq!(texts(contents(None).await), ["a2"]);
        assert_eq!(texts(contents(Some(7)).await), ["mine"]);
    }

    #[tokio::test]
    async fn skills_round_trip_per_user() {
        let db = Db::open_in_memory().expect("open");
        let skill = |name: &str, content: &str| Skill {
            name: name.to_owned(),
            description: "d".to_owned(),
            content: content.to_owned(),
        };
        db.save_skill(None, skill("b", "one")).await.expect("save");
        db.save_skill(None, skill("a", "x")).await.expect("save");
        db.save_skill(None, skill("b", "two"))
            .await
            .expect("replace");
        db.save_skill(Some(7), skill("b", "mine"))
            .await
            .expect("save");
        db.save_skill(Some(7), skill("b", "mine2"))
            .await
            .expect("replace");
        let skills = db.skills(None).await.expect("load");
        assert_eq!(skills, [skill("a", "x"), skill("b", "two")]);
        assert_eq!(
            db.skills(Some(7)).await.expect("load"),
            [skill("b", "mine2")]
        );
        assert!(
            !db.remove_skill(Some(7), "a".to_owned())
                .await
                .expect("not theirs")
        );
        assert!(db.remove_skill(None, "a".to_owned()).await.expect("remove"));
        assert!(
            !db.remove_skill(None, "a".to_owned())
                .await
                .expect("remove again")
        );
        assert_eq!(db.skills(None).await.expect("load").len(), 1);
        assert_eq!(db.skills(Some(7)).await.expect("load").len(), 1);
    }

    #[tokio::test]
    async fn jobs_round_trip() {
        let job = |name: &str, schedule: Option<&str>, next_run| NewJob {
            name: name.to_owned(),
            schedule: schedule.map(str::to_owned),
            prompt: "do it".to_owned(),
            next_run,
        };
        let db = Db::open_in_memory().expect("open");
        let later = db
            .add_job(
                "discord:1".to_owned(),
                Some(7),
                job("a", Some("0 9 * * *"), 20),
            )
            .await
            .expect("add");
        let sooner = db
            .add_job("discord:1".to_owned(), None, job("b", None, 10))
            .await
            .expect("add");
        let ids = |jobs: Vec<Job>| jobs.into_iter().map(|j| j.id).collect::<Vec<_>>();
        let jobs = db.jobs().await.expect("load");
        assert_eq!(jobs[0].name, "b");
        assert_eq!((jobs[0].user, jobs[1].user), (None, Some(7)));
        assert_eq!(ids(jobs), [sooner, later]);
        db.set_next_run(later, 5).await.expect("update");
        assert_eq!(ids(db.jobs().await.expect("load")), [later, sooner]);
        let mut revised = db.jobs().await.expect("load")[1].clone();
        revised.prompt = "do it better".to_owned();
        assert!(db.update_job(&revised).await.expect("update"));
        assert_eq!(db.jobs().await.expect("load")[1], revised);
        assert!(db.remove_job(sooner).await.expect("remove"));
        assert!(!db.remove_job(sooner).await.expect("remove again"));
        assert_eq!(ids(db.jobs().await.expect("load")), [later]);
    }

    #[test]
    fn memories_go_per_conversation_and_back_to_global() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        let tx = conn.transaction().expect("tx");
        for sql in &MIGRATIONS[..3] {
            tx.execute_batch(sql).expect("old migrations");
        }
        tx.execute_batch(
            "INSERT INTO conversations (key) VALUES ('terminal'), ('discord:1');
             INSERT INTO memories (content) VALUES ('fact');",
        )
        .expect("seed");
        tx.execute_batch(MIGRATIONS[3]).expect("migration 4");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM memories WHERE content = 'fact'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 2);

        tx.execute_batch(
            "INSERT INTO memories (conversation_id, content) VALUES (2, 'only in discord');",
        )
        .expect("seed");
        for sql in &MIGRATIONS[4..6] {
            tx.execute_batch(sql).expect("later migrations");
        }
        let mut stmt = tx
            .prepare("SELECT content FROM memories ORDER BY id")
            .expect("prepare");
        let merged: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert_eq!(merged, ["fact", "only in discord"]);
    }

    #[tokio::test]
    async fn rewrite_archives_old_rows() {
        let db = Db::open_in_memory().expect("open");
        let id = db.conversation("t").await.expect("create").id;
        let old = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        db.append(id, old).await.expect("append");
        let summary = vec![json!({"role": "user", "content": "summary"})];
        db.rewrite(id, summary.clone()).await.expect("rewrite");
        let live: Vec<Value> = db
            .messages(id)
            .await
            .expect("load")
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(live, summary);
        let conn = db.conn.lock().expect("lock");
        let archived: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE archived = 1",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(archived, 2);
    }

    #[tokio::test]
    async fn append_rejects_bad_role_atomically() {
        let db = Db::open_in_memory().expect("open");
        let id = db.conversation("t").await.expect("create").id;
        let turn = vec![
            json!({"role": "user", "content": "ok"}),
            json!({"role": "system", "content": "not allowed"}),
        ];
        assert!(db.append(id, turn).await.is_err());
        assert!(db.messages(id).await.expect("load").is_empty());
    }
}
