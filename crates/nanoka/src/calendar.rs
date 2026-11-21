use crate::types::{EndgameType, SeasonMeta};
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use std::collections::HashMap;

/// The shared season calendar follows the Europe game server year-round.
pub const EUROPE_SERVER: FixedOffset = FixedOffset::east_opt(3600).unwrap();
pub const MOSCOW: FixedOffset = FixedOffset::east_opt(3 * 3600).unwrap();
pub const WALL_TIME: &str = "%Y-%m-%d %H:%M:%S";

pub fn parse_server_time(value: &str, zone: FixedOffset) -> Option<DateTime<Utc>> {
    let local = NaiveDateTime::parse_from_str(value, WALL_TIME).ok()?;
    Some(zone.from_local_datetime(&local).single()?.to_utc())
}

pub fn parse_europe_time(value: &str) -> Option<DateTime<Utc>> {
    parse_server_time(value, EUROPE_SERVER)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeasonPosition {
    Previous,
    Current,
    Next,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSeason {
    pub id: u64,
    pub sequence_id: u64,
    pub is_preview: bool,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
}

pub fn indexed_season(
    id: &str,
    meta: &SeasonMeta,
    endgame_type: EndgameType,
    sort: u32,
) -> Option<IndexedSeason> {
    if !(id.len() == 5 || id.len() == 6) || meta.sort != sort {
        return None;
    }
    let id = id.parse::<u64>().ok()?;
    if EndgameType::from_id(id) != Some(endgame_type) {
        return None;
    }
    let (begin, end) = match meta.live_begin.as_deref() {
        Some(start) => (start, meta.live_end.as_deref()),
        None => (meta.begin.as_deref()?, meta.end.as_deref()),
    };
    let sequence_id = if id >= 100_000 { id / 10 } else { id };
    Some(IndexedSeason {
        id,
        sequence_id,
        is_preview: id != sequence_id && meta.live_begin.is_none(),
        starts_at: parse_europe_time(begin)?,
        ends_at: end.and_then(parse_europe_time),
    })
}

pub fn select_indexed_season(
    seasons: &HashMap<String, SeasonMeta>,
    endgame_type: EndgameType,
    sort: u32,
    position: SeasonPosition,
    now: DateTime<Utc>,
) -> Option<IndexedSeason> {
    let entries: Vec<_> = seasons
        .iter()
        .filter_map(|(id, meta)| indexed_season(id, meta, endgame_type, sort))
        .collect();
    let current = current_live_season(&entries, now)?;
    let selected = match position {
        SeasonPosition::Current => Some(current),
        SeasonPosition::Previous => entries
            .iter()
            .filter(|s| !s.is_preview && s.starts_at < current.starts_at)
            .max_by_key(|s| (s.starts_at, s.id == s.sequence_id, s.id)),
        SeasonPosition::Next => {
            let next_live = entries
                .iter()
                .filter(|s| !s.is_preview && s.starts_at > current.starts_at)
                .min_by_key(|s| (s.starts_at, s.id != s.sequence_id, s.id));
            let preview = entries
                .iter()
                .filter(|s| s.is_preview && s.sequence_id == current.sequence_id + 1)
                .min_by_key(|s| s.id);
            match next_live {
                Some(live) if live.sequence_id == current.sequence_id + 1 => Some(live),
                _ => preview.or(next_live),
            }
        }
    };
    selected.cloned()
}

pub fn current_live_season(
    entries: &[IndexedSeason],
    now: DateTime<Utc>,
) -> Option<&IndexedSeason> {
    entries
        .iter()
        .filter(|s| !s.is_preview)
        .filter(|s| {
            s.starts_at <= now
                && s.ends_at
                    .is_none_or(|end| now.timestamp() <= end.timestamp())
        })
        .max_by_key(|s| (s.starts_at, s.id == s.sequence_id, s.id))
}

pub fn endgame_index_sort(endgame_type: EndgameType) -> u32 {
    match endgame_type {
        EndgameType::ShiyuDefence => 1,
        EndgameType::DeadlyAssault => 9,
    }
}
