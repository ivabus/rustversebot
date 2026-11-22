use super::{Db, UserRow};
use anyhow::{Result, ensure};
use libsql::params;
use rustverse::models::zzz::{ZZZAvatar, ZZZDeadlyAssault, ZZZShiyuDefense};
use rustverse_ai::players::{ChatPlayerData, PlayerQuery, RunMode, RunQuery};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};

pub(crate) struct ChatPlayers {
    pub db: Db,
    pub chat: i64,
    pub author: i64,
    pub display_name: String,
}

impl ChatPlayers {
    async fn select(&self, player: Option<&str>) -> Result<Result<UserRow, Value>> {
        let mut matches = match player.map(str::trim) {
            None => {
                self.db
                    .users(
                        "SELECT chat_id,telegram_user_id,uid,nickname FROM users
                 WHERE chat_id=?1 AND telegram_user_id=?2 ORDER BY uid",
                        params![self.chat, self.author],
                    )
                    .await?
            }
            Some("") => Vec::new(),
            Some(uid) if uid.bytes().all(|b| b.is_ascii_digit()) => self
                .db
                .get_user(self.chat, uid)
                .await?
                .into_iter()
                .collect(),
            Some(nickname) => {
                // SQLite NOCASE folds ASCII only. Game nicknames can also be Cyrillic.
                let key = nickname.to_lowercase();
                self.db
                    .get_users_by_chat(self.chat)
                    .await?
                    .into_iter()
                    .filter(|u| u.nickname.as_ref().is_some_and(|n| n.to_lowercase() == key))
                    .collect()
            }
        };
        if matches.len() == 1 {
            return Ok(Ok(matches.pop().unwrap()));
        }
        let accounts: Vec<_> = matches.iter().map(|u| self.account(u)).collect();
        Ok(Err(
            json!({"requester":self.display_name,"accounts":accounts,
            "status": if accounts.is_empty() {"not_registered_in_chat"} else {"choose_account"}}),
        ))
    }

    fn account(&self, u: &UserRow) -> Value {
        json!({"uid":u.uid,"nickname":u.nickname,"is_requester":u.telegram_user_id == self.author})
    }
}

#[async_trait::async_trait]
impl ChatPlayerData for ChatPlayers {
    async fn profile(&self, query: PlayerQuery) -> Result<Value> {
        let account = match self.select(query.player.as_deref()).await? {
            Ok(u) => u,
            Err(status) => return Ok(status),
        };
        let mut rows = self.db.connection()?.query(
            "SELECT a.avatar_id,NULLIF(a.name,'') FROM avatar_cache a JOIN users u USING(uid)
             WHERE u.chat_id=?1 AND u.uid=?2 AND u.telegram_user_id=?3 AND a.avatar_id>0 ORDER BY a.avatar_id",
            params![self.chat, account.uid.clone(), account.telegram_user_id],
        ).await?;
        let mut agents = Vec::new();
        while let Some(row) = rows.next().await? {
            agents.push(json!({"id":row.get::<i64>(0)?,"name":row.get::<Option<String>>(1)?}));
        }
        Ok(
            json!({"requester":self.display_name,"account":self.account(&account),"known_agents":agents}),
        )
    }

    async fn runs(&self, query: RunQuery) -> Result<Value> {
        let account = match self.select(query.player.as_deref()).await? {
            Ok(u) => u,
            Err(status) => return Ok(status),
        };
        let before = query.before_id.map(i64::try_from).transpose()?;
        ensure!(before.is_none_or(|v| v > 0), "Invalid history cursor");
        // Pick season representatives before applying the cursor so an older page
        // cannot reintroduce a season that the caller has already seen.
        let mut rows = self.db.connection()?.query(
            "SELECT r.id,r.season_start,r.fetched_at,r.data_json FROM endgame_results r JOIN users u USING(uid)
             WHERE u.chat_id=?1 AND u.uid=?2 AND u.telegram_user_id=?3 AND r.endgame_type=?4
               AND (?5 IS NULL OR r.id<?5)
               AND (r.season_start=?6 OR (?6 IS NULL AND r.id IN
                 (SELECT MAX(id) FROM endgame_results WHERE uid=?2 AND endgame_type=?4 GROUP BY season_start)))
             ORDER BY r.id ASC",
            params![self.chat, account.uid.clone(), account.telegram_user_id, query.mode.as_str(), before, query.season_start.as_deref()],
        ).await?;
        const PAGE_SIZE: usize = 3;
        let mut page = VecDeque::with_capacity(PAGE_SIZE);
        let mut seen = HashSet::new();
        let mut has_older = false;
        while let Some(row) = rows.next().await? {
            let Some(detail) = run_detail(query.mode, &row.get::<String>(3)?) else {
                continue;
            };
            // The first observation is the stable ID for a run. A later poll of
            // the same fight cannot move it across the caller's pagination cursor.
            if query.season_start.is_some() && seen.contains(&detail) {
                continue;
            }
            let result = json!({
                "season_start": row.get::<String>(1)?,
                "observed_at_utc": row.get::<String>(2)?,
                "result": detail,
            });
            if query.season_start.is_some() {
                seen.insert(detail);
            }
            if page.len() == PAGE_SIZE {
                page.pop_back();
                has_older = true;
            }
            page.push_front((row.get::<i64>(0)?, result));
        }
        let next_before_id = page.back().filter(|_| has_older).map(|(id, _)| *id);
        let runs: Vec<_> = page.into_iter().map(|(_, result)| result).collect();
        Ok(
            json!({"account":self.account(&account),"mode":query.mode.as_str(),
            "runs":runs,"next_before_id":next_before_id}),
        )
    }
}

