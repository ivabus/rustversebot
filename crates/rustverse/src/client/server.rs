/// Auto-detect ZZZ server from UID (first 2 digits).
///
/// | Prefix | Server       |
/// |--------|-------------|
/// | 10     | prod_gf_us  |
/// | 13     | prod_gf_jp  |
/// | 15     | prod_gf_eu  |
/// | 17     | prod_gf_sg  |
pub fn detect_server(uid: &str) -> Option<&'static str> {
    if uid.len() < 2 {
        return None;
    }
    match &uid[..2] {
        "10" => Some("prod_gf_us"),
        "13" => Some("prod_gf_jp"),
        "15" => Some("prod_gf_eu"),
        "17" => Some("prod_gf_sg"),
        _ => None,
    }
}
