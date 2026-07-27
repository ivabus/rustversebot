use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::common::DateComponents;

// ── Daily Note ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZDailyNote {
    pub energy: ZZZEnergy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZEnergy {
    pub progress: ZZZEnergyProgress,
    /// Seconds until full battery restore.
    pub restore: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZEnergyProgress {
    pub current: i64,
    pub max: i64,
}

// ── Shared sub-types (avatars, buddy, buffer, time) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZAvatar {
    pub id: i64,
    pub level: i64,
    pub rank: i64,
    pub rarity: String,
    pub element_type: i64,
    pub avatar_profession: i64,
    pub sub_element_type: i64,
    pub role_square_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZBuddy {
    pub id: i64,
    pub level: i64,
    pub rarity: String,
    pub bangboo_rectangle_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZBuffer {
    #[serde(alias = "name")]
    pub title: Option<String>,
    #[serde(alias = "text")]
    #[serde(alias = "desc")]
    pub description: Option<String>,
    pub icon: Option<String>,
}

// ── Shiyu Defense ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZShiyuDefense {
    pub zone_id: Option<i64>,
    pub hadal_begin_time: Option<DateComponents>,
    pub hadal_end_time: Option<DateComponents>,
    pub pass_fifth_floor: Option<bool>,
    pub begin_time: Option<String>,
    pub end_time: Option<String>,

    #[serde(default)]
    pub brief: Option<ZZZShiyuBrief>,

    /// All floor detail objects, keyed by name.
    #[serde(flatten)]
    pub layers: HashMap<String, ZZZShiyuLayerDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZShiyuBrief {
    pub cur_period_zone_layer_count: Option<i64>,
    pub max_score: Option<i64>,
    pub score: Option<i64>,
    pub rating: Option<String>,
    pub rank_percent: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZShiyuLayerDetail {
    /// Present on frontier/5th-floor layers.
    #[serde(default)]
    pub layer_challenge_info_list: Vec<ZZZShiyuLayerChallenge>,
    /// Present on stable/4th-floor layers.
    #[serde(default)]
    pub buffer: Option<ZZZBuffer>,
    #[serde(default)]
    pub challenge_time: Option<DateComponents>,
    #[serde(default)]
    pub rating: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZShiyuLayerChallenge {
    pub layer_id: Option<i64>,
    pub max_score: Option<i64>,
    pub score: Option<i64>,
    pub rating: Option<String>,
    #[serde(default)]
    pub monster_pic: Option<String>,
    pub challenge_time: Option<DateComponents>,
    pub buffer: Option<ZZZBuffer>,
    #[serde(default)]
    pub avatar_list: Vec<ZZZAvatar>,
    pub buddy: Option<ZZZBuddy>,
}

// ── Deadly Assault ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZDeadlyAssault {
    pub start_time: Option<DateComponents>,
    pub end_time: Option<DateComponents>,
    pub rank_percent: Option<f64>,
    pub total_score: Option<i64>,
    pub total_star: Option<i64>,
    pub total_max_score: Option<i64>,
    pub room_max_score: Option<i64>,
    pub zone_id: Option<i64>,
    pub has_data: Option<bool>,
    pub nick_name: Option<String>,
    pub avatar_icon: Option<String>,
    #[serde(default)]
    pub list: Vec<ZZZDeadlyAssaultRoom>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZDeadlyAssaultRoom {
    pub score: Option<i64>,
    pub star: Option<i64>,
    pub total_star: Option<i64>,
    pub challenge_time: Option<DateComponents>,
    #[serde(default)]
    pub avatar_list: Vec<ZZZAvatar>,
    pub buddy: Option<ZZZBuddy>,
    #[serde(default)]
    pub boss: Vec<ZZZBossInfo>,
    #[serde(default)]
    pub buffer: Vec<ZZZBuffer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZBossInfo {
    pub name: String,
    pub icon: String,
    pub bg_icon: String,
    pub race_icon: String,
}

// ── Avatar List (basic) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZAvatarList {
    #[serde(default)]
    pub avatar_list: Vec<ZZZAvatarInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZAvatarInfo {
    pub id: i64,
    pub level: i64,
    pub name_mi18n: String,
    pub full_name_mi18n: String,
    pub element_type: i64,
    pub camp_name_mi18n: String,
    pub avatar_profession: i64,
    pub rarity: String,
    pub group_icon_path: String,
    pub hollow_icon_path: String,
    pub rank: i64,
    pub is_chosen: bool,
    pub role_square_url: String,
    pub sub_element_type: i64,
    pub awaken_state: String,
}

// ── Index (Profile Summary) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZIndex {
    #[serde(default)]
    pub stats: Option<ZZZIndexStats>,

    #[serde(default)]
    pub avatar_list: Vec<ZZZAvatarInfo>,

    #[serde(default)]
    pub buddy_list: Vec<ZZZBuddyInfo>,

    pub cur_head_icon_url: Option<String>,

    #[serde(default)]
    pub game_data_show: Option<ZZZGameDataShow>,

    #[serde(default)]
    pub area_collections: Vec<ZZZAreaCollection>,

    #[serde(default)]
    pub challenge_schedule_list: Vec<ZZZChallengeSchedule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZBuddyInfo {
    pub id: i64,
    pub name: Option<String>,
    pub rarity: Option<String>,
    pub level: Option<i64>,
    #[serde(default)]
    pub star: Option<i64>,
    pub bangboo_rectangle_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZGameDataShow {
    pub personal_title: Option<String>,
    #[serde(default)]
    pub card_url: Option<String>,
    #[serde(default)]
    pub medal_list: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZAreaCollection {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub collection_progress: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZChallengeSchedule {
    pub challenge_type: Option<String>,
    pub start_ts: Option<String>,
    pub end_ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZIndexStats {
    pub active_days: Option<i64>,
    pub avatar_num: Option<i64>,
    pub world_level_name: Option<String>,
    pub achievement_count: Option<i64>,
    pub buddy_num: Option<i64>,

    /// Shiyu Defense summary (v2).
    #[serde(default)]
    pub hadal_brief: Option<ZZZIndexHadalBrief>,

    /// Deadly Assault summary.
    #[serde(default)]
    pub memory_battlefield: Option<ZZZIndexMemBattle>,

    /// Tower: current layer count.
    pub climbing_tower_layer: Option<i64>,
    pub challenge_full_s_times: Option<i64>,
    pub memory_battlefield_full_stars_times: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZIndexHadalBrief {
    #[serde(rename = "hadal_brief_v2")]
    #[serde(default)]
    pub v2: Option<ZZZIndexHadalBriefV2>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZIndexHadalBriefV2 {
    pub cur_period_zone_layer_count: Option<i64>,
    pub score: Option<i64>,
    pub max_score: Option<i64>,
    pub rating: Option<String>,
    pub rank_percent: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZIndexMemBattle {
    pub rank_percent: Option<i64>,
    pub total_score: Option<i64>,
    pub total_star: Option<i64>,
    pub zone_id: Option<i64>,
}

// ── Gacha Calendar ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZGachaCalendar {
    #[serde(default)]
    pub avatar_gacha_schedule_list: Vec<ZZZCharacterGachaEvent>,
    #[serde(default)]
    pub weapon_gacha_schedule_list: Vec<ZZZWeaponGachaEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZCharacterGachaEvent {
    pub gacha_type: Option<String>,
    pub gacha_state: Option<String>,
    pub start_ts: Option<i64>,
    pub end_ts: Option<i64>,
    pub version: Option<String>,
    #[serde(default)]
    pub avatar_list: Vec<ZZZGachaEventCharacter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZWeaponGachaEvent {
    pub gacha_type: Option<String>,
    pub gacha_state: Option<String>,
    pub start_ts: Option<i64>,
    pub end_ts: Option<i64>,
    pub version: Option<String>,
    #[serde(default)]
    pub weapon_list: Vec<ZZZGachaEventWeapon>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZGachaEventCharacter {
    pub avatar_id: Option<i64>,
    pub avatar_name: Option<String>,
    pub full_name: Option<String>,
    pub rarity: Option<String>,
    pub icon: Option<String>,
    pub avatar_profession: Option<i64>,
    pub avatar_element_type: Option<i64>,
    pub avatar_sub_element_type: Option<i64>,
    #[serde(default)]
    pub wiki_url: Option<String>,
    #[serde(default)]
    pub show_upon: Option<bool>,
    #[serde(default)]
    pub jump_cultivate: Option<bool>,
    #[serde(default)]
    pub is_forward: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZZZGachaEventWeapon {
    pub weapon_id: Option<i64>,
    pub rarity: Option<String>,
    pub icon: Option<String>,
    pub talent_title: Option<String>,
    pub talent_content: Option<String>,
    #[serde(default)]
    pub wiki_url: Option<String>,
    #[serde(default)]
    pub show_upon: Option<bool>,
    pub profession: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deser_deadly_assault() {
        let json = r#"{
            "start_time": {"year":2026,"month":7,"day":17,"hour":4,"minute":0,"second":0},
            "end_time": {"year":2026,"month":7,"day":29,"hour":3,"minute":59,"second":59},
            "rank_percent": 4415,
            "total_score": 68427,
            "total_star": 7,
            "total_max_score": 195000,
            "room_max_score": 65000,
            "zone_id": 69041,
            "has_data": true,
            "nick_name": "test",
            "avatar_icon": "https://...",
            "list": [{
                "avatar_list": [{
                    "id": 1541, "level": 60, "rank": 0, "rarity": "S",
                    "element_type": 202, "avatar_profession": 3,
                    "sub_element_type": 0, "role_square_url": "https://..."
                }],
                "buddy": {"id": 54004, "level": 60, "rarity": "S", "bangboo_rectangle_url": "https://..."},
                "boss": [{"name": "Test Boss", "icon": "https://...", "bg_icon": "https://...", "race_icon": "https://..."}],
                "buffer": [{"desc": "Test buff", "icon": "https://...", "name": "Buff Name"}],
                "challenge_time": {"year":2026,"month":7,"day":17,"hour":6,"minute":48,"second":10},
                "score": 22734,
                "star": 3,
                "total_star": 3
            }]
        }"#;

        let da: ZZZDeadlyAssault = serde_json::from_str(json).unwrap();
        assert_eq!(da.total_star, Some(7));
        assert_eq!(da.total_score, Some(68427));
        assert_eq!(da.rank_percent, Some(4415.0));
        assert_eq!(da.list.len(), 1);

        let room = &da.list[0];
        assert_eq!(room.star, Some(3));
        assert_eq!(room.score, Some(22734));
        assert_eq!(room.avatar_list[0].id, 1541);
        assert_eq!(room.boss[0].name, "Test Boss");
        assert_eq!(room.buffer[0].title.as_deref(), Some("Buff Name"));
        assert_eq!(room.buffer[0].description.as_deref(), Some("Test buff"));
    }

    #[test]
    fn test_deser_shiyu_defense() {
        let json = r#"{
            "zone_id": 62052,
            "hadal_begin_time": {"year":2026,"month":7,"day":10,"hour":4,"minute":0,"second":0},
            "hadal_end_time": {"year":2026,"month":7,"day":24,"hour":3,"minute":59,"second":59},
            "pass_fifth_floor": true,
            "begin_time": "1783652400",
            "end_time": "1784861999",
            "brief": {
                "cur_period_zone_layer_count": 5,
                "max_score": 150000,
                "score": 92233,
                "rating": "A",
                "rank_percent": 4852
            },
            "fourth_layer_detail": {
                "buffer": {"title": "Glacial Gale", "text": "Buff desc"},
                "challenge_time": {"year":2026,"month":7,"day":10,"hour":6,"minute":18,"second":18},
                "rating": "S",
                "layer_challenge_info_list": [{
                    "layer_id": 62052041,
                    "avatar_list": [{
                        "id": 1261, "level": 60, "rank": 0, "rarity": "S",
                        "element_type": 200, "avatar_profession": 3,
                        "sub_element_type": 0, "role_square_url": "https://..."
                    }],
                    "buddy": {"id": 54004, "level": 60, "rarity": "S", "bangboo_rectangle_url": "https://..."},
                    "challenge_time": {"year":2026,"month":7,"day":10,"hour":6,"minute":18,"second":18}
                }]
            }
        }"#;

        let sd: ZZZShiyuDefense = serde_json::from_str(json).unwrap();
        assert_eq!(sd.zone_id, Some(62052));
        assert!(sd.pass_fifth_floor.unwrap());

        let brief = sd.brief.unwrap();
        assert_eq!(brief.score, Some(92233));
        assert_eq!(brief.rating.as_deref(), Some("A"));

        let layer = sd.layers.get("fourth_layer_detail").unwrap();
        assert_eq!(layer.rating.as_deref(), Some("S"));
        assert_eq!(
            layer.buffer.as_ref().unwrap().title.as_deref(),
            Some("Glacial Gale")
        );
        assert_eq!(
            layer.buffer.as_ref().unwrap().description.as_deref(),
            Some("Buff desc")
        );
        assert_eq!(layer.layer_challenge_info_list[0].avatar_list[0].id, 1261);
    }

    #[test]
    fn test_deser_avatar_list() {
        let json = r#"{
            "avatar_list": [{
                "id": 1561,
                "level": 60,
                "name_mi18n": "Велина",
                "full_name_mi18n": "Велина Эйргид",
                "element_type": 204,
                "camp_name_mi18n": "Розкелифер",
                "avatar_profession": 3,
                "rarity": "S",
                "group_icon_path": "https://...",
                "hollow_icon_path": "https://...",
                "rank": 1,
                "is_chosen": false,
                "role_square_url": "https://...",
                "sub_element_type": 0,
                "awaken_state": "AwakenStateNotVisible"
            }]
        }"#;

        let al: ZZZAvatarList = serde_json::from_str(json).unwrap();
        assert_eq!(al.avatar_list.len(), 1);
        assert_eq!(al.avatar_list[0].name_mi18n, "Велина");
        assert_eq!(al.avatar_list[0].rarity, "S");
        assert_eq!(al.avatar_list[0].rank, 1);
    }
}
