//! Versioned Nanoka calendars and season pages for interactive questions.
use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use nanoka::{
    GameManifest, NanokaClient,
    calendar::{
        IndexedSeason, MOSCOW, SeasonPosition, current_live_season, endgame_index_sort,
        indexed_season, select_indexed_season,
    },
    types::{AnySeasonDetail, EndgameType, SeasonMeta, Zone},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::kit::ToolSpec;

const MAX_SEASON_REQUESTS: usize = 8;

pub(crate) fn definitions() -> [ToolSpec; 3] {
    let mode = json!({"type":"string","enum":["shiyu_defense","deadly_assault"]});
    let offset = json!({"type":["integer","null"],"minimum":0,"description":"Start with null. Continue with next_offset."});
    let version = crate::kit::version_parameter();
    let detail = json!({"type":"object","additionalProperties":false,"properties":{"mode":mode,"version":version,"season":{"type":"string","description":"current, next, or a five/six-digit season ID"},"offset":offset,"selector":{"type":["string","null"],"description":"Null for the full page, or an exact /zones/... JSON pointer."}},"required":["mode","season","offset","selector","version"]});
    [
        ToolSpec {
            name: "list_endgame_seasons",
            description: "Browse Nanoka's season calendar. Current and upcoming seasons first, then past rotations. Each result identifies the mode, ID, schedule status and official Moscow dates. A preview has no confirmed dates.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{"mode":{"anyOf":[mode,{"type":"null"}]},"offset":offset,"version":version},"required":["mode","offset","version"]}),
        },
        ToolSpec {
            name: "get_endgame_season",
            description: "Read season rooms, monster IDs, stats and buffs. Deadly Assault has separate normal and hard floors: agents used in normal rooms are available again on hard. For boss mechanics read the monster ID in GachaBase category=enemies. Use current or next for a fresh relative question, or an exact ID for a follow-up. Pages cover all floors and zones. Use /zones pointers on the same page for a focused read or compute.",
            parameters: detail.clone(),
        },
        ToolSpec {
            name: "get_endgame_seasons",
            description: "Read multiple independent season pages in one call. To compare builds, request the same mode and season ID once per exact version. Each result corresponds to one request in input order and has the same fields, pagination, source and team rules as get_endgame_season, or an individual error.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{"requests":{"type":"array","minItems":1,"maxItems":MAX_SEASON_REQUESTS,"items":detail}},"required":["requests"]}),
        },
    ]
}

pub(crate) struct SeasonTools {
    client: NanokaClient,
    manifest: Option<GameManifest>,
    snapshots: HashMap<String, SeasonSnapshot>,
}

pub(crate) struct SeasonSnapshot {
    pub(crate) version: String,
    client: NanokaClient,
    indexes: HashMap<EndgameType, HashMap<String, SeasonMeta>>,
    details: HashMap<u64, Vec<Value>>,
}

fn mode_name(mode: EndgameType) -> &'static str {
    match mode {
        EndgameType::ShiyuDefence => "shiyu_defense",
        EndgameType::DeadlyAssault => "deadly_assault",
    }
}

fn mode(value: &str) -> Result<EndgameType> {
    match value {
        "shiyu_defense" => Ok(EndgameType::ShiyuDefence),
        "deadly_assault" => Ok(EndgameType::DeadlyAssault),
        _ => bail!("Unknown season mode"),
    }
}

fn metadata(
    season: &IndexedSeason,
    mode: EndgameType,
    current: Option<u64>,
    now: DateTime<Utc>,
) -> Value {
    let status = if season.is_preview {
        "preview"
    } else if Some(season.id) == current {
        "live"
    } else if season.starts_at > now {
        "upcoming"
    } else {
        "past"
    };
    let date = |time: DateTime<Utc>| time.with_timezone(&MOSCOW).to_rfc3339();
    json!({"season_id":season.id, "mode":mode_name(mode), "status":status,
        "starts_at":(!season.is_preview).then(||date(season.starts_at)),
        "ends_at":season.ends_at.filter(|_|!season.is_preview).map(date)})
}

