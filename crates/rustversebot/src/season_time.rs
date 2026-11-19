use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};

/// The shared season calendar follows the Europe game server year-round.
pub const EUROPE_SERVER: FixedOffset = FixedOffset::east_opt(3600).unwrap();
const MOSCOW: FixedOffset = FixedOffset::east_opt(3 * 3600).unwrap();
pub const WALL_TIME: &str = "%Y-%m-%d %H:%M:%S";

pub fn parse_server_time(value: &str, zone: FixedOffset) -> Option<DateTime<Utc>> {
    let local = NaiveDateTime::parse_from_str(value, WALL_TIME).ok()?;
    Some(zone.from_local_datetime(&local).single()?.to_utc())
}

pub fn parse_europe_time(value: &str) -> Option<DateTime<Utc>> {
    parse_server_time(value, EUROPE_SERVER)
}

pub fn moscow_time(time: DateTime<Utc>) -> String {
    time.with_timezone(&MOSCOW)
        .format("%d.%m.%Y %H:%M:%S МСК")
        .to_string()
}

pub fn moscow_date(time: DateTime<Utc>) -> String {
    time.with_timezone(&MOSCOW).format("%Y-%m-%d").to_string()
}

pub fn display_wall_time(value: &str, zone: FixedOffset) -> String {
    parse_server_time(value, zone)
        .map(moscow_time)
        .unwrap_or_else(|| "—".into())
}
