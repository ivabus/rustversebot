//! Async Rust client for [nanoka.cc](https://nanoka.cc) Zenless Zone Zero data.
//!
//! This crate provides Shiyu Defense and Deadly Assault season data.
//! The data includes monster stats, buffs, weaknesses, and images.
//!
//! # Endgame types
//!
//! The two game modes are identified via [`EndgameType`]:
//!
//! | Type | ID prefix | Index URL | Detail URL |
//! |------|-----------|-----------|------------|
//! | [`ShiyuDefence`] | `61…`, `62…` | `shiyu.json` | `en/shiyu/{id}.json` |
//! | [`DeadlyAssault`] | `69…` | `boss.json` | `en/boss/{id}.json` |
//!
//! # Quick start
//!
//! ```no_run
//! use nanoka::{NanokaClient, types::EndgameType};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = NanokaClient::new();
//!
//!     // List all Shiyu Defense seasons
//!     let shiyu = client.get_seasons_by_type(EndgameType::ShiyuDefence).await?;
//!     for (id, meta) in &shiyu {
//!         println!("[{id}] {} (sort={})", meta.en, meta.sort);
//!     }
//!
//!     // Fetch Deadly Assault detail
//!     let detail = client.get_boss_detail(69041).await?;
//!     println!("Season {}: {}", detail.id, detail.name);
//!     Ok(())
//! }
//! ```

pub mod types;

use regex::Regex;
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::sync::OnceLock;
use tracing::debug;
use types::*;

static IMAGE_FILENAME_RE: OnceLock<Regex> = OnceLock::new();

#[derive(serde::Deserialize)]
struct DataManifest {
    zzz: GameManifest,
}

#[derive(serde::Deserialize)]
struct GameManifest {
    latest: String,
}

/// The main client for the nanoka.cc ZZZ API.
///
/// On first use, the client resolves the latest game data version.
/// The client stores this version in memory for its lifetime.
///
/// # Example
///
/// ```no_run
/// use nanoka::NanokaClient;
///
/// let client = NanokaClient::new();
/// ```
pub struct NanokaClient {
    http: HttpClient,
    base_url: String,       // https://static.nanoka.cc
    image_base_url: String, // https://static.nanoka.cc/assets/zzz
    version: OnceLock<String>,
    /// Language code for localized data. The default is `"en"`.
    pub lang: String,
}

impl Default for NanokaClient {
    fn default() -> Self {
        Self::new()
    }
}

impl NanokaClient {
    /// Create a new client with default settings.
    ///
    /// Language defaults to `"en"`. Use [`with_lang`](Self::with_lang) to
    /// customize the language.
    pub fn new() -> Self {
        Self {
            http: HttpClient::new(),
            base_url: "https://static.nanoka.cc".into(),
            image_base_url: "https://static.nanoka.cc/assets/zzz".into(),
            version: OnceLock::new(),
            lang: "en".into(),
        }
    }

    /// Set the display language for localized fields.
    ///
    /// Supported values observed on nanoka.cc: `"en"`, `"ko"`, `"zh"`, `"ja"`.
    pub fn with_lang(mut self, lang: impl Into<String>) -> Self {
        self.lang = lang.into();
        self
    }

    /// Override the static asset base URL for tests or mirrors.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the image CDN base URL.
    pub fn with_image_base_url(mut self, url: impl Into<String>) -> Self {
        self.image_base_url = url.into();
        self
    }

    /// Override the game data version instead of resolving it from the website.
    pub fn with_version(self, version: impl Into<String>) -> Self {
        let v = version.into();
        let _ = self.version.set(v);
        self
    }

    // ------------------------------------------------------------------
    //  Version resolution
    // ------------------------------------------------------------------

    /// Resolve and cache the latest game data version.
    ///
    /// Fetch the static-data manifest and read the latest ZZZ version.
    pub async fn version(&self) -> Result<&str, Error> {
        if let Some(v) = self.version.get() {
            return Ok(v.as_str());
        }

        let url = format!("{}/manifest.json", self.base_url.trim_end_matches('/'));
        let manifest: DataManifest = self.get_json(&url).await?;
        let version = manifest.zzz.latest;
        if version.is_empty() {
            return Err(Error::VersionNotFound);
        }
        debug!("Resolved game version: {version}");

        // Cannot fail: OnceLock::set on an empty cell always succeeds.
        let _ = self.version.set(version);
        Ok(self.version.get().unwrap().as_str())
    }

