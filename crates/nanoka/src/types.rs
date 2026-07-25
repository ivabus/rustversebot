//! Type definitions for the nanoka.cc ZZZ Shiyu Defence / Deadly Assault API.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// --------------------------------------------------------------------
//  Endgame type
// --------------------------------------------------------------------

/// The game mode a season belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndgameType {
    /// Shiyu Defence — Critical Node, Stable Node, Disputed Node, Ambush Node.
    /// IDs start with `61` or `62` (e.g. `62053`, `620561`).
    ShiyuDefence,
    /// Deadly Assault — boss-rush "Trial" seasons.
    /// IDs start with `69` (e.g. `69041`, `690441`).
    DeadlyAssault,
}

impl fmt::Display for EndgameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndgameType::ShiyuDefence => write!(f, "Shiyu Defence"),
            EndgameType::DeadlyAssault => write!(f, "Deadly Assault"),
        }
    }
}

impl EndgameType {
    /// Detect the endgame type from a numeric season ID.
    ///
    /// Uses the leading two digits so both 5-digit (`62053`, `69041`) and
    /// 6-digit (`620561`, `690441`) IDs resolve correctly:
    /// `61…`/`62…` → Shiyu, `69…` → Deadly Assault.
    pub fn from_id(id: u64) -> Option<Self> {
        let s = id.to_string();
        match s.get(..2)? {
            "61" | "62" => Some(EndgameType::ShiyuDefence),
            "69" => Some(EndgameType::DeadlyAssault),
            _ => None,
        }
    }
}

// --------------------------------------------------------------------
//  Season index
// --------------------------------------------------------------------

/// Metadata for a single Shiyu Defence / Deadly Assault season.
///
/// Returned by [`NanokaClient::get_seasons`](crate::NanokaClient::get_seasons)
/// and [`NanokaClient::get_boss_seasons`](crate::NanokaClient::get_boss_seasons)
/// as a map keyed by the string season ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SeasonMeta {
    /// Sort priority (1 = Critical Node, 2 = Stable, 3 = Disputed, 4 = Ambush).
    pub sort: u32,
    /// English name.
    pub en: String,
    /// Korean name.
    #[serde(default)]
    pub ko: String,
    /// Chinese name.
    #[serde(default)]
    pub zh: String,
    /// Japanese name.
    #[serde(default)]
    pub ja: String,
    /// When the season begins (UTC+8, format: `"YYYY-MM-DD HH:MM:SS"`).
    #[serde(default)]
    pub begin: Option<String>,
    /// When the season ends (UTC+8).
    #[serde(default)]
    pub end: Option<String>,
    /// Live / re-run begin time.
    #[serde(default)]
    pub live_begin: Option<String>,
    /// Live / re-run end time.
    #[serde(default)]
    pub live_end: Option<String>,
}

impl SeasonMeta {
    /// Detect the endgame type from the index `sort` field.
    ///
    /// `sort == 9` → Deadly Assault; `sort` in `1..=4` → Shiyu Defence.
    /// Prefer [`EndgameType::from_id`] when the numeric season ID is available.
    pub fn endgame_type(&self) -> Option<EndgameType> {
        if self.sort == 9 {
            return Some(EndgameType::DeadlyAssault);
        }
        if (1..=4).contains(&self.sort) {
            return Some(EndgameType::ShiyuDefence);
        }
        None
    }
}

// --------------------------------------------------------------------
//  Season detail
// --------------------------------------------------------------------

/// Full detail for a single Shiyu Defence / Deadly Assault season.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SeasonDetail {
    /// Numeric season ID (e.g. 62053).
    pub id: u64,
    /// Season name in the configured language (e.g. "Critical Node").
    pub name: String,
    /// Priority / sort order.
    pub priority: u32,
    /// Map of zone ID → zone data.
    pub zone: HashMap<String, Zone>,
    /// Season begin time (UTC+8).
    #[serde(default)]
    pub begin_time: Option<String>,
    /// Season end time (UTC+8).
    #[serde(default)]
    pub end_time: Option<String>,
}

impl SeasonDetail {
    /// Detect the endgame type for this season detail.
    pub fn endgame_type(&self) -> Option<EndgameType> {
        EndgameType::from_id(self.id)
    }
}

