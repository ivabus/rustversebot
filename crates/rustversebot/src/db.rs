use anyhow::{Context, bail};
use libsql::{Builder, Connection, Database, params};
use serde::Serialize;
use std::collections::HashMap;
use uuid::Uuid;

mod admin;
pub(crate) use admin::AiFeature;

const SCHEMA_VERSION: i64 = 20;

#[cfg_attr(test, derive(Default))]
pub struct DeliveredTurn<'a> {
    pub session_id: Uuid,
    pub question: &'a str,
    pub image_ids: &'a [String],
    pub answer: &'a str,
    pub author: Option<i64>,
    pub references: rustverse_ai::TurnReferences,
}

pub struct PromptSession {
    pub id: Uuid,
    pub last_answer: Option<i32>,
    pub turns: Vec<rustverse_ai::ImagePromptTurn<String>>,
}

impl Default for PromptSession {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            last_answer: None,
            turns: Vec::new(),
        }
    }
}

#[derive(Serialize)]
pub struct AgentUser {
    id: i64,
    username: Option<String>,
    name: String,
    last_seen: String,
    command: String,
    decision: String,
    requests: i64,
    banned: bool,
}

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
        if version < 6 {
            tx.execute_batch(
                "CREATE TABLE promo_chat_baselines (
                    chat_id INTEGER PRIMARY KEY, codes_json TEXT NOT NULL);
                 CREATE TABLE promo_announcements (
                    chat_id INTEGER NOT NULL, code TEXT COLLATE NOCASE NOT NULL,
                    posted_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY(chat_id, code));
                 CREATE TRIGGER reset_promo_baseline AFTER DELETE ON users
                 WHEN NOT EXISTS (SELECT 1 FROM users WHERE chat_id = OLD.chat_id)
                 BEGIN
                    DELETE FROM promo_chat_baselines WHERE chat_id = OLD.chat_id;
                 END;
                 PRAGMA user_version = 6;",
            )
            .await
            .context("applying migration 6")?;
        }
        if version < 7 {
            tx.execute_batch(
                "CREATE TABLE beta_cursors(chat_id INTEGER PRIMARY KEY, revision TEXT NOT NULL);
                 CREATE TABLE beta_deliveries(chat_id INTEGER NOT NULL, revision TEXT NOT NULL, agent INTEGER NOT NULL,
                    PRIMARY KEY(chat_id, revision, agent));
                 CREATE TABLE beta_reports(previous TEXT NOT NULL, current TEXT NOT NULL, agent INTEGER NOT NULL,
                    report TEXT NOT NULL, PRIMARY KEY(previous,current,agent));
                 CREATE TRIGGER reset_beta_cursor AFTER DELETE ON users
                   WHEN NOT EXISTS (SELECT 1 FROM users WHERE chat_id=OLD.chat_id)
                   BEGIN DELETE FROM beta_cursors WHERE chat_id=OLD.chat_id; END;
                 PRAGMA user_version = 7;"
            ).await.context("applying migration 7")?;
        }
        if version < 8 {
            // Old rows do not identify the model or prompts used to generate them.
            tx.execute_batch(
                "ALTER TABLE beta_reports RENAME TO beta_reports_v7;
                 CREATE TABLE beta_reports(previous TEXT NOT NULL, current TEXT NOT NULL, agent INTEGER NOT NULL,
                    model TEXT NOT NULL, prompt_version TEXT NOT NULL, report TEXT NOT NULL,
                    PRIMARY KEY(previous,current,agent,model,prompt_version));
                 INSERT INTO beta_reports SELECT previous,current,agent,'','',report FROM beta_reports_v7;
                 DROP TABLE beta_reports_v7;
                 PRAGMA user_version = 8;"
            ).await.context("applying migration 8")?;
        }
        if version < 9 {
            tx.execute_batch(
                "DROP TRIGGER reset_beta_cursor;
                 CREATE TABLE revision_cursors(chat_id INTEGER NOT NULL, branch TEXT NOT NULL,
                    revision TEXT NOT NULL, PRIMARY KEY(chat_id,branch));
                 INSERT INTO revision_cursors SELECT chat_id,'beta',revision FROM beta_cursors;
                 DROP TABLE beta_cursors;
                 CREATE TRIGGER reset_revision_cursor AFTER DELETE ON users
                   WHEN NOT EXISTS (SELECT 1 FROM users WHERE chat_id=OLD.chat_id)
                   BEGIN DELETE FROM revision_cursors WHERE chat_id=OLD.chat_id; END;
                 CREATE TABLE ai_cooldowns(user_id INTEGER PRIMARY KEY, until_unix INTEGER NOT NULL);
                 PRAGMA user_version=9;"
            ).await.context("applying migration 9")?;
        }
        if version < 10 {
            tx.execute_batch(
                "CREATE TABLE ai_bans(user_id INTEGER PRIMARY KEY);
                 CREATE TABLE ai_requests(id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL,
                   username TEXT, full_name TEXT NOT NULL, command TEXT NOT NULL,
                   decision TEXT NOT NULL, at_unix INTEGER NOT NULL DEFAULT (unixepoch()));
                 CREATE INDEX ai_requests_user ON ai_requests(user_id,id);
                 CREATE TRIGGER start_ai_cooldown AFTER INSERT ON ai_requests
                 WHEN NEW.decision='allowed' BEGIN
                   INSERT INTO ai_cooldowns VALUES(NEW.user_id, NEW.at_unix+60)
                   ON CONFLICT(user_id) DO UPDATE SET until_unix=excluded.until_unix;
                 END;
                 PRAGMA user_version=10;",
            )
            .await
            .context("applying migration 10")?;
        }
        if version < 11 {
            tx.execute_batch(
                "CREATE TABLE prompt_turns (
                    chat_id INTEGER NOT NULL, message_id INTEGER NOT NULL, topic_id INTEGER,
                    parent_id INTEGER, question TEXT NOT NULL, answer TEXT NOT NULL,
                    expires_at INTEGER NOT NULL DEFAULT (unixepoch()+86400),
                    PRIMARY KEY(chat_id,message_id));
                 CREATE INDEX prompt_turns_expiry ON prompt_turns(expires_at);
                 CREATE TABLE prompt_parts (
                    chat_id INTEGER NOT NULL, message_id INTEGER NOT NULL, first_id INTEGER NOT NULL,
                    PRIMARY KEY(chat_id,message_id));
                 CREATE INDEX prompt_parts_first ON prompt_parts(chat_id,first_id);
                 CREATE TRIGGER expire_prompt_parts AFTER DELETE ON prompt_turns BEGIN
                    DELETE FROM prompt_parts WHERE chat_id=OLD.chat_id AND first_id=OLD.message_id;
                 END;
                 PRAGMA user_version=11;",
            ).await.context("applying migration 11")?;
        }
        if version < 12 {
            tx.execute_batch(
                "CREATE TABLE ai_model (
                id INTEGER PRIMARY KEY CHECK(id=1), setting TEXT NOT NULL);
                PRAGMA user_version=12;",
            )
            .await
            .context("applying migration 12")?;
        }
        if version < 13 {
            tx.execute_batch(
                "ALTER TABLE prompt_turns ADD COLUMN author INTEGER;
                ALTER TABLE prompt_turns ADD COLUMN lookup_refs TEXT;
                PRAGMA user_version=13;",
            )
            .await
            .context("applying migration 13")?;
        }
        if version < 14 {
            tx.execute_batch(
                "CREATE TABLE chat_features (
                    chat_id INTEGER PRIMARY KEY, prompt INTEGER NOT NULL DEFAULT 0,
                    diff INTEGER NOT NULL DEFAULT 0, title TEXT);
                 INSERT INTO chat_features(chat_id,prompt,diff) SELECT DISTINCT chat_id,1,1 FROM users;
                 CREATE INDEX prompt_turns_parent ON prompt_turns(chat_id,parent_id);
                 CREATE TRIGGER disable_chat_diff AFTER UPDATE OF diff ON chat_features
                 WHEN OLD.diff=1 AND NEW.diff=0 BEGIN
                    DELETE FROM revision_cursors WHERE chat_id=NEW.chat_id;
                 END;
                 PRAGMA user_version=14;",
            ).await.context("applying migration 14")?;
        }
        if version < 15 {
            tx.execute_batch(
                "ALTER TABLE prompt_turns ADD COLUMN image_ids TEXT NOT NULL DEFAULT '[]';
                PRAGMA user_version=15;",
            )
            .await
            .context("applying migration 15")?;
        }
        if version < 16 {
            tx.execute_batch("ALTER TABLE prompt_turns ADD COLUMN session_id TEXT;")
                .await?;
            // Backfill each surviving conversation in one transaction, including
            // branches whose original parent has already expired and been deleted.
            let mut roots = tx
                .query(
                    "SELECT DISTINCT t.chat_id,t.topic_id,COALESCE(t.parent_id,t.message_id) FROM prompt_turns t
                WHERE NOT EXISTS (SELECT 1 FROM prompt_turns p WHERE p.chat_id=t.chat_id
                    AND p.topic_id IS t.topic_id AND p.message_id=t.parent_id)",
                    (),
                )
                .await?;
            while let Some(root) = roots.next().await? {
                tx.execute("WITH RECURSIVE branch(id) AS (SELECT ?3 UNION ALL
                    SELECT p.message_id FROM prompt_turns p JOIN branch ON p.parent_id=branch.id
                    WHERE p.chat_id=?1 AND p.topic_id IS ?2)
                    UPDATE prompt_turns SET session_id=?4 WHERE chat_id=?1 AND topic_id IS ?2 AND message_id IN branch",
                    params![root.get::<i64>(0)?, root.get::<Option<i32>>(1)?, root.get::<i32>(2)?, Uuid::new_v4().to_string()]).await?;
            }
            tx.execute_batch("PRAGMA user_version=16;").await?;
        }
        if version < 19 {
            // Versions 17 and 18 indexed battle rosters for the retired AI tools.
            tx.execute_batch(
                "DROP TRIGGER IF EXISTS collect_played_agents;
                 DROP TRIGGER IF EXISTS cache_used_agent;
                 DROP VIEW IF EXISTS played_agents;
                 DROP TABLE IF EXISTS used_agents;
                 PRAGMA user_version=19;",
            )
            .await
            .context("retiring the AI roster index")?;
        }
        if version < 20 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS beta_pending_text (
                    chat_id INTEGER NOT NULL, revision TEXT NOT NULL, agent INTEGER NOT NULL,
                    remaining TEXT NOT NULL, PRIMARY KEY(chat_id,revision,agent));
                 PRAGMA user_version=20;",
            )
            .await
            .context("applying migration 20")?;
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

    pub async fn model_override(&self) -> anyhow::Result<Option<String>> {
        self.optional_string("SELECT setting FROM ai_model WHERE id=1", ())
            .await
    }

    pub async fn set_ai_model(&self, setting: &str) -> anyhow::Result<()> {
        let setting = if setting == "auto" {
            setting.to_owned()
        } else {
            setting.parse::<rustverse_ai::ModelSelection>()?.to_string()
        };
        self.connection()?
            .execute(
                "INSERT INTO ai_model(id,setting) VALUES(1,?1)
            ON CONFLICT(id) DO UPDATE SET setting=excluded.setting",
                [setting],
            )
            .await?;
        Ok(())
    }

    /// Follow only the selected answer's live ancestors, in conversational order.
    pub async fn prompt_session(
        &self,
        chat: i64,
        topic: Option<i32>,
        reply_to: i32,
        participant: Option<i64>,
    ) -> anyhow::Result<Option<PromptSession>> {
        let mut rows=self.connection()?.query(
            "WITH RECURSIVE history(message_id,parent_id,question,answer,author,lookup_refs,image_ids,session_id,depth) AS (
                SELECT message_id,parent_id,question,answer,author,lookup_refs,image_ids,session_id,0 FROM prompt_turns
                WHERE chat_id=?1 AND topic_id IS ?2 AND expires_at>unixepoch()
                  AND message_id=COALESCE((SELECT first_id FROM prompt_parts WHERE chat_id=?1 AND message_id=?3),?3)
                UNION ALL
                SELECT p.message_id,p.parent_id,p.question,p.answer,p.author,p.lookup_refs,p.image_ids,p.session_id,h.depth+1
                FROM history h JOIN prompt_turns p ON p.chat_id=?1 AND p.message_id=h.parent_id
                WHERE p.topic_id IS ?2 AND p.expires_at>unixepoch()
             ) SELECT message_id,question,answer,author,lookup_refs,image_ids,session_id FROM history ORDER BY depth DESC",
            params![chat,topic,reply_to]).await?;
        let mut session = None;
        while let Some(row) = rows.next().await? {
            let session = match &mut session {
                Some(session) => session,
                None => session.insert(PromptSession {
                    id: row.get::<String>(6)?.parse()?,
                    last_answer: None,
                    turns: Vec::new(),
                }),
            };
            session.last_answer = Some(row.get(0)?);
            let mut question: String = row.get(1)?;
            if let Some(author) = row.get::<Option<i64>>(3)? {
                let speaker = if Some(author) == participant {
                    "Текущий участник"
                } else {
                    "Другой участник"
                };
                question = format!("[{speaker}] {question}");
            }
            let mut answer: String = row.get(2)?;
            if let Some(refs) = row.get::<Option<String>>(4)? {
                answer.push_str(&format!("\n\nПроверенные источники этой реплики: {refs}"));
            }
            session.turns.push(rustverse_ai::ImagePromptTurn {
                question,
                answer,
                images: serde_json::from_str(&row.get::<String>(5)?)?,
            });
        }
        Ok(session)
    }

    pub async fn save_prompt_turn(
        &self,
        chat: i64,
        topic: Option<i32>,
        message: i32,
        parent: Option<i32>,
        turn: &DeliveredTurn<'_>,
    ) -> anyhow::Result<()> {
        let refs = (!turn.references.is_empty())
            .then(|| serde_json::to_string(&turn.references))
            .transpose()?;
        self.connection()?
            .execute(
                "INSERT INTO prompt_turns(chat_id,topic_id,message_id,parent_id,question,answer,author,lookup_refs,image_ids,session_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(chat_id,message_id) DO NOTHING",
                params![chat, topic, message, parent, turn.question, turn.answer, turn.author, refs, serde_json::to_string(turn.image_ids)?, turn.session_id.to_string()],
            )
            .await?;
        Ok(())
    }

    pub async fn save_prompt_part(
        &self,
        chat: i64,
        first: i32,
        message: i32,
    ) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "INSERT INTO prompt_parts(chat_id,message_id,first_id)
             SELECT chat_id,?3,message_id FROM prompt_turns WHERE chat_id=?1 AND message_id=?2
             ON CONFLICT(chat_id,message_id) DO NOTHING",
                params![chat, first, message],
            )
            .await?;
        Ok(())
    }

    pub async fn expire_prompt_sessions(&self) -> anyhow::Result<()> {
        self.connection()?
            .execute("DELETE FROM prompt_turns WHERE expires_at<=unixepoch()", ())
            .await?;
        Ok(())
    }

    pub async fn admit_ai(
        &self,
        user: i64,
        username: Option<&str>,
        name: &str,
        command: &str,
        admin: i64,
        capacity: bool,
    ) -> anyhow::Result<String> {
        // The decision, audit row, and cooldown trigger are one SQLite statement.
        self.optional_string("INSERT INTO ai_requests(user_id,username,full_name,command,decision)
            SELECT ?1,?2,?3,?4,CASE
              WHEN ?1=?5 THEN CASE WHEN ?6 THEN 'admin' ELSE 'busy' END
              WHEN EXISTS(SELECT 1 FROM ai_bans WHERE user_id=?1) THEN 'banned'
              WHEN EXISTS(SELECT 1 FROM ai_cooldowns WHERE user_id=?1 AND until_unix>unixepoch()) THEN 'limited'
              WHEN NOT ?6 THEN 'busy'
              ELSE 'allowed' END RETURNING decision",params![user,username,name,command,admin,capacity])
            .await?.context("missing AI access decision")
    }

    pub async fn ban_ai(&self, user: i64, banned: bool) -> anyhow::Result<()> {
        let sql = if banned {
            "INSERT OR IGNORE INTO ai_bans VALUES(?1)"
        } else {
            "DELETE FROM ai_bans WHERE user_id=?1"
        };
        self.connection()?.execute(sql, [user]).await?;
        Ok(())
    }

    pub async fn ai_users(&self, page: u32) -> anyhow::Result<Vec<AgentUser>> {
        let mut rows=self.connection()?.query(
          "SELECT r.user_id,substr(r.username,1,32),substr(replace(replace(r.full_name,char(10),' '),char(13),' '),1,64),datetime(r.at_unix,'unixepoch','+3 hours'),
                  r.command,r.decision,g.n,b.user_id IS NOT NULL
           FROM (SELECT max(id) AS last_id,count(*) AS n FROM ai_requests GROUP BY user_id) g
           JOIN ai_requests r ON r.id=g.last_id LEFT JOIN ai_bans b ON b.user_id=r.user_id
           ORDER BY r.id DESC LIMIT 15 OFFSET ?1",[i64::from(page.saturating_sub(1))*15]).await?;
        let mut users = Vec::new();
        while let Some(r) = rows.next().await? {
            users.push(AgentUser {
                id: r.get(0)?,
                username: r.get(1)?,
                name: r.get(2)?,
                last_seen: r.get(3)?,
                command: r.get(4)?,
                decision: r.get(5)?,
                requests: r.get(6)?,
                banned: r.get::<i64>(7)? != 0,
            });
        }
        Ok(users)
    }

    pub async fn reset_ai_limit(&self, user: i64) -> anyhow::Result<()> {
        self.connection()?
            .execute("DELETE FROM ai_cooldowns WHERE user_id=?1", [user])
            .await?;
        Ok(())
    }

    pub async fn revision_cursors(
        &self,
        branch: &str,
        latest: &str,
    ) -> anyhow::Result<Vec<(i64, String)>> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT OR IGNORE INTO revision_cursors SELECT DISTINCT chat_id, ?1, ?2
             FROM users JOIN chat_features USING(chat_id) WHERE diff=1",
            [branch, latest],
        )
        .await?;
        let mut rows = conn
            .query(
                "SELECT chat_id, revision FROM revision_cursors JOIN chat_features USING(chat_id)
                 WHERE diff=1 AND branch=?1 AND revision != ?2 ORDER BY chat_id",
                [branch, latest],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push((r.get(0)?, r.get(1)?));
        }
        Ok(out)
    }

    pub(crate) async fn revision_pending(
        &self,
        chat: i64,
        branch: &str,
        previous: &str,
    ) -> anyhow::Result<bool> {
        let mut rows = self
            .connection
            .query(
                "SELECT 1 FROM revision_cursors JOIN chat_features USING(chat_id)
             WHERE chat_id=?1 AND branch=?2 AND revision=?3 AND diff=1",
                params![chat, branch, previous],
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    pub async fn advance_revision(
        &self,
        chat: i64,
        branch: &str,
        prev: &str,
        cur: &str,
    ) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "UPDATE revision_cursors SET revision=?4 WHERE chat_id=?1 AND branch=?2 AND revision=?3",
                params![chat, branch, prev, cur],
            )
            .await?;
        Ok(())
    }

    pub async fn delivered_beta_agents(
        &self,
        chat: i64,
        cur: &str,
    ) -> anyhow::Result<std::collections::HashSet<u64>> {
        let mut rows = self
            .connection()?
            .query(
                "SELECT agent FROM beta_deliveries WHERE chat_id=?1 AND revision=?2",
                params![chat, cur],
            )
            .await?;
        let mut agents = std::collections::HashSet::new();
        while let Some(row) = rows.next().await? {
            agents.insert(u64::try_from(row.get::<i64>(0)?)?);
        }
        Ok(agents)
    }

    pub async fn pending_beta_text(
        &self,
        chat: i64,
        cur: &str,
        agent: u64,
    ) -> anyhow::Result<Option<Vec<String>>> {
        self.optional_string(
            "SELECT remaining FROM beta_pending_text WHERE chat_id=?1 AND revision=?2 AND agent=?3",
            params![chat, cur, i64::try_from(agent)?],
        )
        .await?
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(Into::into)
    }

    pub async fn save_pending_beta_text(
        &self,
        chat: i64,
        cur: &str,
        agent: u64,
        remaining: &[String],
    ) -> anyhow::Result<()> {
        self.connection()?.execute("INSERT INTO beta_pending_text(chat_id,revision,agent,remaining)
            VALUES(?1,?2,?3,?4) ON CONFLICT(chat_id,revision,agent) DO UPDATE SET remaining=excluded.remaining",
            params![chat,cur,i64::try_from(agent)?,serde_json::to_string(remaining)?]).await?;
        Ok(())
    }

    pub async fn mark_beta_delivered(
        &self,
        chat: i64,
        cur: &str,
        ids: &[u64],
    ) -> anyhow::Result<()> {
        let ids: Vec<i64> = ids
            .iter()
            .copied()
            .map(i64::try_from)
            .collect::<Result<_, _>>()?;
        let ids = serde_json::to_string(&ids)?;
        let connection = self.connection()?;
        let tx = connection.transaction().await?;
        tx.execute(
            "INSERT OR IGNORE INTO beta_deliveries(chat_id,revision,agent)
                    SELECT ?1, ?2, value FROM json_each(?3)",
            params![chat, cur, ids.as_str()],
        )
        .await?;
        tx.execute(
            "DELETE FROM beta_pending_text WHERE chat_id=?1 AND revision=?2
                    AND agent IN (SELECT value FROM json_each(?3))",
            params![chat, cur, ids.as_str()],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn beta_report(
        &self,
        prev: &str,
        cur: &str,
        id: u64,
        model: &str,
        prompt_version: &str,
    ) -> anyhow::Result<Option<String>> {
        self.optional_string(
            "SELECT report FROM beta_reports WHERE previous=?1 AND current=?2 AND agent=?3 AND model=?4 AND prompt_version=?5",
            params![prev, cur, i64::try_from(id)?, model, prompt_version],
        )
        .await
    }
    pub async fn save_beta_report(
        &self,
        prev: &str,
        cur: &str,
        id: u64,
        model: &str,
        prompt_version: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "INSERT OR IGNORE INTO beta_reports(previous,current,agent,model,prompt_version,report)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![prev, cur, i64::try_from(id)?, model, prompt_version, text],
            )
            .await?;
        Ok(())
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
        chat_id: i64,
        endgame_type: &str,
    ) -> anyhow::Result<Option<String>> {
        self.optional_string("SELECT r.season_start FROM users u JOIN endgame_results r ON r.uid=u.uid
            WHERE u.chat_id=?1 AND r.endgame_type=?2 AND r.season_start<>'' ORDER BY r.season_start DESC LIMIT 1",
            params![chat_id,endgame_type]).await
    }

    pub async fn checkpoint_candidates(
        &self,
        kind: &str,
        earliest: &str,
        latest: &str,
        label: &str,
    ) -> anyhow::Result<Vec<(i64, String)>> {
        let mut rows = self.connection()?.query(
            "SELECT DISTINCT u.chat_id, r.season_start FROM users u JOIN endgame_results r ON r.uid=u.uid
             WHERE r.endgame_type=?1 AND r.season_start BETWEEN ?2 AND ?3
             AND NOT EXISTS (SELECT 1 FROM checkpoints_posted p WHERE p.chat_id=u.chat_id
                 AND p.endgame_type=r.endgame_type AND p.season_start=r.season_start AND p.checkpoint=?4)
             ORDER BY u.chat_id, r.season_start", params![kind, earliest, latest, label]).await?;
        let mut candidates = Vec::new();
        while let Some(row) = rows.next().await? {
            candidates.push((row.get(0)?, row.get(1)?));
        }
        Ok(candidates)
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
            "SELECT name FROM avatar_cache WHERE uid=?1 AND avatar_id=?2 AND name<>''",
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
    /// Seed newly registered chats from the first successful poll, including an empty list.
    /// Only confirmed Telegram deliveries belong in promo_announcements.
    pub async fn pending_promo_announcements(
        &self,
        active_codes: &[&str],
    ) -> anyhow::Result<HashMap<String, Vec<i64>>> {
        let codes = serde_json::to_string(active_codes)?;
        let conn = self.connection()?;
        conn.execute(
            "INSERT OR IGNORE INTO promo_chat_baselines(chat_id, codes_json)
             SELECT DISTINCT chat_id, ?1 FROM users",
            [codes.as_str()],
        )
        .await?;
        let mut rows = conn
            .query(
                "SELECT active.value, b.chat_id
             FROM promo_chat_baselines b CROSS JOIN json_each(?1) active
             WHERE EXISTS (SELECT 1 FROM users WHERE chat_id = b.chat_id)
               AND NOT EXISTS (SELECT 1 FROM json_each(b.codes_json) initial
                               WHERE initial.value = active.value COLLATE NOCASE)
               AND NOT EXISTS (SELECT 1 FROM promo_announcements sent
                               WHERE sent.chat_id = b.chat_id AND sent.code = active.value)
             ORDER BY b.chat_id",
                [codes.as_str()],
            )
            .await?;
        let mut pending = HashMap::<String, Vec<i64>>::new();
        while let Some(row) = rows.next().await? {
            pending.entry(row.get(0)?).or_default().push(row.get(1)?);
        }
        Ok(pending)
    }

    pub async fn mark_promo_announced(&self, chat: i64, code: &str) -> anyhow::Result<()> {
        self.connection()?
            .execute(
                "INSERT OR IGNORE INTO promo_announcements(chat_id, code) VALUES (?1, ?2)",
                params![chat, code],
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
    async fn v19_retires_roster_collection_and_preserves_player_results() {
        for version in [17, 18] {
            let db = Db::new_test().await.unwrap();
            db.add_user(10, 1, "111", Some("Player")).await.unwrap();
            db.cache_avatars("111", &[(1, "Agent"), (2, "")])
                .await
                .unwrap();
            db.insert_result("111", "deadly_assault", "first", "end", 10, "{}")
                .await
                .unwrap();
            db.connection
                .execute_batch(
                    "CREATE VIEW played_agents AS SELECT uid FROM endgame_results;
                 CREATE TRIGGER collect_played_agents AFTER INSERT ON endgame_results BEGIN
                     INSERT OR IGNORE INTO avatar_cache(uid,avatar_id,name) VALUES(NEW.uid,42,'');
                 END;",
                )
                .await
                .unwrap();
            if version == 18 {
                db.connection.execute_batch(
                    "CREATE TABLE used_agents(uid TEXT, avatar_id INTEGER, PRIMARY KEY(uid,avatar_id));
                     CREATE TRIGGER cache_used_agent AFTER INSERT ON used_agents BEGIN
                         INSERT OR IGNORE INTO avatar_cache(uid,avatar_id,name) VALUES(NEW.uid,NEW.avatar_id,'');
                     END;"
                ).await.unwrap();
            }
            db.connection
                .execute(&format!("PRAGMA user_version={version}"), ())
                .await
                .unwrap();
            db.migrate().await.unwrap();

            let mut state = db
                .connection
                .query(
                    "SELECT user_version,(SELECT count(*) FROM sqlite_master WHERE name IN
                 ('played_agents','used_agents','collect_played_agents','cache_used_agent'))
                 FROM pragma_user_version",
                    (),
                )
                .await
                .unwrap();
            let row = state.next().await.unwrap().unwrap();
            assert_eq!(
                (row.get::<i64>(0).unwrap(), row.get::<i64>(1).unwrap()),
                (SCHEMA_VERSION, 0)
            );
            assert!(db.get_user(10, "111").await.unwrap().is_some());
            assert_eq!(
                db.resolve_avatar_name("111", 1).await.unwrap().as_deref(),
                Some("Agent")
            );
            assert_eq!(db.resolve_avatar_name("111", 2).await.unwrap(), None);
            db.insert_result("111", "deadly_assault", "second", "end", 20, "{}")
                .await
                .unwrap();
            assert_eq!(db.web_history("111", 10).await.unwrap().len(), 2);
        }
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
    async fn promo_baseline_follows_chat_registration_and_supports_an_empty_first_poll() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file:{}", dir.path().join("promos.db").display());
        let db = Db::connect(&url).await.unwrap();
        db.add_user(10, 1, "111", None).await.unwrap();
        assert!(
            db.pending_promo_announcements(&[])
                .await
                .unwrap()
                .is_empty()
        );
        db.add_user(10, 2, "222", None).await.unwrap();
        db.add_user(20, 3, "333", None).await.unwrap();
        let pending = db.pending_promo_announcements(&["A"]).await.unwrap();
        assert_eq!(pending["A"], [10]); // The new chat starts with A in its baseline.
        db.mark_promo_announced(10, "A").await.unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        assert!(
            db.pending_promo_announcements(&["a"])
                .await
                .unwrap()
                .is_empty()
        );
        db.remove_user(10, "111").await.unwrap();
        assert_eq!(
            db.pending_promo_announcements(&["B"]).await.unwrap()["B"],
            [10, 20]
        );
        db.remove_user(10, "222").await.unwrap();
        assert_eq!(
            db.pending_promo_announcements(&["B"]).await.unwrap()["B"],
            [20]
        );
        db.add_user(10, 2, "222", None).await.unwrap();
        assert_eq!(
            db.pending_promo_announcements(&["B"]).await.unwrap()["B"],
            [20]
        );
    }

    #[tokio::test]
    async fn text_delivery_resumes_after_migration_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file:{}", dir.path().join("report.db").display());
        let db = Db::connect(&url).await.unwrap();
        db.connection
            .execute_batch("DROP TABLE beta_pending_text; PRAGMA user_version=19;")
            .await
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO beta_deliveries(chat_id,revision,agent) VALUES(5,\'r2\',1)",
                (),
            )
            .await
            .unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        assert!(
            db.delivered_beta_agents(5, "r2")
                .await
                .unwrap()
                .contains(&1)
        );
        let tail = vec!["вторая часть".to_string(), "третья часть".to_string()];
        db.save_pending_beta_text(5, "r2", 2, &tail).await.unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        assert_eq!(
            db.pending_beta_text(5, "r2", 2).await.unwrap().unwrap(),
            tail
        );
        assert!(db.pending_beta_text(6, "r2", 2).await.unwrap().is_none());
        db.mark_beta_delivered(5, "r2", &[2, 3]).await.unwrap();
        assert!(db.pending_beta_text(5, "r2", 2).await.unwrap().is_none());
        assert_eq!(db.delivered_beta_agents(5, "r2").await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn beta_hotfix_resumes_at_the_unconfirmed_page_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file:{}", dir.path().join("beta.db").display());
        let db = Db::connect(&url).await.unwrap();
        db.add_user(10, 1, "1", None).await.unwrap();
        db.add_user(20, 2, "2", None).await.unwrap();
        for chat in [10, 20] {
            db.set_feature(chat, AiFeature::Diff, true).await.unwrap();
        }
        assert!(db.revision_cursors("beta", "r1").await.unwrap().is_empty());
        assert!(
            db.revision_cursors("release", "release_1")
                .await
                .unwrap()
                .is_empty()
        );
        db.mark_beta_delivered(10, "r2", &[1601, 1603])
            .await
            .unwrap();
        db.advance_revision(20, "beta", "r1", "r2").await.unwrap();
        db.save_beta_report("r1", "r2", 1601, "model", "v1", "reviewed text")
            .await
            .unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        assert_eq!(
            db.revision_cursors("beta", "r2").await.unwrap(),
            vec![(10, "r1".into())]
        );
        let delivered = db.delivered_beta_agents(10, "r2").await.unwrap();
        assert_eq!(delivered, std::collections::HashSet::from([1601, 1603]));
        assert!(db.delivered_beta_agents(20, "r2").await.unwrap().is_empty());
        assert_eq!(
            db.beta_report("r1", "r2", 1601, "model", "v1")
                .await
                .unwrap()
                .as_deref(),
            Some("reviewed text")
        );
        db.remove_user(10, "1").await.unwrap();
        db.add_user(10, 1, "3", None).await.unwrap();
        assert!(db.revision_cursors("beta", "r2").await.unwrap().is_empty());
        assert!(
            db.revision_cursors("release", "release_1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn model_override_survives_restart_and_can_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file:{}", dir.path().join("model.db").display());
        let db = Db::connect(&url).await.unwrap();
        assert_eq!(db.model_override().await.unwrap(), None);
        db.set_ai_model("gpt-6.2-sol@medium").await.unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        assert_eq!(
            db.model_override().await.unwrap().as_deref(),
            Some("gpt-6.2-sol@medium")
        );
        db.set_ai_model("gpt-6.2-sol@default").await.unwrap();
        assert_eq!(
            db.model_override().await.unwrap().as_deref(),
            Some("gpt-6.2-sol@default")
        );
        db.set_ai_model("auto").await.unwrap();
        assert_eq!(db.model_override().await.unwrap().as_deref(), Some("auto"));
    }

    #[tokio::test]
    async fn ai_cooldowns_are_atomic_global_expire_and_reset_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file:{}", dir.path().join("limits.db").display());
        let db = Db::connect(&url).await.unwrap();
        assert_eq!(
            db.admit_ai(20, None, "Waiting", "prompt", 99, false)
                .await
                .unwrap(),
            "busy"
        );
        assert_eq!(db.ai_users(1).await.unwrap()[0].decision, "busy");
        let ask = |id| db.admit_ai(id, Some("name"), "Player", "prompt", 99, true);
        let (a, b) = tokio::join!(ask(10), ask(10));
        let mut outcomes = [a.unwrap(), b.unwrap()];
        outcomes.sort();
        assert_eq!(outcomes, ["allowed", "limited"]);
        assert_eq!(ask(20).await.unwrap(), "allowed");
        db.ban_ai(10, true).await.unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        let ask = |id| db.admit_ai(id, None, "Player", "diff", 99, true);
        db.reset_ai_limit(10).await.unwrap();
        assert_eq!(ask(10).await.unwrap(), "banned");
        db.ban_ai(10, false).await.unwrap();
        assert_eq!(ask(10).await.unwrap(), "allowed");
        assert_eq!(ask(20).await.unwrap(), "limited");
        db.connection()
            .unwrap()
            .execute(
                "UPDATE ai_cooldowns SET until_unix=unixepoch() WHERE user_id=20",
                (),
            )
            .await
            .unwrap();
        assert_eq!(ask(20).await.unwrap(), "allowed");
        assert_eq!(ask(99).await.unwrap(), "admin");
        assert_eq!(ask(99).await.unwrap(), "admin");
        let users = db.ai_users(1).await.unwrap();
        assert_eq!(users.len(), 3);
        let user = users.iter().find(|u| u.id == 10).unwrap();
        assert_eq!(
            (user.requests, user.decision.as_str(), user.banned),
            (4, "allowed", false)
        );
    }

    #[tokio::test]
    async fn v15_migration_groups_replies_and_keeps_unrelated_sessions_separate() {
        let db = Db::new_test().await.unwrap();
        db.connection
            .execute_batch(
                "ALTER TABLE prompt_turns DROP COLUMN session_id;
            PRAGMA user_version=15;
            INSERT INTO prompt_turns(chat_id,topic_id,message_id,parent_id,question,answer) VALUES
                (1,5,10,NULL,'root','a'), (1,5,11,10,'reply','a'), (1,5,12,10,'sibling','a'),
                (2,5,10,NULL,'other chat','a'), (1,6,20,NULL,'other topic','a'),
                (1,5,31,30,'missing parent','a'), (1,5,32,30,'sibling','a');
            INSERT INTO prompt_parts(chat_id,message_id,first_id) VALUES(1,13,11);",
            )
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let id = async |chat, topic, message| {
            db.prompt_session(chat, Some(topic), message, None)
                .await
                .unwrap()
                .unwrap()
                .id
        };
        let root = id(1, 5, 10).await;
        assert_eq!(root.get_version_num(), 4);
        assert_eq!(root, id(1, 5, 12).await);
        assert_eq!(root, id(1, 5, 13).await);
        assert_ne!(root, id(2, 5, 10).await);
        assert_ne!(root, id(1, 6, 20).await);
        assert_ne!(root, id(1, 5, 31).await);
        assert_eq!(id(1, 5, 31).await, id(1, 5, 32).await);
    }

    #[tokio::test]
    async fn v14_text_sessions_accept_image_replies_after_migration() {
        let db = Db::new_test().await.unwrap();
        db.connection.execute_batch("ALTER TABLE prompt_turns DROP COLUMN session_id; ALTER TABLE prompt_turns DROP COLUMN image_ids;
            PRAGMA user_version=14;
            INSERT INTO prompt_turns(chat_id,message_id,question,answer) VALUES(1,1,'before','answer');").await.unwrap();
        db.migrate().await.unwrap();
        let root = db.prompt_session(1, None, 1, None).await.unwrap().unwrap();
        db.save_prompt_turn(
            1,
            None,
            2,
            Some(1),
            &DeliveredTurn {
                question: "after",
                session_id: root.id,
                answer: "new",
                image_ids: &["file".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let history = db.prompt_session(1, None, 2, None).await.unwrap().unwrap();
        assert!(history.turns[0].images.is_empty());
        assert_eq!(history.id, root.id);
        assert_eq!(history.turns[1].images, ["file"]);
    }

    #[tokio::test]
    async fn prompt_replies_recover_the_selected_branch_across_restart_and_expire_each_turn() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file:{}", dir.path().join("prompts.db").display());
        let topic = Some(7);
        let db = Db::connect(&url).await.unwrap();
        let session_id = PromptSession::default().id;
        assert_ne!(session_id, PromptSession::default().id);
        db.save_prompt_turn(
            -1,
            topic,
            100,
            None,
            &DeliveredTurn {
                question: "Q1",
                session_id,
                answer: "A1",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.save_prompt_part(-1, 100, 101).await.unwrap();
        db.save_prompt_turn(
            -1,
            topic,
            105,
            Some(100),
            &DeliveredTurn {
                question: "Q2",
                session_id,
                answer: "A2",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.save_prompt_turn(
            -1,
            topic,
            106,
            Some(100),
            &DeliveredTurn {
                question: "Q3",
                session_id,
                answer: "A3",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.save_prompt_part(-1, 106, 107).await.unwrap();
        drop(db);
        let db = Db::connect(&url).await.unwrap();
        for (id, expected) in [
            (101, vec![("Q1", "A1")]),
            (105, vec![("Q1", "A1"), ("Q2", "A2")]),
            (107, vec![("Q1", "A1"), ("Q3", "A3")]),
        ] {
            let session = db
                .prompt_session(-1, topic, id, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.id, session_id);
            assert_eq!(
                session
                    .turns
                    .iter()
                    .map(|t| (t.question.as_str(), t.answer.as_str()))
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert_eq!(
            db.prompt_session(-1, topic, 107, None)
                .await
                .unwrap()
                .unwrap()
                .last_answer,
            Some(106)
        );
        assert!(
            db.prompt_session(-2, topic, 101, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.prompt_session(-1, None, 101, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.prompt_session(-1, Some(8), 101, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.prompt_session(-1, topic, 999, None)
                .await
                .unwrap()
                .is_none()
        );
        db.connection()
            .unwrap()
            .execute(
                "UPDATE prompt_turns SET expires_at=unixepoch() WHERE message_id=100",
                (),
            )
            .await
            .unwrap();
        assert!(
            db.prompt_session(-1, topic, 101, None)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.prompt_session(-1, topic, 105, None)
                .await
                .unwrap()
                .unwrap()
                .turns
                .len(),
            1
        );
        db.expire_prompt_sessions().await.unwrap();
        assert_eq!(
            db.prompt_session(-1, topic, 107, None)
                .await
                .unwrap()
                .unwrap()
                .id,
            session_id
        );
        assert!(
            db.optional_string("SELECT question FROM prompt_turns WHERE message_id=100", ())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.optional_string(
                "SELECT CAST(first_id AS TEXT) FROM prompt_parts WHERE message_id=101",
                ()
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn branch_remembers_exact_season_and_attributes_constraints_to_the_speaker() {
        let db = Db::new_test().await.unwrap();
        let save = |season| rustverse_ai::TurnReferences {
            seasons: [rustverse_ai::SeasonReference {
                mode: "shiyu_defense".into(),
                season_id: season,
                requested: "next".into(),
                page: 0,
                path: None,
            }]
            .into(),
            ..Default::default()
        };
        db.save_prompt_turn(
            -1,
            Some(8),
            100,
            None,
            &DeliveredTurn {
                question: "Нет Кафки",
                answer: "Комната 2",
                author: Some(21),
                references: save(62053),
                session_id: Uuid::new_v4(),
                image_ids: &["first".into(), "second".into()],
            },
        )
        .await
        .unwrap();
        db.save_prompt_turn(
            -1,
            Some(8),
            101,
            Some(100),
            &DeliveredTurn {
                question: "А сейчас?",
                answer: "Новый сезон",
                author: Some(22),
                references: save(62054),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        db.save_prompt_turn(
            -1,
            Some(8),
            102,
            Some(100),
            &DeliveredTurn {
                question: "А в первой?",
                answer: "Тот же сезон",
                author: Some(21),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let branch = db
            .prompt_session(-1, Some(8), 102, Some(22))
            .await
            .unwrap()
            .unwrap();
        assert!(branch.turns[0].question.starts_with("[Другой участник]"));
        assert!(branch.turns[0].answer.contains("62053"));
        assert_eq!(branch.turns[0].images, ["first", "second"]);
        assert!(branch.turns[1].images.is_empty());
        assert!(branch.turns.iter().all(|t| !t.answer.contains("62054")));
        let other = db
            .prompt_session(-1, Some(8), 101, Some(22))
            .await
            .unwrap()
            .unwrap();
        assert!(other.turns[1].question.starts_with("[Текущий участник]"));
        assert!(other.turns[1].answer.contains("62054"));
    }

    #[tokio::test]
    async fn beta_report_cache_migrates_and_isolates_prompt_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("previous-schema.db");
        {
            let database = Builder::new_local(&path).build().await.unwrap();
            database.connect().unwrap().execute_batch(
                "CREATE TABLE users(chat_id INTEGER);
                 INSERT INTO users VALUES(77);
                 CREATE TABLE beta_cursors(chat_id INTEGER PRIMARY KEY,revision TEXT NOT NULL);
                 INSERT INTO beta_cursors VALUES(77,'old_beta');
                 CREATE TRIGGER reset_beta_cursor AFTER DELETE ON users BEGIN DELETE FROM beta_cursors WHERE chat_id=OLD.chat_id; END;
                 CREATE TABLE beta_reports(previous TEXT, current TEXT, agent INTEGER, report TEXT,
                     PRIMARY KEY(previous,current,agent));
                 INSERT INTO beta_reports VALUES('before','after',42,'unversioned');
                 PRAGMA user_version=7;"
            ).await.unwrap();
        }
        let db = Db::connect(&format!("file:{}", path.display()))
            .await
            .unwrap();
        assert_eq!(
            db.revision_cursors("beta", "new").await.unwrap(),
            vec![(77, "old_beta".into())]
        );
        assert!(
            db.revision_cursors("release", "first")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.beta_report("before", "after", 42, "sol", "v1")
                .await
                .unwrap()
                .is_none()
        );
        for (model, version, report) in [
            ("sol", "v1", "first"),
            ("sol", "v2", "second"),
            ("other", "v2", "third"),
        ] {
            db.save_beta_report("before", "after", 42, model, version, report)
                .await
                .unwrap();
        }
        for (model, version, report) in [
            ("", "", "unversioned"),
            ("sol", "v1", "first"),
            ("sol", "v2", "second"),
            ("other", "v2", "third"),
        ] {
            assert_eq!(
                db.beta_report("before", "after", 42, model, version)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(report)
            );
        }
        assert!(
            db.beta_report("after", "before", 42, "sol", "v2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.beta_report("before", "after", 43, "sol", "v2")
                .await
                .unwrap()
                .is_none()
        );
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
