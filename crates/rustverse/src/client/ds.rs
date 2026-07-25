use rand::Rng;

/// The known salt for global (overseas) HoYoLAB API.
pub const SALT: &str = "6s25p5ox5y14umn1p61aqyyvbvvl3lrt";

/// Generate a `DS` header value: `{timestamp},{random},{md5}`
///
/// - `timestamp` — current Unix timestamp in seconds
/// - `random` — 6 random lowercase alphanumeric characters
/// - `hash` — md5(`salt={SALT}&t={timestamp}&r={random}`)
pub fn generate_ds() -> String {
    let now = chrono::Utc::now().timestamp();
    let random: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(6)
        .map(char::from)
        .collect::<String>()
        .to_lowercase();

    let input = format!("salt={SALT}&t={now}&r={random}");
    let hash = format!("{:x}", md5::compute(input.as_bytes()));

    format!("{now},{random},{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ds_format() {
        let ds = generate_ds();
        let parts: Vec<&str> = ds.split(',').collect();
        assert_eq!(parts.len(), 3, "DS should have 3 comma-separated parts");
        // timestamp is all digits
        assert!(parts[0].chars().all(|c| c.is_ascii_digit()));
        // random is 6 alphanumeric
        assert_eq!(parts[1].len(), 6);
        // hash is 32 hex chars
        assert_eq!(parts[2].len(), 32);
    }
}