    // ------------------------------------------------------------------
    //  Shiyu Defence API
    // ------------------------------------------------------------------

    /// Fetch the full index of all Shiyu Defense seasons.
    ///
    /// This is the `shiyu.json` endpoint. ID prefix: `61…` / `62…`.
    /// For Deadly Assault see [`get_boss_seasons`](Self::get_boss_seasons).
    pub async fn get_seasons(&self) -> Result<HashMap<String, SeasonMeta>, Error> {
        let url = self.data_index_url("shiyu").await?;
        self.get_json(&url).await
    }

    /// Fetch detailed data for a single Shiyu Defense season.
    ///
    /// Example IDs: `62053`, `61001`, `620561`.
    /// For Deadly Assault see [`get_boss_detail`](Self::get_boss_detail).
    pub async fn get_season_detail(&self, id: u64) -> Result<SeasonDetail, Error> {
        let url = self.data_detail_url("shiyu", id).await?;
        self.get_json(&url).await
    }

    // ------------------------------------------------------------------
    //  Deadly Assault API
    // ------------------------------------------------------------------

    /// Fetch the full index of all Deadly Assault (Trial) seasons.
    ///
    /// This is the `boss.json` endpoint. ID prefix: `69…`.
    pub async fn get_boss_seasons(&self) -> Result<HashMap<String, SeasonMeta>, Error> {
        let url = self.data_index_url("boss").await?;
        self.get_json(&url).await
    }

    /// Fetch detailed data for a single Deadly Assault (Trial) season.
    ///
    /// Returns a [`BossSeasonDetail`] which has a different structure than
    /// Shiyu Defense — zones are wrapped in a `modes` array. Use
    /// [`BossSeasonDetail::zones`] to access the zone map.
    ///
    /// Example IDs: `69041`, `690441`.
    pub async fn get_boss_detail(&self, id: u64) -> Result<BossSeasonDetail, Error> {
        let url = self.data_detail_url("boss", id).await?;
        self.get_json(&url).await
    }

    // ------------------------------------------------------------------
    //  Unified / convenience API
    // ------------------------------------------------------------------

    /// Fetch the season index for a specific [`EndgameType`].
    ///
    /// Convenience wrapper that dispatches to [`get_seasons`](Self::get_seasons)
    /// or [`get_boss_seasons`](Self::get_boss_seasons).
    pub async fn get_seasons_by_type(
        &self,
        ty: EndgameType,
    ) -> Result<HashMap<String, SeasonMeta>, Error> {
        match ty {
            EndgameType::ShiyuDefence => self.get_seasons().await,
            EndgameType::DeadlyAssault => self.get_boss_seasons().await,
        }
    }

    /// Fetch season detail for any ID, returning a unified enum.
    ///
    /// Dispatches to [`get_season_detail`](Self::get_season_detail) or
    /// [`get_boss_detail`](Self::get_boss_detail) based on the ID prefix.
    pub async fn get_detail(&self, id: u64) -> Result<AnySeasonDetail, Error> {
        match EndgameType::from_id(id) {
            Some(EndgameType::DeadlyAssault) => {
                self.get_boss_detail(id).await.map(AnySeasonDetail::Boss)
            }
            Some(EndgameType::ShiyuDefence) => {
                self.get_season_detail(id).await.map(AnySeasonDetail::Shiyu)
            }
            None => Err(Error::UnknownSeasonId(id)),
        }
    }

    // ------------------------------------------------------------------
    //  Resolved / scaled convenience methods
    // ------------------------------------------------------------------

    /// Fetch a Deadly Assault detail with images resolved and stats scaled
    /// to final in-game values.
    ///
    /// Equivalent to calling [`get_boss_detail`](Self::get_boss_detail) then
    /// [`BossSeasonDetail::scale_stats_in_place`] then resolving images.
    pub async fn get_boss_detail_resolved(
        &self,
        id: u64,
        level: Option<usize>,
    ) -> Result<BossSeasonDetail, Error> {
        let mut detail = self.get_boss_detail(id).await?;
        detail.scale_all_modes_in_place(level);
        for mode in &mut detail.modes {
            Self::resolve_zone_images(&self.image_base_url, &mut mode.zone);
        }
        Ok(detail)
    }