fn zones(mode_id: u64, zone_type: Option<u64>, zones: &HashMap<String, Zone>) -> Vec<Value> {
    let mut sorted: Vec<_> = zones.iter().collect();
    sorted.sort_by_key(|(id, zone)| (zone.stage_num, id.parse::<u64>().unwrap_or_default(), *id));
    sorted
        .into_iter()
        .map(|(id, zone)| {
            let mut data = json!(zone);
            if let Some(rooms) = data["layer_room"].as_object_mut() {
                for room in rooms.values_mut() {
                    if let Some(fields) = room.as_object_mut() {
                        fields.remove("monster_icon");
                    }
                    if let Some(monsters) = room["monster_list"].as_object_mut() {
                        for monster in monsters.values_mut().filter_map(Value::as_object_mut) {
                            monster.remove("image");
                        }
                    }
                }
            }
            let mut row = json!({"mode_id":mode_id,"zone_type":zone_type,"zone_id":id,"zone":data});
            match zone_type {
                Some(1001) => row["floor"] = json!("normal"),
                Some(1002) => row["floor"] = json!("hard"),
                _ => {}
            }
            row
        })
        .collect()
}

pub(crate) fn batch_requests(args: Value) -> Result<Vec<Value>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Batch {
        requests: Vec<Value>,
    }
    let batch: Batch = serde_json::from_value(args)?;
    ensure!(
        (1..=MAX_SEASON_REQUESTS).contains(&batch.requests.len()),
        "Expected 1 to {MAX_SEASON_REQUESTS} season requests"
    );
    Ok(batch.requests)
}

impl SeasonTools {
    pub(crate) fn new(client: NanokaClient) -> Self {
        Self {
            client,
            manifest: None,
            snapshots: HashMap::new(),
        }
    }

    pub(crate) async fn snapshot(
        &mut self,
        requested: Option<&str>,
    ) -> Result<(String, &mut SeasonSnapshot)> {
        if self.manifest.is_none() {
            self.manifest = Some(self.client.manifest().await?);
        }
        let manifest = self.manifest.as_ref().unwrap();
        let version = match requested {
            None => &manifest.latest,
            Some("release") => manifest
                .live
                .as_deref()
                .context("Nanoka has no live version")?,
            Some(exact) => exact,
        };
        let build = if let Some((_, numbers)) = version.split_once('_') {
            crate::beta::revision_group(version)?;
            numbers.replace('_', "+")
        } else {
            version.to_owned()
        };
        ensure!(
            manifest.available.contains(&build)
                || manifest.latest == build
                || manifest.live.as_ref() == Some(&build),
            "Nanoka has no snapshot for {version} ({build})"
        );
        let snapshot = self
            .snapshots
            .entry(build.clone())
            .or_insert_with(|| SeasonSnapshot::new(self.client.clone(), build));
        Ok((version.to_owned(), snapshot))
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        let mut tools = Self::new(NanokaClient::new());
        let version = "1.0.0+2";
        let builds = [
            (version, 60),
            ("1.0", 50),
            ("3.4.5+20713015", 65),
            ("3.4.12+20749856", 70),
        ];
        tools.manifest = Some(GameManifest {
            latest: version.into(),
            live: Some("1.0".into()),
            available: builds.iter().map(|(build, _)| build.to_string()).collect(),
        });
        for (build, level) in builds {
            let mut snapshot = SeasonSnapshot::new(tools.client.clone(), build.into());
            snapshot.add_fixture(level);
            tools.snapshots.insert(build.into(), snapshot);
        }
        tools
    }
}

impl SeasonSnapshot {
    #[cfg(test)]
    fn add_fixture(&mut self, level: u64) {
        for (mode, id, sort) in [
            (EndgameType::ShiyuDefence, "62053", 1),
            (EndgameType::DeadlyAssault, "69041", 9),
        ] {
            self.indexes.insert(
                mode,
                serde_json::from_value(
                    json!({id:{"en":"Trial","sort":sort,"begin":"2000-01-01 04:00:00"}}),
                )
                .unwrap(),
            );
        }
        let fixture = serde_json::from_value(
            json!({"123":{"name":"Test enemy","stage_num":5,"monster_level":level}}),
        )
        .unwrap();
        self.details.insert(62053, zones(62053, None, &fixture));
    }