/// A zone (stage) within a season.
///
/// Each zone has rooms that contain monsters, buffs and rank goals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Zone {
    /// Human-readable zone name.
    pub name: String,
    /// Stage number (1-7 typically).
    pub stage_num: u32,
    /// Base monster level for this zone.
    pub monster_level: u32,
    /// Global buff effects applied to this zone.
    #[serde(default)]
    pub layer_buff: HashMap<String, Buff>,
    /// Selectable buffs (Deadly Assault only — player chooses one before fight).
    #[serde(default)]
    pub selectable_buff: HashMap<String, Buff>,
    /// Child zone IDs (used for Deadly Assault-style branching).
    #[serde(default)]
    pub child: Vec<u64>,
    /// Rooms in this zone, keyed by room ID.
    #[serde(default)]
    pub layer_room: HashMap<String, Room>,
    /// Type of goal (0 = timer, 2 = score, 3 = combined).
    #[serde(default)]
    pub goal_type: u32,
    /// SS-rank goal threshold (0 means not applicable).
    #[serde(default)]
    pub ss_rank_goal: u64,
    /// S-rank goal threshold (seconds for timer, points for score).
    #[serde(default)]
    pub s_rank_goal: u64,
    /// A-rank goal threshold.
    #[serde(default)]
    pub a_rank_goal: u64,
    /// B-rank goal threshold.
    #[serde(default)]
    pub b_rank_goal: u64,
}

/// A combat room within a zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Room {
    /// Icon path for the main monster / boss of this room.
    #[serde(default)]
    pub monster_icon: String,
    /// Monsters appearing in this room, keyed by instance ID.
    #[serde(default)]
    pub monster_list: HashMap<String, Monster>,
    /// Element weakness map. Keys are numeric element IDs, values are names.
    ///
    /// Known IDs: 200 = Physical, 201 = Fire, 202 = Ice, 203 = Electric,
    /// 204 = Wind, 205 = Ether.
    #[serde(default)]
    pub monster_weakness: HashMap<String, String>,
    /// Number of enemy waves in this room.
    #[serde(default)]
    pub waves_num: u32,
}

/// A monster / enemy instance in a room.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Monster {
    /// Numeric monster type ID.
    pub id: u64,
    /// Localised monster name.
    pub name: String,
    /// Relative image path (e.g. `"UI/Sprite/.../Monster_Banyrek.png"`).
    /// Use [`NanokaClient::monster_image_url`](crate::NanokaClient::monster_image_url)
    /// to convert to a full URL.
    pub image: String,
    /// Element weakness / resistance values.
    ///
    /// Values: `1` = weak to this element, `0` = neutral, `-1` = resistant.
    pub element: ElementResist,
    /// Monster base stats at this level.
    pub stats: MonsterStats,
}

/// Element resistance / weakness values.
///
/// Each field is an i32: 1 = weak to this element, 0 = neutral, -1 = resistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ElementResist {
    pub ice: i32,
    pub fire: i32,
    pub electric: i32,
    pub ether: i32,
    pub physical: i32,
    #[serde(default)]
    pub wind: i32,
}

/// Base stats for a monster at the current zone level.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MonsterStats {
    /// Hit points.
    pub hp: f64,
    /// Attack power.
    pub attack: f64,
    /// Defence.
    pub defence: f64,
    /// Stun / daze gauge.
    pub stun: f64,
    /// Attribute Anomaly infliction rate.
    pub attribute_infliction: f64,
}

/// A buff effect applied to a zone or room.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Buff {
    /// Buff display title.
    pub title: String,
    /// Buff description (may contain HTML color tags like `<color=#FFFFFF>`).
    pub desc: String,
}

// --------------------------------------------------------------------
//  Deadly Assault (boss) detail
// --------------------------------------------------------------------

/// Full detail for a Deadly Assault (Trial) season.
///
/// Deadly Assault has a different top-level structure than Shiyu Defence:
/// it wraps zones in a `modes` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BossSeasonDetail {
    /// Numeric season ID.
    pub id: u64,
    /// Season name (e.g. "Trial").
    pub name: String,
    /// Priority / sort order (always 9 for Deadly Assault).
    pub priority: u32,
    /// Boss adjustment data (miscellaneous, keyed by zone type ID).
    #[serde(default)]
    pub boss_adjust: HashMap<String, serde_json::Value>,
    /// Default zone type ID.
    #[serde(default)]
    pub zone_type: u64,
    /// Game modes — usually a single element array.
    #[serde(default)]
    pub modes: Vec<BossMode>,
}

/// A single mode within a Deadly Assault season.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BossMode {
    /// Mode ID (same as parent season ID).
    pub id: u64,
    /// Zone type ID for this mode.
    #[serde(default)]
    pub zone_type: u64,
    /// Map of zone ID → zone data.
    #[serde(default)]
    pub zone: HashMap<String, Zone>,
}

// --------------------------------------------------------------------
//  Boss stat scaling (Deadly Assault level slider)
// --------------------------------------------------------------------

