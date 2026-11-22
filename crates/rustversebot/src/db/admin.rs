use super::Db;
use anyhow::Result;
use libsql::params;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AiFeature {
    Prompt,
    Diff,
}

#[derive(Serialize)]
pub(crate) struct ChatSettings {
    chat_id: i64,
    title: Option<String>,
    players: i64,
    prompt: bool,
    diff: bool,
}

#[derive(Serialize)]
pub(crate) struct SessionSummary {
    chat_id: i64,
    root: i32,
    topic: Option<i32>,
    title: String,
    turns: i64,
    updated_at: i64,
}

#[derive(Serialize)]
pub(crate) struct SessionTurn {
    message: i32,
    parent: Option<i32>,
    question: String,
    answer: String,
    created_at: i64,
}

impl Db {
    pub(crate) async fn observe_chat(&self, chat: i64, title: Option<&str>) -> Result<()> {
        self.connection.execute(
            "INSERT INTO chat_features(chat_id,title) VALUES(?1,?2)
             ON CONFLICT(chat_id) DO UPDATE SET title=excluded.title WHERE title IS NOT excluded.title",
            params![chat,title]).await?;
        Ok(())
    }

    pub(crate) async fn feature_enabled(&self, chat: i64, feature: AiFeature) -> Result<bool> {
        let sql = match feature {
            AiFeature::Prompt => "SELECT prompt FROM chat_features WHERE chat_id=?1",
            AiFeature::Diff => "SELECT diff FROM chat_features WHERE chat_id=?1",
        };
        let mut rows = self.connection.query(sql, [chat]).await?;
        Ok(rows
            .next()
            .await?
            .map(|row| row.get::<i64>(0))
            .transpose()?
            .unwrap_or(0)
            != 0)
    }

    pub(crate) async fn set_feature(
        &self,
        chat: i64,
        feature: AiFeature,
        enabled: bool,
    ) -> Result<()> {
        let sql = match feature {
            AiFeature::Prompt => {
                "INSERT INTO chat_features(chat_id,prompt) VALUES(?1,?2)
                ON CONFLICT(chat_id) DO UPDATE SET prompt=excluded.prompt"
            }
            AiFeature::Diff => {
                "INSERT INTO chat_features(chat_id,diff) VALUES(?1,?2)
                ON CONFLICT(chat_id) DO UPDATE SET diff=excluded.diff"
            }
        };
        self.connection.execute(sql, params![chat, enabled]).await?;
        Ok(())
    }