fn team(agents: &[ZZZAvatar]) -> Vec<Value> {
    agents
        .iter()
        .map(|a| json!({"id":a.id,"level":a.level,"rank":a.rank}))
        .collect()
}

/// Project only combat observations, excluding profile fields and asset URLs.
fn run_detail(mode: RunMode, raw: &str) -> Option<Value> {
    match mode {
        RunMode::DeadlyAssault => {
            let run: ZZZDeadlyAssault = serde_json::from_str(raw).ok()?;
            if run.has_data == Some(false)
                || (run.list.is_empty() && run.hard_list.is_empty() && run.total_score.is_none())
            {
                return None;
            }
            let mut rooms = Vec::new();
            for (floor, list) in [("normal", &run.list), ("hard", &run.hard_list)] {
                for (index, room) in list.iter().enumerate() {
                    rooms.push(json!({
                        "floor": floor, "room": index + 1,
                        "score": room.score, "stars": room.star,
                        "agents": team(&room.avatar_list),
                        "bangboo_id": room.buddy.as_ref().map(|b| b.id),
                        "bosses": room.boss.iter().map(|b| &b.name).collect::<Vec<_>>(),
                        "buffs": room.buffer.iter().filter_map(|b| b.title.as_ref()).collect::<Vec<_>>(),
                        "challenge_time": room.challenge_time.as_ref().map(ToString::to_string),
                    }));
                }
            }
            Some(
                json!({"zone_id":run.zone_id,"normal_score":run.total_score,"hard_score":run.hard_score(),
                "normal_stars":run.total_star,"rooms":rooms}),
            )
        }
        RunMode::ShiyuDefense => {
            let run: ZZZShiyuDefense = serde_json::from_str(raw).ok()?;
            let mut layers = serde_json::Map::new();
            for (key, layer) in run.layers {
                let rooms: Vec<_> = layer
                    .layer_challenge_info_list
                    .iter()
                    .enumerate()
                    .map(|(index, room)| {
                        json!({
                            "room": index + 1, "layer_id": room.layer_id,
                            "score": room.score, "max_score": room.max_score, "rating": room.rating,
                            "agents": team(&room.avatar_list),
                            "bangboo_id": room.buddy.as_ref().map(|b| b.id),
                            "buff": room.buffer.as_ref().and_then(|b| b.title.as_ref()),
                            "challenge_time": room.challenge_time.as_ref().map(ToString::to_string),
                        })
                    })
                    .collect();
                layers.insert(
                    key,
                    json!({
                        "rating": layer.rating,
                        "buff": layer.buffer.and_then(|b| b.title),
                        "challenge_time": layer.challenge_time.as_ref().map(ToString::to_string),
                        "rooms": rooms,
                    }),
                );
            }
            if layers.is_empty() && run.brief.is_none() {
                return None;
            }
            Some(
                json!({"zone_id":run.zone_id,"score":run.brief.as_ref().and_then(|b|b.score),
                "rating":run.brief.as_ref().and_then(|b|b.rating.as_ref()),"layers":layers}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_scope_both_selectors_and_page_distinct_observations() {
        let db = Db::new_test().await.unwrap();
        db.add_user(7, 1, "101", Some("Автор")).await.unwrap();
        db.add_user(7, 2, "102", Some("Маро")).await.unwrap();
        db.add_user(8, 2, "103", Some("Другой")).await.unwrap();
        let tools = ChatPlayers {
            db: db.clone(),
            chat: 7,
            author: 1,
            display_name: "Аня".into(),
        };
        for (uid, season, score, rank) in [
            ("102", "old", 1, 1),
            ("102", "new", 2, 2),
            ("102", "new", 3, 2),
            ("102", "new", 4, 2),
            ("102", "new", 5, 2),
            ("102", "new", 5, 3),
            ("102", "new", 3, 4),
            ("103", "new", 100, 1),
        ] {
            let raw = json!({"rank_percent":rank,"nick_name":"private extra","total_score":score,
                "list":[{"score":score}],"hard_list":[{"score":9,"buffer":[{"title":"Buff"}]}]})
            .to_string();
            db.insert_result(uid, "deadly_assault", season, "end", score, &raw)
                .await
                .unwrap();
        }
        let lookup = async |player: Option<&str>, season: Option<&str>, before_id| {
            tools
                .runs(RunQuery {
                    player: player.map(str::to_owned),
                    mode: RunMode::DeadlyAssault,
                    season_start: season.map(str::to_owned),
                    before_id,
                })
                .await
                .unwrap()
        };
        assert_eq!(lookup(None, None, None).await["runs"], json!([]));
        for foreign in ["103", "Другой"] {
            assert_eq!(
                lookup(Some(foreign), None, None).await["status"],
                "not_registered_in_chat"
            );
        }
        let current = lookup(Some("маро"), None, None).await;
        assert_eq!(current["account"]["uid"], "102");
        assert_eq!(current["runs"].as_array().unwrap().len(), 2);
        assert_eq!(current["runs"][1]["season_start"], "old");
        assert_eq!(current["runs"][0]["result"]["normal_score"], 3);
        let page = lookup(Some("102"), Some("new"), None).await;
        let scores: Vec<_> = page["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["result"]["normal_score"].as_i64().unwrap())
            .collect();
        assert_eq!(scores, [5, 4, 3]);
        assert_eq!(
            page["runs"][0]["result"]["rooms"][1]["buffs"],
            json!(["Buff"])
        );
        assert_eq!(page["runs"][0]["result"]["rooms"][1]["floor"], "hard");
        assert!(!page.to_string().contains("private extra"));
        // An old result polled again between requests must still appear on its own page.
        db.insert_result("102", "deadly_assault", "new", "end", 2,
            r#"{"rank_percent":9,"total_score":2,"list":[{"score":2}],"hard_list":[{"score":9,"buffer":[{"title":"Buff"}]}]}"#)
            .await.unwrap();
        let next = lookup(
            Some("102"),
            Some("new"),
            Some(page["next_before_id"].as_u64().unwrap()),
        )
        .await;
        assert_eq!(next["runs"].as_array().unwrap().len(), 1);
        assert_eq!(next["runs"][0]["result"]["normal_score"], 2);
        assert!(next["next_before_id"].is_null());
        db.add_user(7, 3, "104", Some("МАРО")).await.unwrap();
        let ambiguous = lookup(Some("маро"), None, None).await;
        assert_eq!(ambiguous["status"], "choose_account");
        assert!(ambiguous.get("runs").is_none());
        db.remove_user(7, "102").await.unwrap();
        assert_eq!(
            lookup(Some("102"), None, None).await["status"],
            "not_registered_in_chat"
        );
    }

    #[tokio::test]
    async fn shiyu_history_recognizes_repeated_fights_and_equal_score_replays() {
        let db = Db::new_test().await.unwrap();
        db.add_user(7, 1, "101", None).await.unwrap();
        let tools = ChatPlayers {
            db,
            chat: 7,
            author: 1,
            display_name: "Игрок".into(),
        };
        for (minute, agent, percentile) in [(1, 1011, 8), (2, 1011, 7), (2, 1021, 6), (1, 1011, 5)]
        {
            let raw = json!({
                "brief": {"score": 100, "rank_percent": percentile},
                "fitfh_layer_detail": {"layer_challenge_info_list": [{
                    "score": 100, "challenge_time": {"year":2026,"month":11,"day":1,"hour":9,"minute":minute,"second":0},
                    "avatar_list": [{"id":agent,"rank":0,"level":60,"rarity":"A","element_type":1,
                        "avatar_profession":1,"sub_element_type":0,"role_square_url":"icon"}]
                }]}
            });
            tools
                .db
                .insert_result(
                    "101",
                    "shiyu_defense",
                    "season",
                    "end",
                    100,
                    &raw.to_string(),
                )
                .await
                .unwrap();
        }
        let history = tools
            .runs(RunQuery {
                player: None,
                mode: RunMode::ShiyuDefense,
                season_start: Some("season".into()),
                before_id: None,
            })
            .await
            .unwrap();
        let fights: Vec<_> = history["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| {
                let room = &run["result"]["layers"]["fitfh_layer_detail"]["rooms"][0];
                (
                    room["challenge_time"].as_str().unwrap(),
                    room["agents"][0]["id"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            fights,
            [
                ("2026-11-01 09:02:00", 1021),
                ("2026-11-01 09:02:00", 1011),
                ("2026-11-01 09:01:00", 1011)
            ]
        );
        assert!(history["next_before_id"].is_null());
    }

    #[test]
    fn shiyu_snapshot_exposes_combat_fields_for_all_layers() {
        let raw = json!({"zone_id":62053,"brief":{"rating":"S","score":111},
            "fourth_layer_detail":{"buffer":{"title":"Buff"}},
            "fitfh_layer_detail":{"layer_challenge_info_list":[{"score":111,"layer_id":5,
                "avatar_list":[{"id":1011,"rank":3,"level":60,"rarity":"A","element_type":1,
                    "avatar_profession":1,"sub_element_type":0,"role_square_url":"private URL"}]}]}});
        let detail = run_detail(RunMode::ShiyuDefense, &raw.to_string()).unwrap();
        assert_eq!(detail["score"], 111);
        assert_eq!(detail["layers"]["fourth_layer_detail"]["buff"], "Buff");
        assert_eq!(
            detail["layers"]["fitfh_layer_detail"]["rooms"][0]["agents"],
            json!([{"id":1011,"rank":3,"level":60}])
        );
        assert!(!detail.to_string().contains("private URL"));
    }
}