/// Per-level rate modifiers extracted from `boss_adjust`.
///
/// These are applied to the API monster stats to compute the final
/// in-game stats at a given player Inter-Knot level.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LevelRates {
    /// HP rate in basis points (10000 = 100%).
    pub hp_rate: i64,
    /// ATK rate in basis points (0 = no change, 2500 = +25%).
    pub atk_rate: i64,
    /// Performance points for this level.
    pub points: i64,
}

/// Final monster stats after applying level scaling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScaledStats {
    /// Final HP.
    pub hp: f64,
    /// Final ATK.
    pub atk: f64,
    /// Performance points.
    pub points: i64,
}

impl BossSeasonDetail {
    /// Detect the endgame type. Always [`EndgameType::DeadlyAssault`].
    pub fn endgame_type(&self) -> EndgameType {
        EndgameType::DeadlyAssault
    }

    /// Convenience: extract all zones from the first mode.
    ///
    /// A beta season can contain an additional mode for its complex boss;
    /// callers which present the complete season must iterate [`modes`](Self::modes).
    pub fn zones(&self) -> Option<&HashMap<String, Zone>> {
        self.modes.first().map(|m| &m.zone)
    }

    /// Extract the per-level rate array for the given mode (or the first mode).
    ///
    /// This walks the `boss_adjust` map starting from the mode's `zone_type`
    /// base key, collecting sequential entries until a gap is found.
    /// Returns an ordered vector of [`LevelRates`] — index 0 = player level 1.
    pub fn level_rates(&self, mode_index: usize) -> Vec<LevelRates> {
        let mode = match self.modes.get(mode_index) {
            Some(m) => m,
            None => return Vec::new(),
        };

        // The base key usually equals the zone type (e.g. 1001 for the
        // standard Trial mode). The complex Trial mode advertises zone type
        // 1002, but its 24-level scaling table is stored under 1301..=1324.
        // Keep the zone-type fallback for older payloads that do not include
        // that dedicated table.
        let base = if mode.zone_type == 1002 && self.boss_adjust.contains_key("1301") {
            1301
        } else {
            mode.zone_type
        };

        let mut rates = Vec::new();
        for offset in 0i64.. {
            let key = (base as i64 + offset).to_string();
            match self.boss_adjust.get(&key) {
                Some(v) => {
                    let hp_rate = v.get("hp").and_then(|v| v.as_i64()).unwrap_or(10000);
                    let atk_rate = v.get("atk").and_then(|v| v.as_i64()).unwrap_or(0);
                    let points = v.get("points").and_then(|v| v.as_i64()).unwrap_or(0);
                    rates.push(LevelRates {
                        hp_rate,
                        atk_rate,
                        points,
                    });
                }
                None => break,
            }
        }
        rates
    }

    /// Compute final stats for a monster at a given player level.
    ///
    /// * `api_hp` / `api_atk` — raw stats from the monster JSON (`stats.hp`, `stats.attack`).
    /// * `level` — difficulty level on the nanoka slider (1-based).
    /// * `mode_index` — which mode to use (usually 0).
    ///
    /// Formulas match nanoka.cc:
    ///   - **HP**: cumulative `floor(api_hp × hp_rate / 10000)` summed over levels `1..=level`
    ///   - **ATK**: `floor(api_atk × (1 + atk_rate / 10000))` at `level`
    ///   - **Points**: cumulative sum of per-level `points` over `1..=level`
    pub fn scale_stats(
        &self,
        api_hp: f64,
        api_atk: f64,
        level: usize,
        mode_index: usize,
    ) -> Option<ScaledStats> {
        let rates = self.level_rates(mode_index);
        if rates.is_empty() || level == 0 || level > rates.len() {
            return None;
        }

        let rate = rates[level - 1];

        let hp: f64 = rates[..level]
            .iter()
            .map(|r| api_hp * r.hp_rate as f64 / 10000.0)
            .sum();

        let atk = (api_atk * (1.0 + rate.atk_rate as f64 / 10000.0)).floor();

        let points: i64 = rates[..level].iter().map(|r| r.points).sum();

        Some(ScaledStats {
            hp: hp.floor(),
            atk,
            points,
        })
    }