    fn new(client: NanokaClient, version: String) -> Self {
        Self {
            client: client.with_version(&version),
            version,
            indexes: HashMap::new(),
            details: HashMap::new(),
        }
    }
    async fn index(&mut self, ty: EndgameType) -> Result<&HashMap<String, SeasonMeta>> {
        let entry = self.indexes.entry(ty);
        Ok(match entry {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(self.client.get_seasons_by_type(ty).await?)
            }
        })
    }

    pub async fn call(
        &mut self,
        name: &str,
        args: Value,
        sources: &mut HashMap<String, Value>,
        now: DateTime<Utc>,
    ) -> Result<Value> {
        if name == "get_endgame_season" {
            return self.read_page(args, now, sources).await;
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Query {
            mode: Option<String>,
            offset: Option<usize>,
        }
        let q: Query = serde_json::from_value(args)?;
        let ty = q.mode.as_deref().map(mode).transpose()?;
        let offset = q.offset.unwrap_or(0);
        self.list(ty, offset, now).await
    }

    async fn read_page(
        &mut self,
        args: Value,
        now: DateTime<Utc>,
        sources: &mut HashMap<String, Value>,
    ) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Page {
            mode: String,
            season: String,
            offset: Option<usize>,
            selector: Option<String>,
        }
        let q: Page = serde_json::from_value(args)?;
        let mut result = self
            .detail(mode(&q.mode)?, &q.season, q.offset.unwrap_or_default(), now)
            .await?;
        if let Some(source) = result["source"].as_str() {
            sources
                .entry(source.to_owned())
                .or_insert_with(|| result.clone());
            if let Some(path) = q.selector {
                ensure!(
                    path.starts_with("/zones/"),
                    "Expected a pointer into the zones of this page"
                );
                let value = result.pointer(&path).context("Unknown zone field")?.clone();
                result.as_object_mut().unwrap().remove("zones");
                result["path"] = json!(path);
                result["value"] = value;
            }
        }
        Ok(result)
    }

    async fn list(
        &mut self,
        ty: Option<EndgameType>,
        offset: usize,
        now: DateTime<Utc>,
    ) -> Result<Value> {
        let mut rows = Vec::new();
        for mode in [EndgameType::ShiyuDefence, EndgameType::DeadlyAssault] {
            if ty.is_some_and(|ty| ty != mode) {
                continue;
            }
            let entries: Vec<_> = self
                .index(mode)
                .await?
                .iter()
                .filter_map(|(id, m)| indexed_season(id, m, mode, endgame_index_sort(mode)))
                .collect();
            let current = current_live_season(&entries, now).map(|s| s.id);
            for s in entries {
                let item = metadata(&s, mode, current, now);
                let order = match item["status"].as_str() {
                    Some("live") => 0,
                    Some("upcoming") => 1,
                    Some("preview") => 2,
                    _ => 3,
                };
                let time = s.starts_at.timestamp();
                rows.push(((order, if order == 3 { -time } else { time }, s.id), item));
            }
        }
        rows.sort_by_key(|r| r.0);
        ensure!(offset <= rows.len(), "Invalid season offset");
        let end = offset.saturating_add(24).min(rows.len());
        Ok(
            json!({"seasons":rows[offset..end].iter().map(|r|&r.1).collect::<Vec<_>>(),"next_offset":(end<rows.len()).then_some(end)}),
        )
    }

    async fn detail(
        &mut self,
        mode: EndgameType,
        selector: &str,
        offset: usize,
        now: DateTime<Utc>,
    ) -> Result<Value> {
        let index = self.index(mode).await?;
        let sort = endgame_index_sort(mode);
        let current = select_indexed_season(index, mode, sort, SeasonPosition::Current, now);
        let season = match selector {
            "current" => current.clone(),
            "next" => select_indexed_season(index, mode, sort, SeasonPosition::Next, now),
            id => {
                ensure!(
                    matches!(id.len(), 5 | 6) && id.bytes().all(|b| b.is_ascii_digit()),
                    "Invalid season selector"
                );
                index
                    .get(id)
                    .and_then(|m| indexed_season(id, m, mode, sort))
            }
        };
        let Some(season) = season else {
            return Ok(json!({"status":"unavailable","mode":mode_name(mode),"selector":selector}));
        };
        let mut result = metadata(&season, mode, current.map(|s| s.id), now);
        let id = season.id;
        if let std::collections::hash_map::Entry::Vacant(entry) = self.details.entry(id) {
            let rows = match mode {
                EndgameType::DeadlyAssault => {
                    let detail = self.client.get_boss_detail_resolved(id, None).await?;
                    ensure!(detail.id == id, "Unexpected season ID");
                    detail
                        .modes
                        .iter()
                        .flat_map(|m| zones(m.id, Some(m.zone_type), &m.zone))
                        .collect()
                }
                EndgameType::ShiyuDefence => {
                    let AnySeasonDetail::Shiyu(detail) =
                        self.client.get_detail_resolved(id).await?
                    else {
                        bail!("Unexpected season type")
                    };
                    ensure!(detail.id == id, "Unexpected season ID");
                    zones(detail.id, None, &detail.zone)
                }
            };
            entry.insert(rows);
        }
        let rows = &self.details[&id];
        if mode == EndgameType::DeadlyAssault {
            result["floors"] = json!(
                ["normal", "hard"]
                    .into_iter()
                    .filter(|floor| rows.iter().any(|row| row["floor"] == *floor))
                    .collect::<Vec<_>>()
            );
            result["team_rules"] = json!({"reuse_between_normal_rooms":false});
            if rows.iter().any(|row| row["floor"] == "hard") {
                result["team_rules"]["reuse_from_normal_on_hard"] = json!(true);
            }
        }
        ensure!(offset <= rows.len(), "Invalid zone offset");
        // Include the first remaining zone even when it alone exceeds the page budget.
        let mut end = offset;
        let mut bytes = 0;
        while end < rows.len() {
            let size = serde_json::to_vec(&rows[end])?.len();
            if end > offset && bytes + size > 16_384 {
                break;
            }
            bytes += size;
            end += 1;
        }
        result["requested_season"] = json!(selector);
        result["offset"] = json!(offset);
        result["source"] = json!(format!(
            "nanoka/{}/{}/{id}/page/{offset}",
            self.version,
            mode_name(mode)
        ));
        result["zones"] = json!(&rows[offset..end]);
        result["next_offset"] = json!((end < rows.len()).then_some(end));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_followups_survive_a_rotation_and_preview_dates_stay_unconfirmed() {
        let ty = EndgameType::DeadlyAssault;
        let mut tools = SeasonSnapshot::new(NanokaClient::new(), "1.0".into());
        let index = serde_json::from_value(json!({
            "69041":{"en":"Trial","sort":9,"begin":"2026-11-01 04:00:00","end":"2026-11-15 03:59:59"},
            "690421":{"en":"Trial","sort":9,"begin":"2026-01-01 00:00:00","live_begin":"2026-11-15 04:00:00"},
            "690431":{"en":"Preview","sort":9,"begin":"2026-01-01 00:00:00"}
        })).unwrap();
        tools.indexes.insert(ty, index);
        let zone = serde_json::from_value(json!({
            "1":{"name":"Test boss","stage_num":1,"monster_level":70}
        }))
        .unwrap();
        for id in [69041, 690421, 690431] {
            tools.details.insert(
                id,
                zones(id + 1, Some(1002), &zone)
                    .into_iter()
                    .chain(zones(id, Some(1001), &zone))
                    .collect(),
            );
        }
        let time = |date: &str| DateTime::parse_from_rfc3339(date).unwrap().to_utc();
        let before = time("2026-11-15T02:59:59Z");
        let after = time("2026-11-15T03:00:00Z");
        let selected = tools.detail(ty, "next", 0, before).await.unwrap();
        assert_eq!(selected["season_id"], 690421);
        assert_eq!(selected["starts_at"], "2026-11-15T06:00:00+03:00");
        assert_eq!(
            tools.detail(ty, "690421", 0, after).await.unwrap()["status"],
            "live"
        );
        let preview = tools.detail(ty, "next", 0, after).await.unwrap();
        assert_eq!(preview["season_id"], 690431);
        assert_eq!(preview["status"], "preview");
        assert!(preview["starts_at"].is_null() && preview["ends_at"].is_null());
        assert_eq!(preview["zones"].as_array().unwrap().len(), 2);
        assert_eq!(preview["floors"], json!(["normal", "hard"]));
        assert_eq!(preview["zones"][0]["floor"], "hard");
        assert_eq!(preview["zones"][1]["floor"], "normal");
        assert_eq!(preview["team_rules"]["reuse_between_normal_rooms"], false);
        assert_eq!(preview["team_rules"]["reuse_from_normal_on_hard"], true);
        let later_page = tools.detail(ty, "690431", 1, after).await.unwrap();
        assert_eq!(later_page["team_rules"], preview["team_rules"]);
        let listed = tools.list(Some(ty), 0, after).await.unwrap();
        let rows = listed["seasons"].as_array().unwrap();
        assert!(rows.iter().all(|row| row["mode"] == "deadly_assault"));
        assert_eq!(
            rows.iter()
                .map(|r| r["status"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["live", "preview", "past"]
        );
    }
}
