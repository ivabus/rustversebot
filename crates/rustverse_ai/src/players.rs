use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::kit::ToolSpec;

/// Implemented by the Telegram adapter with the chat and sender fixed for this turn.
#[async_trait::async_trait]
pub trait ChatPlayerData: Send + Sync {
    async fn profile(&self, query: PlayerQuery) -> Result<Value>;
    async fn runs(&self, query: RunQuery) -> Result<Value>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerQuery {
    pub player: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunQuery {
    pub player: Option<String>,
    pub mode: RunMode,
    pub season_start: Option<String>,
    pub before_id: Option<u64>,
}

#[derive(Copy, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    DeadlyAssault,
    ShiyuDefense,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeadlyAssault => "deadly_assault",
            Self::ShiyuDefense => "shiyu_defense",
        }
    }
}

pub(crate) fn definitions() -> [ToolSpec; 2] {
    let player = json!({"type":["string","null"],"description":"Null for the current speaker, otherwise the requested player's game nickname or UID registered in this chat."});
    [
        ToolSpec {
            name: "get_player_profile",
            description: "Only for an explicit request to build teams for a player in this chat or analyze a failed run. Identify the speaker and selected account and read its known agents. Otherwise never call.",
            parameters: json!({"type":"object","properties":{"player":player},"required":["player"],"additionalProperties":false}),
        },
        ToolSpec {
            name: "get_player_runs",
            description: "Only for an explicit request to build teams for a player in this chat or analyze a failed run. With no season, returns the latest recorded result per season. For a selected season, returns distinct combat results newest first by first observation. Repeated polls and leaderboard changes are ignored, including across pages. Otherwise never call.",
            parameters: json!({"type":"object","properties":{"player":player,
                "mode":{"type":"string","enum":["deadly_assault","shiyu_defense"]},
                "season_start":{"type":["string","null"],"description":"Exact season_start from a result, or null."},
                "before_id":{"type":["integer","null"],"minimum":1,"description":"Start with null. Use next_before_id for the same query."}
            },"required":["player","mode","season_start","before_id"],"additionalProperties":false}),
        },
    ]
}
