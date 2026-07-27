use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::models::zzz::ZZZAvatarInfo;

static AGENT_NAMES: OnceLock<RwLock<HashMap<i64, String>>> = OnceLock::new();

fn cache() -> &'static RwLock<HashMap<i64, String>> {
    AGENT_NAMES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Populate the agent name cache from an avatar list.
pub fn cache_avatars(avatars: &[ZZZAvatarInfo]) {
    let mut map = cache().write().unwrap();
    for av in avatars {
        map.insert(av.id, av.name_mi18n.clone());
    }
}

/// Try to resolve an agent ID to a name.
///
/// Check the dynamic cache, then the static database.
/// Return the ID as a string if neither source contains a name.
pub fn resolve_name(id: i64) -> String {
    if let Some(name) = cache().read().unwrap().get(&id) {
        return name.clone();
    }
    if let Some(name) = super::agent_db::static_resolve(id) {
        return name.to_string();
    }
    id.to_string()
}

/// Check if the dynamic cache has any entries.
pub fn is_cached() -> bool {
    !cache().read().unwrap().is_empty()
}
