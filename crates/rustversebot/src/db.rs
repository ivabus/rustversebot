use anyhow::{Context, bail};
use libsql::{Builder, Connection, Database, params};
use serde::Serialize;

const SCHEMA_VERSION: i64 = 5;

/// A compact, normalized copy of a production season from Nanoka's index.
/// Details and rendered cards deliberately remain outside the cache.
#[derive(Debug, Clone)]
pub struct SeasonEvent {
    pub endgame_type: String,
    pub season_id: String,
    pub starts_at: String,
    pub ends_at: Option<String>,
    pub name: String,
}

/// Async persistence backed by a local libSQL database.
pub struct Db {
    connection: Connection,
}

impl Db {
    /// Open a local `file:path.db` database.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let path = url
            .strip_prefix("file:")
            .context("TURSO_DATABASE_URL must use a local file: URL")?;
        if path.is_empty() {
            bail!("local database URL must include a path after file:");
        }
        let database = Builder::new_local(path)
            .build()
            .await
            .with_context(|| format!("opening local libSQL database {path}"))?;
        Self::from_database(database).await
    }

    #[cfg(test)]
    pub async fn new_test() -> anyhow::Result<Self> {
        Self::from_database(Builder::new_local(":memory:").build().await?).await
    }

    async fn from_database(database: Database) -> anyhow::Result<Self> {
        let connection = database.connect().context("opening libSQL connection")?;
        let db = Self { connection };
        db.migrate().await?;
        Ok(db)
    }

    fn connection(&self) -> anyhow::Result<Connection> {
        Ok(self.connection.clone())
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.connection()?;
        let tx = conn.transaction().await.context("starting migration")?;
        let mut rows = tx.query("PRAGMA user_version", ()).await?;
        let version = rows.next().await?.map_or(Ok(0), |row| row.get(0))?;
        drop(rows);
        if version > SCHEMA_VERSION {
            bail!(
                "database schema version {version} is newer than supported version {SCHEMA_VERSION}"
            );
        }
        if version < 1 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL,
                    telegram_user_id INTEGER NOT NULL, uid TEXT NOT NULL, nickname TEXT,
                    registered_at TEXT NOT NULL DEFAULT (datetime('now')), UNIQUE(chat_id, uid));
                 CREATE TABLE IF NOT EXISTS cookies (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, cookie_string TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')));
                 CREATE TABLE IF NOT EXISTS endgame_results (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, uid TEXT NOT NULL,
                    endgame_type TEXT NOT NULL, season_start TEXT NOT NULL,
                    season_end TEXT NOT NULL, total_score INTEGER NOT NULL DEFAULT 0,
                    data_json TEXT NOT NULL, fetched_at TEXT NOT NULL DEFAULT (datetime('now')));
                 CREATE INDEX IF NOT EXISTS idx_results_uid_type
                    ON endgame_results(uid, endgame_type, season_start);
                 CREATE TABLE IF NOT EXISTS avatar_cache (
                    uid TEXT NOT NULL, avatar_id INTEGER NOT NULL, name TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (uid, avatar_id));
                 PRAGMA user_version = 1;",
            )
            .await
            .context("applying migration 1")?;
        }
        if version < 2 {
            let mut rows = tx
                .query(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master
                     WHERE type='table' AND name='checkpoints_posted')",
                    (),
                )
                .await?;
            let legacy: i64 = rows
                .next()
                .await?
                .context("legacy checkpoint existence query returned no row")?
                .get(0)?;
            drop(rows);
            if legacy != 0 {
                tx.execute_batch(
                    "ALTER TABLE checkpoints_posted RENAME TO checkpoints_posted_legacy;
                     CREATE TABLE checkpoints_posted (
                       id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL,
                       endgame_type TEXT NOT NULL, season_start TEXT NOT NULL,
                       checkpoint TEXT NOT NULL, posted_at TEXT NOT NULL DEFAULT (datetime('now')),
                       UNIQUE(chat_id,endgame_type,season_start,checkpoint));
                     INSERT OR IGNORE INTO checkpoints_posted
                       (chat_id,endgame_type,season_start,checkpoint,posted_at)
                     SELECT chats.chat_id,old.endgame_type,old.season_start,old.checkpoint,old.posted_at
                     FROM checkpoints_posted_legacy old
                     CROSS JOIN (SELECT DISTINCT chat_id FROM users) chats;
                     DROP TABLE checkpoints_posted_legacy;",
                )
                .await?;
            } else {
                tx.execute_batch(
                    "CREATE TABLE checkpoints_posted (
                       id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL,
                       endgame_type TEXT NOT NULL, season_start TEXT NOT NULL,
                       checkpoint TEXT NOT NULL, posted_at TEXT NOT NULL DEFAULT (datetime('now')),
                       UNIQUE(chat_id,endgame_type,season_start,checkpoint));",
                )
                .await?;
            }
            tx.execute("PRAGMA user_version = 2", ()).await?;
        }
        if version < 3 {
            tx.execute(
                "DELETE FROM endgame_results WHERE id NOT IN (
                   SELECT MIN(id) FROM endgame_results GROUP BY uid,endgame_type,season_start,
                   season_end,total_score,data_json)",
                (),
            )
            .await?;
            tx.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_results_snapshot_unique
                 ON endgame_results(uid,endgame_type,season_start,season_end,total_score,data_json)",
                (),
            )
            .await?;
            tx.execute("PRAGMA user_version = 3", ()).await?;
        }
        if version < 4 {
            tx.execute_batch(
                "CREATE TABLE season_announcements (
                    chat_id INTEGER NOT NULL,
                    event_kind TEXT NOT NULL,
                    season_id TEXT NOT NULL,
                    starts_at TEXT NOT NULL,
                    posted_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY(chat_id,event_kind,season_id));
                 PRAGMA user_version = 4;",
            )
            .await
            .context("applying migration 4")?;
        }
        if version < 5 {
            tx.execute_batch(
                "CREATE TABLE season_events (
                    endgame_type TEXT NOT NULL,
                    season_id TEXT NOT NULL,
                    starts_at TEXT NOT NULL,
                    ends_at TEXT,
                    name TEXT NOT NULL,
                    observed_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY(endgame_type, season_id));
                 CREATE INDEX idx_season_events_start
                    ON season_events(endgame_type, starts_at);
                 PRAGMA user_version = 5;",
            )
            .await
            .context("applying migration 5")?;
        }
        tx.commit().await.context("committing migrations")?;
        Ok(())
    }

    async fn optional_string(
        &self,
        sql: &str,
        args: impl libsql::params::IntoParams,
    ) -> anyhow::Result<Option<String>> {
        let mut rows = self.connection()?.query(sql, args).await?;
        rows.next()
            .await?
            .map(|row| row.get(0))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn get_cookie(&self) -> anyhow::Result<Option<String>> {
        self.optional_string(
            "SELECT cookie_string FROM cookies ORDER BY id DESC LIMIT 1",
            (),
        )
        .await
    }
    pub async fn set_cookie(&self, cookie: &str) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "INSERT INTO cookies(cookie_string) VALUES (?1)",
                params![cookie],
            )
            .await?;
        Ok(())
    }
    pub async fn add_user(
        &self,
        chat_id: i64,
        telegram_user_id: i64,
        uid: &str,
        nickname: Option<&str>,
    ) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "INSERT OR REPLACE INTO users(chat_id,telegram_user_id,uid,nickname,registered_at)
             VALUES (?1,?2,?3,?4,datetime('now'))",
                params![chat_id, telegram_user_id, uid, nickname],
            )
            .await?;
        Ok(())
    }
    pub async fn remove_user(&self, chat_id: i64, uid: &str) -> anyhow::Result<bool> {
        Ok(self
            .connection()?
            .execute(
                "DELETE FROM users WHERE chat_id=?1 AND uid=?2",
                params![chat_id, uid],
            )
            .await?
            > 0)
    }
    pub async fn get_users_by_chat(&self, chat_id: i64) -> anyhow::Result<Vec<UserRow>> {
        self.users(
            "SELECT chat_id,telegram_user_id,uid,nickname FROM users WHERE chat_id=?1 ORDER BY id",
            params![chat_id],
        )
        .await
    }
    pub async fn get_all_users(&self) -> anyhow::Result<Vec<UserRow>> {
        self.users(
            "SELECT chat_id,telegram_user_id,uid,nickname FROM users ORDER BY id",
            (),
        )
        .await
    }
    async fn users(
        &self,
        sql: &str,
        args: impl libsql::params::IntoParams,
    ) -> anyhow::Result<Vec<UserRow>> {
        let mut rows = self.connection()?.query(sql, args).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(UserRow {
                chat_id: r.get(0)?,
                telegram_user_id: r.get(1)?,
                uid: r.get(2)?,
                nickname: r.get(3)?,
            });
        }
        Ok(out)
    }
    pub async fn find_uid_by_nickname(
        &self,
        chat_id: i64,
        nickname: &str,
    ) -> anyhow::Result<Option<String>> {
        self.optional_string(
            "SELECT uid FROM users WHERE chat_id=?1 AND nickname=?2 COLLATE NOCASE LIMIT 1",
            params![chat_id, nickname],
        )
        .await
    }
    pub async fn get_nickname_for_uid(&self, uid: &str) -> anyhow::Result<Option<String>> {
        self.optional_string(
            "SELECT nickname FROM users WHERE uid=?1 AND nickname IS NOT NULL LIMIT 1",
            params![uid],
        )
        .await
    }
    pub async fn get_user(&self, chat_id: i64, uid: &str) -> anyhow::Result<Option<UserRow>> {
        Ok(self.users("SELECT chat_id,telegram_user_id,uid,nickname FROM users WHERE chat_id=?1 AND uid=?2",params![chat_id,uid]).await?.into_iter().next())
    }
    pub async fn get_distinct_chats(&self) -> anyhow::Result<Vec<i64>> {
        let mut rows = self
            .connection()?
            .query("SELECT DISTINCT chat_id FROM users", ())
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(r.get(0)?);
        }
        Ok(out)
    }
    pub async fn insert_result(
        &self,
        uid: &str,
        endgame_type: &str,
        season_start: &str,
        season_end: &str,
        total_score: i64,
        data_json: &str,
    ) -> anyhow::Result<()> {
        self.connection()?.execute("INSERT OR IGNORE INTO endgame_results(uid,endgame_type,season_start,season_end,total_score,data_json) VALUES(?1,?2,?3,?4,?5,?6)",params![uid,endgame_type,season_start,season_end,total_score,data_json]).await?;
        Ok(())
    }
    pub async fn get_latest_results(
        &self,
        chat_id: i64,
        endgame_type: &str,
        season_start: &str,
    ) -> anyhow::Result<Vec<LeaderboardEntry>> {
        let mut rows=self.connection()?.query(
            "SELECT r.uid,u.telegram_user_id,u.nickname,r.total_score,r.data_json,r.season_end FROM users u
             JOIN endgame_results r ON r.id=(SELECT MAX(x.id) FROM endgame_results x WHERE x.uid=u.uid AND x.endgame_type=?2 AND x.season_start=?3)
             WHERE u.chat_id=?1 ORDER BY r.total_score DESC",params![chat_id,endgame_type,season_start]).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(LeaderboardEntry {
                uid: r.get(0)?,
                telegram_user_id: r.get(1)?,
                nickname: r.get(2)?,
                total_score: r.get(3)?,
                data_json: r.get(4)?,
                season_end: r.get(5)?,
            });
        }
        Ok(out)
    }
    pub async fn get_latest_season_start(
        &self,
        endgame_type: &str,
    ) -> anyhow::Result<Option<String>> {
        self.optional_string("SELECT season_start FROM endgame_results WHERE endgame_type=?1 ORDER BY id DESC LIMIT 1",params![endgame_type]).await
    }
    pub async fn get_latest_result_json(
        &self,
        uid: &str,
        endgame_type: &str,
    ) -> anyhow::Result<Option<String>> {
        self.optional_string("SELECT data_json FROM endgame_results WHERE uid=?1 AND endgame_type=?2 ORDER BY id DESC LIMIT 1",params![uid,endgame_type]).await
    }
    pub async fn get_current_and_previous_result(
        &self,
        uid: &str,
        endgame_type: &str,
    ) -> anyhow::Result<Vec<SeasonResult>> {
        let mut rows=self.connection()?.query(
            "SELECT season_start,total_score,data_json FROM endgame_results r WHERE r.uid=?1 AND r.endgame_type=?2
             AND r.id=(SELECT MAX(x.id) FROM endgame_results x WHERE x.uid=r.uid AND x.endgame_type=r.endgame_type AND x.season_start=r.season_start)
             ORDER BY season_start DESC LIMIT 2",params![uid,endgame_type]).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(SeasonResult {
                season_start: r.get(0)?,
                total_score: r.get(1)?,
                data_json: r.get(2)?,
            });
        }
        Ok(out)
    }
    pub async fn cache_avatars(&self, uid: &str, names: &[(i64, &str)]) -> anyhow::Result<()> {
        let conn = self.connection()?;
        let tx = conn.transaction().await?;
        for (id, name) in names {
            tx.execute("INSERT OR REPLACE INTO avatar_cache(uid,avatar_id,name,updated_at) VALUES(?1,?2,?3,datetime('now'))",params![uid,*id,*name]).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn resolve_avatar_name(
        &self,
        uid: &str,
        avatar_id: i64,
    ) -> anyhow::Result<Option<String>> {
        self.optional_string(
            "SELECT name FROM avatar_cache WHERE uid=?1 AND avatar_id=?2",
            params![uid, avatar_id],
        )
        .await
    }
    pub async fn is_checkpoint_posted(
        &self,
        chat_id: i64,
        endgame_type: &str,
        season_start: &str,
        checkpoint: &str,
    ) -> anyhow::Result<bool> {
        let mut rows=self.connection()?.query("SELECT EXISTS(SELECT 1 FROM checkpoints_posted WHERE chat_id=?1 AND endgame_type=?2 AND season_start=?3 AND checkpoint=?4)",params![chat_id,endgame_type,season_start,checkpoint]).await?;
        Ok(rows
            .next()
            .await?
            .context("checkpoint existence query returned no row")?
            .get::<i64>(0)?
            != 0)
    }
    pub async fn mark_checkpoint_posted(
        &self,
        chat_id: i64,
        endgame_type: &str,
        season_start: &str,
        checkpoint: &str,
    ) -> anyhow::Result<()> {
        self.connection()?.execute("INSERT OR IGNORE INTO checkpoints_posted(chat_id,endgame_type,season_start,checkpoint) VALUES(?1,?2,?3,?4)",params![chat_id,endgame_type,season_start,checkpoint]).await?;
        Ok(())
    }
    pub async fn is_season_announcement_posted(
        &self,
        chat_id: i64,
        event_kind: &str,
        season_id: &str,
    ) -> anyhow::Result<bool> {
        let mut rows = self
            .connection()?
            .query(
                "SELECT EXISTS(SELECT 1 FROM season_announcements
             WHERE chat_id=?1 AND event_kind=?2 AND season_id=?3)",
                params![chat_id, event_kind, season_id],
            )
            .await?;
        Ok(rows
            .next()
            .await?
            .context("season announcement existence query returned no row")?
            .get::<i64>(0)?
            != 0)
    }
    pub async fn mark_season_announcement_posted(
        &self,
        chat_id: i64,
        event_kind: &str,
        season_id: &str,
        starts_at: &str,
    ) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "INSERT OR IGNORE INTO season_announcements
             (chat_id,event_kind,season_id,starts_at) VALUES(?1,?2,?3,?4)",
                params![chat_id, event_kind, season_id, starts_at],
            )
            .await?;
        Ok(())
    }
    pub async fn cache_season_events(&self, events: &[SeasonEvent]) -> anyhow::Result<()> {
        let conn = self.connection()?;
        let tx = conn.transaction().await?;
        // Both Nanoka indexes were fetched successfully before this method is
        // called, so replacing this small cache keeps it authoritative.
        tx.execute("DELETE FROM season_events", ()).await?;
        for event in events {
            tx.execute(
                "INSERT INTO season_events(endgame_type,season_id,starts_at,ends_at,name,observed_at)
                 VALUES(?1,?2,?3,?4,?5,datetime('now'))
                 ON CONFLICT(endgame_type,season_id) DO UPDATE SET
                   starts_at=excluded.starts_at, ends_at=excluded.ends_at,
                   name=excluded.name, observed_at=datetime('now')",
                params![
                    event.endgame_type.clone(),
                    event.season_id.clone(),
                    event.starts_at.clone(),
                    event.ends_at.clone(),
                    event.name.clone()
                ],
            ).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn next_season_event(
        &self,
        endgame_type: &str,
        after: &str,
    ) -> anyhow::Result<Option<SeasonEvent>> {
        let mut rows = self
            .connection()?
            .query(
                "SELECT endgame_type,season_id,starts_at,ends_at,name FROM season_events
             WHERE endgame_type=?1 AND starts_at>?2 ORDER BY starts_at LIMIT 1",
                params![endgame_type, after],
            )
            .await?;
        rows.next()
            .await?
            .map(|row| {
                Ok(SeasonEvent {
                    endgame_type: row.get(0)?,
                    season_id: row.get(1)?,
                    starts_at: row.get(2)?,
                    ends_at: row.get(3)?,
                    name: row.get(4)?,
                })
            })
            .transpose()
    }
    pub async fn active_season_events(&self, now: &str) -> anyhow::Result<Vec<SeasonEvent>> {
        let mut rows = self.connection()?.query(
            "SELECT endgame_type,season_id,starts_at,ends_at,name FROM season_events
             WHERE starts_at<=?1 AND (ends_at IS NULL OR ends_at>?1) ORDER BY endgame_type,starts_at DESC",
            params![now],
        ).await?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().await? {
            events.push(SeasonEvent {
                endgame_type: row.get(0)?,
                season_id: row.get(1)?,
                starts_at: row.get(2)?,
                ends_at: row.get(3)?,
                name: row.get(4)?,
            });
        }
        Ok(events)
    }
    pub async fn cleanup_old_results(&self, retention_days: i64) -> anyhow::Result<usize> {
        if retention_days <= 0 {
            bail!("retention_days must be greater than zero");
        }
        let changed=self.connection()?.execute("DELETE FROM endgame_results WHERE fetched_at < datetime('now','-' || ?1 || ' days') AND id NOT IN(SELECT MAX(id) FROM endgame_results GROUP BY uid,endgame_type,season_start)",params![retention_days]).await?;
        usize::try_from(changed).context("affected row count is too large")
    }
    pub async fn web_chats(&self) -> anyhow::Result<Vec<WebChatSummary>> {
        let mut rows = self
            .connection()?
            .query(
                "SELECT chat_id,COUNT(*) FROM users GROUP BY chat_id ORDER BY chat_id",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(WebChatSummary {
                chat_id: r.get(0)?,
                user_count: r.get(1)?,
            });
        }
        Ok(out)
    }
    pub async fn web_leaderboard(
        &self,
        chat_id: i64,
        endgame_type: &str,
    ) -> anyhow::Result<Vec<WebLeaderboardEntry>> {
        let mut rows=self.connection()?.query(
            "SELECT r.uid,u.nickname,r.total_score,r.season_start,r.season_end,r.fetched_at FROM users u
             JOIN endgame_results r ON r.id=(SELECT MAX(x.id) FROM endgame_results x WHERE x.uid=u.uid AND x.endgame_type=?2)
             WHERE u.chat_id=?1 ORDER BY r.total_score DESC,r.uid",params![chat_id,endgame_type]).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(WebLeaderboardEntry {
                uid: r.get(0)?,
                nickname: r.get(1)?,
                total_score: r.get(2)?,
                season_start: r.get(3)?,
                season_end: r.get(4)?,
                fetched_at: r.get(5)?,
            });
        }
        Ok(out)
    }
    pub async fn web_history(
        &self,
        uid: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<WebHistoryEntry>> {
        let limit = i64::try_from(limit)?;
        let mut rows=self.connection()?.query(
            "SELECT endgame_type,season_start,season_end,total_score,fetched_at FROM
             (SELECT id,endgame_type,season_start,season_end,total_score,fetched_at FROM endgame_results WHERE uid=?1 ORDER BY id DESC LIMIT ?2)
             ORDER BY id ASC",params![uid,limit]).await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(WebHistoryEntry {
                endgame_type: r.get(0)?,
                season_start: r.get(1)?,
                season_end: r.get(2)?,
                total_score: r.get(3)?,
                fetched_at: r.get(4)?,
            });
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub chat_id: i64,
    pub telegram_user_id: i64,
    pub uid: String,
    pub nickname: Option<String>,
}
#[derive(Debug, Clone)]
pub struct LeaderboardEntry {
    pub uid: String,
    pub telegram_user_id: i64,
    pub nickname: Option<String>,
    pub total_score: i64,
    pub data_json: String,
    pub season_end: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebChatSummary {
    pub chat_id: i64,
    pub user_count: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebLeaderboardEntry {
    pub uid: String,
    pub nickname: Option<String>,
    pub total_score: i64,
    pub season_start: String,
    pub season_end: String,
    pub fetched_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebHistoryEntry {
    pub endgame_type: String,
    pub season_start: String,
    pub season_end: String,
    pub total_score: i64,
    pub fetched_at: String,
}
#[derive(Debug, Clone)]
pub struct SeasonResult {
    pub season_start: String,
    pub total_score: i64,
    pub data_json: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn season_index_cache_is_normalized_and_replaced() {
        let db = Db::new_test().await.unwrap();
        let first = SeasonEvent {
            endgame_type: "deadly_assault".into(),
            season_id: "69041".into(),
            starts_at: "2026-07-25T22:00:00+00:00".into(),
            ends_at: None,
            name: "First".into(),
        };
        let changed = SeasonEvent {
            name: "Corrected".into(),
            ..first.clone()
        };
        db.cache_season_events(&[first, changed]).await.unwrap();
        let event = db
            .next_season_event("deadly_assault", "2026-07-24T00:00:00+00:00")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.name, "Corrected");

        db.cache_season_events(&[]).await.unwrap();
        assert!(
            db.next_season_event("deadly_assault", "2026-07-24T00:00:00+00:00")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn accepts_local_file_urls_and_rejects_remote_urls() {
        let db = Db::connect("file::memory:").await.unwrap();
        assert!(db.get_all_users().await.unwrap().is_empty());

        let error = Db::connect("libsql://example.invalid")
            .await
            .err()
            .expect("remote databases must be rejected");
        assert!(error.to_string().contains("local file: URL"));
    }

    #[tokio::test]
    async fn migrations_and_core_queries_work_in_memory() {
        let db = Db::new_test().await.unwrap();
        db.add_user(10, 1, "123456789", Some("Alice"))
            .await
            .unwrap();
        db.insert_result("123456789", "deadly_assault", "2026-07-01", "end", 10, "{}")
            .await
            .unwrap();
        db.insert_result("123456789", "deadly_assault", "2026-07-01", "end", 10, "{}")
            .await
            .unwrap();
        assert_eq!(db.get_users_by_chat(10).await.unwrap().len(), 1);
        assert_eq!(
            db.get_latest_results(10, "deadly_assault", "2026-07-01")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(db.web_history("123456789", 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn checkpoints_are_per_chat_and_retention_is_validated() {
        let db = Db::new_test().await.unwrap();
        db.mark_checkpoint_posted(10, "deadly_assault", "season", "6h")
            .await
            .unwrap();
        assert!(
            db.is_checkpoint_posted(10, "deadly_assault", "season", "6h")
                .await
                .unwrap()
        );
        assert!(
            !db.is_checkpoint_posted(20, "deadly_assault", "season", "6h")
                .await
                .unwrap()
        );
        assert!(db.cleanup_old_results(0).await.is_err());

        db.mark_season_announcement_posted(10, "deadly_assault", "69041", "2026-07-25 06:00:00")
            .await
            .unwrap();
        assert!(
            db.is_season_announcement_posted(10, "deadly_assault", "69041")
                .await
                .unwrap()
        );
        assert!(
            !db.is_season_announcement_posted(20, "deadly_assault", "69041")
                .await
                .unwrap()
        );
    }
}
