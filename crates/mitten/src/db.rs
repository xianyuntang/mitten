//! SQLite storage: hand-written migrations tracked by `PRAGMA user_version`, hand-written entities.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, Row, params};
use serde_json::Value;

use crate::memory::Edit;

/// Applied in order; `user_version` records how many have run. Append only, never edit.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_conversations.sql"),
    include_str!("../migrations/0002_rig_messages.sql"),
    include_str!("../migrations/0003_memories.sql"),
    include_str!("../migrations/0004_memories_per_conversation.sql"),
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

    pub async fn messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, conversation_id, role, content FROM messages
                 WHERE conversation_id = ?1 ORDER BY id",
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
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO messages (conversation_id, role, content) VALUES (?1, ?2, ?3)",
                )?;
                for message in &messages {
                    let Some(role) = message["role"].as_str() else {
                        bail!("message without a role: {message}");
                    };
                    stmt.execute(params![conversation_id, role, message.to_string()])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Deletes every message in the conversation, keeping the conversation row.
    pub async fn clear(&self, conversation_id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM messages WHERE conversation_id = ?1",
                params![conversation_id],
            )?;
            Ok(())
        })
        .await
    }
}

impl Db {
    pub async fn memories(&self, conversation_id: i64) -> Result<Vec<Memory>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, content FROM memories WHERE conversation_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![conversation_id], Memory::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    // ponytail: edits are planned against a read taken just before; two processes editing
    // memory at the same instant could overshoot the budget slightly.
    pub async fn save_memory(&self, conversation_id: i64, edit: Edit) -> Result<()> {
        self.with_conn(move |conn| {
            match edit {
                Edit::Add(content) => conn.execute(
                    "INSERT INTO memories (conversation_id, content) VALUES (?1, ?2)",
                    params![conversation_id, content],
                )?,
                Edit::Replace(id, content) => conn.execute(
                    "UPDATE memories SET content = ?3 WHERE id = ?1 AND conversation_id = ?2",
                    params![id, conversation_id, content],
                )?,
                Edit::Remove(id) => conn.execute(
                    "DELETE FROM memories WHERE id = ?1 AND conversation_id = ?2",
                    params![id, conversation_id],
                )?,
            };
            Ok(())
        })
        .await
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let applied = usize::try_from(applied).context("negative user_version")?;
    if applied > MIGRATIONS.len() {
        bail!(
            "database is at migration {applied}, newer than this binary ({}); upgrade mitten",
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
    async fn memories_round_trip_per_conversation() {
        let db = Db::open_in_memory().expect("open");
        let here = db.conversation("a").await.expect("create").id;
        let there = db.conversation("b").await.expect("create").id;
        db.save_memory(here, Edit::Add("a".to_owned()))
            .await
            .expect("add");
        db.save_memory(here, Edit::Add("b".to_owned()))
            .await
            .expect("add");
        db.save_memory(there, Edit::Add("other".to_owned()))
            .await
            .expect("add");
        let ids: Vec<i64> = db
            .memories(here)
            .await
            .expect("load")
            .iter()
            .map(|m| m.id)
            .collect();
        db.save_memory(here, Edit::Replace(ids[0], "a2".to_owned()))
            .await
            .expect("replace");
        db.save_memory(here, Edit::Remove(ids[1]))
            .await
            .expect("remove");
        // An edit can't reach into another conversation's memory.
        db.save_memory(there, Edit::Remove(ids[0]))
            .await
            .expect("no-op");
        let contents = |rows: Vec<Memory>| rows.into_iter().map(|m| m.content).collect::<Vec<_>>();
        assert_eq!(contents(db.memories(here).await.expect("load")), ["a2"]);
        assert_eq!(contents(db.memories(there).await.expect("load")), ["other"]);
    }

    #[test]
    fn global_memories_are_copied_to_every_conversation() {
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