    /// Compute the dual values shown on nanoka.cc for the max difficulty.
    ///
    /// - **HP**: single-level HP at max level → cumulative HP over all levels
    /// - **ATK**: ATK at max level (unchanged across the pair)
    /// - **Points**: max-level entry points → cumulative points over all levels
    pub fn scale_stats_range(
        &self,
        api_hp: f64,
        api_atk: f64,
        mode_index: usize,
    ) -> Option<ScaledStatsRange> {
        let rates = self.level_rates(mode_index);
        if rates.is_empty() {
            return None;
        }

        let last = rates.last().unwrap();

        let hp_simple_max = (api_hp * last.hp_rate as f64 / 10000.0).floor();

        let hp_cumulative: f64 = rates
            .iter()
            .map(|r| api_hp * r.hp_rate as f64 / 10000.0)
            .sum();
        let hp_cumulative = hp_cumulative.floor();

        let atk = (api_atk * (1.0 + last.atk_rate as f64 / 10000.0)).floor();

        let points_min = last.points;
        let points_max: i64 = rates.iter().map(|r| r.points).sum();

        Some(ScaledStatsRange {
            hp_min: hp_simple_max,
            hp_max: hp_cumulative,
            atk,
            points_min,
            points_max,
        })
    }

    /// Replace raw API monster stats in-place with final scaled stats.
    ///
    /// Walks every zone → room → monster and overwrites:
    /// - `stats.hp` → cumulative HP at the given level
    /// - `stats.attack` → ATK at the given level
    ///
    /// Rank goals (`s_rank_goal` / `a_rank_goal` / `b_rank_goal`) are left
    /// unchanged — they are fixed score thresholds from the API, not level rates.
    ///
    /// If `level` is `None` the max available level is used.
    /// `mode_index` selects which mode (usually 0).
    pub fn scale_stats_in_place(&mut self, level: Option<usize>, mode_index: usize) {
        let rates = self.level_rates(mode_index);
        if rates.is_empty() {
            return;
        }

        let max_level = rates.len();
        let lvl = level.unwrap_or(max_level).clamp(1, max_level);
        let rate = &rates[lvl - 1];

        let hp_cumul_mult: f64 = rates[..lvl]
            .iter()
            .map(|r| r.hp_rate as f64 / 10000.0)
            .sum();
        let atk_mult = 1.0 + rate.atk_rate as f64 / 10000.0;

        let Some(mode) = self.modes.get_mut(mode_index) else {
            return;
        };
        for zone in mode.zone.values_mut() {
            for room in zone.layer_room.values_mut() {
                for monster in room.monster_list.values_mut() {
                    monster.stats.hp = (monster.stats.hp * hp_cumul_mult).floor();
                    monster.stats.attack = (monster.stats.attack * atk_mult).floor();
                }
            }
        }
    }

    /// Scale every mode with the adjustment series assigned to that mode.
    ///
    /// Most production seasons have one mode. Beta seasons may include a
    /// separate complex-boss mode with a different `zone_type`, so applying
    /// the first mode's rates to every room would show incorrect stats.
    pub fn scale_all_modes_in_place(&mut self, level: Option<usize>) {
        for mode_index in 0..self.modes.len() {
            self.scale_stats_in_place(level, mode_index);
        }
    }
}

/// Dual scaled-stat values as displayed on nanoka.cc at max difficulty.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScaledStatsRange {
    /// Single-level HP at max difficulty.
    pub hp_min: f64,
    /// Cumulative HP across all difficulty levels.
    pub hp_max: f64,
    /// ATK at max difficulty.
    pub atk: f64,
    /// Per-level points entry at max difficulty.
    pub points_min: i64,
    /// Cumulative points across all difficulty levels.
    pub points_max: i64,
}

// --------------------------------------------------------------------
//  Unified detail enum
// --------------------------------------------------------------------

/// Unified season detail returned by [`NanokaClient::get_detail`](crate::NanokaClient::get_detail).
///
/// Dispatches to the correct variant based on the season ID prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnySeasonDetail {
    /// Shiyu Defence season.
    Shiyu(SeasonDetail),
    /// Deadly Assault (Trial) season.
    Boss(BossSeasonDetail),
}

impl AnySeasonDetail {
    /// Return the endgame type for this detail.
    pub fn endgame_type(&self) -> EndgameType {
        match self {
            AnySeasonDetail::Shiyu(_) => EndgameType::ShiyuDefence,
            AnySeasonDetail::Boss(_) => EndgameType::DeadlyAssault,
        }
    }

    /// Return a reference to the zone map regardless of variant.
    pub fn zones(&self) -> &HashMap<String, Zone> {
        match self {
            AnySeasonDetail::Shiyu(d) => &d.zone,
            AnySeasonDetail::Boss(d) => {
                // Return zones from the first mode, or an empty ref.
                // This is a bit awkward — we keep a static empty map for the fallback.
                d.zones().unwrap_or(&EMPTY_ZONE_MAP)
            }
        }
    }
}

static EMPTY_ZONE_MAP: std::sync::LazyLock<HashMap<String, Zone>> =
    std::sync::LazyLock::new(HashMap::new);