    /// Fetch any season detail with images resolved and stats scaled
    /// (Deadly Assault only — Shiyu gets images resolved but no stat scaling).
    ///
    /// Equivalent to [`get_detail`](Self::get_detail) followed by
    /// [`resolve_images`](Self::resolve_images).
    pub async fn get_detail_resolved(&self, id: u64) -> Result<AnySeasonDetail, Error> {
        let mut detail = self.get_detail(id).await?;
        self.resolve_images(&mut detail);
        Ok(detail)
    }

    /// Build a full, working image URL from a monster `image` or `monster_icon`
    /// path found in the JSON.
    ///
    /// The path is expected to be something like
    /// `"UI/Sprite/A1DynamicLoad/BossCard/UnPacker/BossCardLv02/Monster_Banyrek.png"`.
    ///
    /// Returns `None` when the path is empty.
    pub fn monster_image_url(&self, path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }

        let re = IMAGE_FILENAME_RE
            .get_or_init(|| Regex::new(r"([^/]+)\.\w+$").expect("image filename regex compile"));

        if let Some(caps) = re.captures(path) {
            Some(format!("{}/{}.webp", self.image_base_url, &caps[1]))
        } else {
            // Fallback: use the whole last segment as-is and tack on .webp
            let name = path.rsplit('/').next().unwrap_or(path);
            let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
            Some(format!("{}/{}.webp", self.image_base_url, stem))
        }
    }

    /// Replace all relative image paths AND raw API stats in the given detail.
    ///
    /// For Shiyu Defense: only image paths are resolved.
    /// For Deadly Assault: image paths are resolved AND monster stats + zone
    /// goals are scaled to their final in-game values using the max player
    /// level from boss_adjust.
    ///
    /// Use this method before serialization to JSON.
    /// Consumers then do not need to resolve images or scale stats.
    pub fn resolve_images(&self, detail: &mut AnySeasonDetail) {
        match detail {
            AnySeasonDetail::Shiyu(d) => {
                Self::resolve_zone_images(&self.image_base_url, &mut d.zone);
            }
            AnySeasonDetail::Boss(d) => {
                // Scale stats first, then resolve images
                d.scale_all_modes_in_place(None);
                for mode in &mut d.modes {
                    Self::resolve_zone_images(&self.image_base_url, &mut mode.zone);
                }
            }
        }
    }

    /// Internal helper: walk a zone map and replace image paths.
    fn resolve_zone_images(image_base: &str, zones: &mut HashMap<String, Zone>) {
        let re =
            IMAGE_FILENAME_RE.get_or_init(|| Regex::new(r"([^/]+)\.\w+$").expect("image regex"));

        for zone in zones.values_mut() {
            for room in zone.layer_room.values_mut() {
                // Resolve monster_icon
                if !room.monster_icon.is_empty() {
                    room.monster_icon =
                        Self::resolve_image_path(image_base, re, &room.monster_icon);
                }
                // Resolve each monster's image
                for monster in room.monster_list.values_mut() {
                    if !monster.image.is_empty() {
                        monster.image = Self::resolve_image_path(image_base, re, &monster.image);
                    }
                }
            }
        }
    }

    fn resolve_image_path(image_base: &str, re: &Regex, path: &str) -> String {
        if let Some(caps) = re.captures(path) {
            format!("{}/{}.webp", image_base, &caps[1])
        } else {
            let name = path.rsplit('/').next().unwrap_or(path);
            let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
            format!("{}/{}.webp", image_base, stem)
        }
    }

    // ------------------------------------------------------------------
    //  Internal helpers
    // ------------------------------------------------------------------

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }
        Ok(resp.json().await?)
    }

    async fn data_index_url(&self, kind: &str) -> Result<String, Error> {
        let v = self.version().await?;
        Ok(format!("{}/zzz/{}/{kind}.json", self.base_url, v))
    }

    async fn data_detail_url(&self, kind: &str, id: u64) -> Result<String, Error> {
        let v = self.version().await?;
        Ok(format!(
            "{}/zzz/{}/{}/{kind}/{id}.json",
            self.base_url, v, self.lang
        ))
    }
}

// --------------------------------------------------------------------
//  Error type
// --------------------------------------------------------------------