    pub(crate) async fn has_diff_subscribers(&self) -> Result<bool> {
        let mut rows = self
            .connection
            .query(
                "SELECT 1 FROM users JOIN chat_features USING(chat_id) WHERE diff=1 LIMIT 1",
                (),
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    pub(crate) async fn admin_chats(&self) -> Result<Vec<ChatSettings>> {
        let mut rows = self.connection.query(
            "WITH players AS (SELECT chat_id,count(*) AS n FROM users GROUP BY chat_id),
             chats AS (SELECT chat_id FROM players UNION SELECT chat_id FROM chat_features
                       UNION SELECT chat_id FROM prompt_turns WHERE expires_at>unixepoch())
             SELECT chat_id,coalesce(n,0),coalesce(prompt,0),coalesce(diff,0),title FROM chats
             LEFT JOIN players USING(chat_id) LEFT JOIN chat_features USING(chat_id) ORDER BY chat_id", ()
        ).await?;
        let mut chats = Vec::new();
        while let Some(r) = rows.next().await? {
            chats.push(ChatSettings {
                chat_id: r.get(0)?,
                players: r.get(1)?,
                prompt: r.get::<i64>(2)? != 0,
                diff: r.get::<i64>(3)? != 0,
                title: r.get(4)?,
            });
        }
        Ok(chats)
    }

    pub(crate) async fn admin_sessions(
        &self,
        chat: Option<i64>,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<SessionSummary>> {
        // Once a parent expires, its surviving children become separate visible roots.
        let mut rows = self.connection.query(
            "WITH RECURSIVE tree(chat_id,root,message_id,topic_id,expiry) AS (
                SELECT t.chat_id,t.message_id,t.message_id,t.topic_id,t.expires_at FROM prompt_turns t
                WHERE t.expires_at>unixepoch() AND (?1 IS NULL OR t.chat_id=?1)
                  AND NOT EXISTS (SELECT 1 FROM prompt_turns p WHERE p.chat_id=t.chat_id
                    AND p.topic_id IS t.topic_id AND p.message_id=t.parent_id AND p.expires_at>unixepoch())
                UNION ALL
                SELECT c.chat_id,t.root,c.message_id,c.topic_id,c.expires_at FROM tree t
                JOIN prompt_turns c ON c.chat_id=t.chat_id AND c.parent_id=t.message_id
                  AND c.topic_id IS t.topic_id WHERE c.expires_at>unixepoch()
             ) SELECT t.chat_id,t.root,r.topic_id,substr(r.question,1,120),count(*),max(t.expiry)-86400 AS updated
             FROM tree t JOIN prompt_turns r ON r.chat_id=t.chat_id AND r.message_id=t.root
             GROUP BY t.chat_id,t.root ORDER BY updated DESC,t.chat_id,t.root DESC LIMIT ?2 OFFSET ?3",
            params![chat,limit,offset]).await?;
        let mut sessions = Vec::new();
        while let Some(r) = rows.next().await? {
            sessions.push(SessionSummary {
                chat_id: r.get(0)?,
                root: r.get(1)?,
                topic: r.get(2)?,
                title: r.get(3)?,
                turns: r.get(4)?,
                updated_at: r.get(5)?,
            });
        }
        Ok(sessions)
    }

    pub(crate) async fn admin_turns(&self, chat: i64, root: i32) -> Result<Vec<SessionTurn>> {
        let mut rows = self.connection.query(
            "WITH RECURSIVE thread(message_id,topic_id) AS (
                SELECT message_id,topic_id FROM prompt_turns WHERE chat_id=?1 AND message_id=?2 AND expires_at>unixepoch()
                UNION ALL SELECT c.message_id,c.topic_id FROM thread t JOIN prompt_turns c
                ON c.chat_id=?1 AND c.parent_id=t.message_id AND c.topic_id IS t.topic_id WHERE c.expires_at>unixepoch()
             ) SELECT p.message_id,p.parent_id,p.question,p.answer,p.expires_at-86400 FROM thread t
             JOIN prompt_turns p ON p.chat_id=?1 AND p.message_id=t.message_id ORDER BY p.message_id",
            params![chat,root]).await?;
        let mut turns = Vec::new();
        while let Some(r) = rows.next().await? {
            turns.push(SessionTurn {
                message: r.get(0)?,
                parent: r.get(1)?,
                question: r.get(2)?,
                answer: r.get(3)?,
                created_at: r.get(4)?,
            });
        }
        Ok(turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn v13_upgrade_preserves_registered_chats_and_new_chats_opt_in() {
        let db = Db::new_test().await.unwrap();
        db.connection
            .execute_batch(
                "DROP TABLE chat_features; DROP INDEX prompt_turns_parent;
                 ALTER TABLE prompt_turns DROP COLUMN image_ids; PRAGMA user_version=13;",
            )
            .await
            .unwrap();
        db.add_user(-10, 1, "100", None).await.unwrap();
        db.migrate().await.unwrap();
        assert!(db.feature_enabled(-10, AiFeature::Prompt).await.unwrap());
        assert!(db.feature_enabled(-10, AiFeature::Diff).await.unwrap());
        db.add_user(-20, 2, "200", None).await.unwrap();
        db.observe_chat(-20, Some("Test group")).await.unwrap();
        assert!(!db.feature_enabled(-20, AiFeature::Prompt).await.unwrap());
        assert!(db.revision_cursors("beta", "r1").await.unwrap().is_empty());
        assert_eq!(
            db.revision_cursors("beta", "r2").await.unwrap(),
            [(-10, "r1".into())]
        );
        db.set_feature(-10, AiFeature::Diff, false).await.unwrap();
        assert!(!db.has_diff_subscribers().await.unwrap());
        assert!(db.feature_enabled(-10, AiFeature::Prompt).await.unwrap());
        db.set_feature(-10, AiFeature::Diff, true).await.unwrap();
        assert!(db.revision_cursors("beta", "r3").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn expired_parent_removes_its_text_and_promotes_surviving_replies() {
        let db = Db::new_test().await.unwrap();
        db.connection.execute_batch(
            "INSERT INTO prompt_turns(chat_id,message_id,parent_id,question,answer,expires_at) VALUES
             (-1,1,NULL,'expired','expired',unixepoch()), (-1,2,1,'active','answer',unixepoch()+60);"
        ).await.unwrap();
        let sessions = db.admin_sessions(None, 0, 10).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].root, 2);
        assert!(db.admin_turns(-1, 1).await.unwrap().is_empty());
        assert_eq!(db.admin_turns(-1, 2).await.unwrap()[0].question, "active");
    }
}
