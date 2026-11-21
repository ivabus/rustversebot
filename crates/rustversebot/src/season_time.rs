use chrono::{DateTime, FixedOffset, Utc};
use nanoka::calendar::MOSCOW;
pub use nanoka::calendar::{EUROPE_SERVER, WALL_TIME, parse_europe_time, parse_server_time};

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
