use serde::Deserialize;

/// Top-level configuration for recurring endgame events.
/// Expected file: `config.toml` in the working directory (or alongside the binary).
#[derive(Debug, Clone, Deserialize)]
pub struct RecurringConfig {
    #[serde(rename = "recurring")]
    pub events: Vec<RecurringEvent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecurringEvent {
    /// Display name: "Deadly Assault", "Shiyu Defense"
    pub name: String,
    /// Interval in days (both modes are 14)
    pub every: u32,
    /// First occurrence date: "2026-07-03" or "2026-07-10"
    pub from: String,
    /// Labels (for future use: grouping, filtering)
    pub labels: Vec<String>,
}

impl RecurringEvent {
    /// Map the display name to our internal endgame type identifier.
    pub fn endgame_type(&self) -> &str {
        match self.name.as_str() {
            "Deadly Assault" => "deadly_assault",
            "Shiyu Defense" => "shiyu_defense",
            other => other,
        }
    }

    /// Parse the `from` date as a chrono::NaiveDate.
    pub fn start_date(&self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(&self.from, "%Y-%m-%d").ok()
    }

    /// How many days after season start we consider the season "ended" (итог).
    pub fn season_duration_days(&self) -> u32 {
        self.every // 14 days
    }
}

/// Load config from the given path.
pub fn load_config(path: &str) -> anyhow::Result<RecurringConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: RecurringConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Default config used when no file is present.
pub fn default_config() -> RecurringConfig {
    RecurringConfig {
        events: vec![
            RecurringEvent {
                name: "Deadly Assault".into(),
                every: 14,
                from: "2026-07-03".into(),
                labels: vec!["ZZZ".into(), "Endgame".into()],
            },
            RecurringEvent {
                name: "Shiyu Defense".into(),
                every: 14,
                from: "2026-07-10".into(),
                labels: vec!["ZZZ".into(), "Endgame".into()],
            },
        ],
    }
}
