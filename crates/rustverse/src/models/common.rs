use serde::{Deserialize, Serialize};

/// Generic API response wrapper as described in the docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub retcode: i64,
    pub message: String,
    pub data: Option<T>,
}

/// Date components in UTC+8 (used by ZZZ and HSR).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateComponents {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl std::fmt::Display for DateComponents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second,
        )
    }
}

/// Game record card returned when listing games linked to HoYoLAB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRecordCard {
    pub has_role: bool,
    pub game_id: i64,
    pub game_role_id: String,
    pub nickname: String,
    pub region: String,
    pub level: i64,
    /// Human-readable region name (e.g. "Europe").
    #[serde(default)]
    pub region_name: Option<String>,
    /// Human-readable game name (e.g. "Zenless Zone Zero").
    #[serde(default)]
    pub game_name: Option<String>,
    /// Whether the battle chronicle is public.
    #[serde(default)]
    pub is_public: Option<bool>,
    /// Quick stats shown on the card (days active, achievements, etc.).
    #[serde(default)]
    pub data: Vec<GameRecordCardStat>,
    /// Game logo URL.
    #[serde(default)]
    pub logo: Option<String>,
}

/// A single stat row on a game record card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRecordCardStat {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub stat_type: Option<i64>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRecordCardResponse {
    pub list: Vec<GameRecordCard>,
}