/// Errors that can occur while using the nanoka API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Network / HTTP transport error.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The server returned a non-success HTTP status.
    #[error("HTTP {status} for {url}")]
    HttpStatus { status: u16, url: String },

    /// Season ID does not match a known endgame prefix (`61`/`62`/`69`).
    #[error("Unknown season ID {0} — expected a Shiyu (61…/62…) or Deadly Assault (69…) ID")]
    UnknownSeasonId(u64),

    /// The game data version string was absent from the static-data manifest.
    #[error("Could not find the ZZZ data version in the nanoka.cc manifest")]
    VersionNotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_version_from_manifest() {
        let manifest: DataManifest = serde_json::from_str(
            r#"{"zzz":{"latest":"3.1","available":["3.1","3.2.0+17782873"]}}"#,
        )
        .unwrap();

        assert_eq!(manifest.zzz.latest, "3.1");
    }

    #[test]
    fn test_endgame_type_from_id() {
        assert_eq!(EndgameType::from_id(62053), Some(EndgameType::ShiyuDefence));
        assert_eq!(EndgameType::from_id(61001), Some(EndgameType::ShiyuDefence));
        assert_eq!(
            EndgameType::from_id(620561),
            Some(EndgameType::ShiyuDefence)
        );
        assert_eq!(
            EndgameType::from_id(69041),
            Some(EndgameType::DeadlyAssault)
        );
        assert_eq!(
            EndgameType::from_id(690441),
            Some(EndgameType::DeadlyAssault)
        );
        assert_eq!(EndgameType::from_id(99999), None);
        assert_eq!(EndgameType::from_id(99), None);
    }

    #[test]
    fn test_monster_image_url() {
        let client = NanokaClient::new();
        let url = client.monster_image_url(
            "UI/Sprite/A1DynamicLoad/BossCard/UnPacker/BossCardLv02/Monster_Banyrek.png",
        );
        assert_eq!(
            url.unwrap(),
            "https://static.nanoka.cc/assets/zzz/Monster_Banyrek.webp"
        );
    }

    #[test]
    fn test_monster_image_url_empty() {
        let client = NanokaClient::new();
        assert!(client.monster_image_url("").is_none());
    }

    #[test]
    fn test_monster_image_url_monster_icon() {
        let client = NanokaClient::new();
        let url = client.monster_image_url(
            "Assets/NapResources/UI/Sprite/A1DynamicLoad/IconBossGeneral/UnPacker/IconMonster_HatiArmoredBoss.png"
        );
        assert_eq!(
            url.unwrap(),
            "https://static.nanoka.cc/assets/zzz/IconMonster_HatiArmoredBoss.webp"
        );
    }

    #[test]
    fn test_boss_stat_scaling() {
        // Simulate boss_adjust data for zone_type 1001
        let mut boss_adjust = HashMap::new();
        boss_adjust.insert(
            "1001".into(),
            serde_json::json!({"hp": 1200, "atk": -5000, "points": 1000}),
        );
        boss_adjust.insert(
            "1002".into(),
            serde_json::json!({"hp": 1200, "atk": -3000, "points": 1000}),
        );

        let detail = BossSeasonDetail {
            id: 69041,
            name: "Trial".into(),
            priority: 9,
            boss_adjust,
            zone_type: 1001,
            modes: vec![BossMode {
                id: 69041,
                zone_type: 1001,
                zone: HashMap::new(),
            }],
        };

        let rates = detail.level_rates(0);
        assert_eq!(rates.len(), 2);
        assert_eq!(rates[0].hp_rate, 1200);
        assert_eq!(rates[0].atk_rate, -5000);
        assert_eq!(rates[0].points, 1000);

        // Level 1 stats for the known boss
        let api_hp = 19_081_773.292_5;
        let api_atk = 2_967.1515151515155;

        let scaled = detail.scale_stats(api_hp, api_atk, 1, 0).unwrap();
        // Cumulative HP at level 1 = api_hp * 1200 / 10000
        assert_eq!(scaled.hp, (api_hp * 1200.0 / 10000.0).floor());
        // ATK = api_atk * (1 + (-5000)/10000) = api_atk * 0.5
        assert_eq!(scaled.atk, (api_atk * 0.5).floor());
        assert_eq!(scaled.points, 1000);

        // Level 2
        let scaled2 = detail.scale_stats(api_hp, api_atk, 2, 0).unwrap();
        // Cumulative HP = api_hp * (1200+1200) / 10000
        assert_eq!(scaled2.hp, (api_hp * 2400.0 / 10000.0).floor());
        // ATK = api_atk * (1 + (-3000)/10000) = api_atk * 0.7
        assert_eq!(scaled2.atk, (api_atk * 0.7).floor());
        // Points are cumulative
        assert_eq!(scaled2.points, 2000);

        let range = detail.scale_stats_range(api_hp, api_atk, 0).unwrap();
        assert_eq!(range.hp_min, (api_hp * 1200.0 / 10000.0).floor());
        assert_eq!(range.hp_max, (api_hp * 2400.0 / 10000.0).floor());
        assert_eq!(range.points_min, 1000); // last level entry
        assert_eq!(range.points_max, 2000); // cumulative
    }

    #[test]
    fn test_complex_boss_uses_dedicated_scaling_series() {
        let mut boss_adjust = HashMap::new();
        // `1002` is the advertised complex-mode zone type, but its values
        // are not the complex boss's level table.
        boss_adjust.insert(
            "1002".into(),
            serde_json::json!({"hp": 1200, "atk": -3000, "points": 1000}),
        );
        boss_adjust.insert(
            "1301".into(),
            serde_json::json!({"hp": 3600, "atk": 0, "points": 750}),
        );
        boss_adjust.insert(
            "1302".into(),
            serde_json::json!({"hp": 3600, "atk": 0, "points": 750}),
        );

        let detail = BossSeasonDetail {
            id: 690431,
            name: "Trial".into(),
            priority: 9,
            boss_adjust,
            zone_type: 1002,
            modes: vec![BossMode {
                id: 690432,
                zone_type: 1002,
                zone: HashMap::new(),
            }],
        };

        let rates = detail.level_rates(0);
        assert_eq!(rates.len(), 2);
        assert_eq!(rates[0].hp_rate, 3600);
        assert_eq!(rates[0].points, 750);

        // At level two, this must use 1301 + 1302 rather than key 1002.
        let scaled = detail.scale_stats(1_000.0, 2_000.0, 2, 0).unwrap();
        assert_eq!(scaled.hp, 720.0);
        assert_eq!(scaled.atk, 2_000.0);
        assert_eq!(scaled.points, 1_500);
    }

    #[test]
    fn test_scale_stats_in_place_preserves_rank_goals() {
        let mut zones = HashMap::new();
        let mut rooms = HashMap::new();
        let mut monsters = HashMap::new();
        monsters.insert(
            "1".into(),
            Monster {
                id: 1,
                name: "Boss".into(),
                image: String::new(),
                element: ElementResist {
                    ice: 0,
                    fire: 1,
                    electric: 0,
                    ether: 0,
                    physical: 0,
                    wind: 0,
                },
                stats: MonsterStats {
                    hp: 10_000.0,
                    attack: 1_000.0,
                    defence: 100.0,
                    stun: 100.0,
                    attribute_infliction: 10.0,
                },
            },
        );
        rooms.insert(
            "1".into(),
            Room {
                monster_icon: String::new(),
                monster_list: monsters,
                monster_weakness: HashMap::new(),
                waves_num: 1,
            },
        );
        zones.insert(
            "1".into(),
            Zone {
                name: "Stage".into(),
                stage_num: 1,
                monster_level: 70,
                layer_buff: HashMap::new(),
                selectable_buff: HashMap::new(),
                child: vec![],
                layer_room: rooms,
                goal_type: 2,
                ss_rank_goal: 0,
                s_rank_goal: 20_000,
                a_rank_goal: 14_000,
                b_rank_goal: 6_000,
            },
        );

        let mut boss_adjust = HashMap::new();
        boss_adjust.insert(
            "1001".into(),
            serde_json::json!({"hp": 1200, "atk": -5000, "points": 1000}),
        );

        let mut detail = BossSeasonDetail {
            id: 69041,
            name: "Trial".into(),
            priority: 9,
            boss_adjust,
            zone_type: 1001,
            modes: vec![BossMode {
                id: 69041,
                zone_type: 1001,
                zone: zones,
            }],
        };

        detail.scale_stats_in_place(Some(1), 0);

        let zone = detail.zones().unwrap().get("1").unwrap();
        assert_eq!(zone.s_rank_goal, 20_000);
        assert_eq!(zone.a_rank_goal, 14_000);
        assert_eq!(zone.b_rank_goal, 6_000);

        let monster = zone
            .layer_room
            .get("1")
            .unwrap()
            .monster_list
            .get("1")
            .unwrap();
        assert_eq!(monster.stats.hp, 1_200.0);
        assert_eq!(monster.stats.attack, 500.0);
    }
}
