//! Compact default views for model lookups. The cached DTO stays intact for JSON pointers.
use std::sync::LazyLock;

use anyhow::{Result, ensure};
use regex::Regex;
use serde_json::{Value, json};

use crate::{catalog::Category, kit::PAGE_CHARS};

static COLOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<color=#[0-9a-f]{6}(?:[0-9a-f]{2})?>|</color>").unwrap());

fn fields(source: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for &key in keys {
        let Some(value) = source.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        if value.as_array().is_some_and(Vec::is_empty) {
            continue;
        }
        let text = value
            .as_str()
            .or_else(|| value.get("text").and_then(Value::as_str));
        out[key] = match text {
            Some(text) => {
                let clean = COLOR.replace_all(text, "");
                match value.get("override").filter(|v| !v.is_null()) {
                    Some(override_text) => json!({"text":clean,"override":override_text}),
                    None => json!(clean),
                }
            }
            None => value.clone(),
        };
    }
    out
}

fn rows<'a>(entry: &'a Value, key: &str) -> impl Iterator<Item = (usize, &'a Value)> {
    entry[key].as_array().into_iter().flatten().enumerate()
}

/// Keeps the source pointer even for views which normally need only a page or rank number.
struct Record {
    path: String,
    data: Value,
}

fn agent_records(entry: &Value) -> Vec<Record> {
    let mut records = Vec::new();
    for (section, children, skill_type) in [
        ("core_skills", "skills", "skill_type_id"),
        ("skills", "descriptions", "agent_skill_type_id"),
    ] {
        for (i, group) in rows(entry, section) {
            if group[children].as_array().is_none_or(Vec::is_empty) {
                let path = format!("/{section}/{i}");
                records.push(Record {
                    data: json!({"read":path}),
                    path,
                });
                continue;
            }
            for (j, skill) in rows(group, children) {
                let mut data = fields(skill, &["id", "potential_ids", "name", "description"]);
                if data.get("description").is_none()
                    && let Some(text) =
                        fields(skill, &["short_description"]).get("short_description")
                {
                    data["description"] = text.clone();
                }
                for key in [skill_type, "level", "potential_ids"] {
                    if data.get(key).is_none()
                        && let Some(value) = group.get(key)
                        && !value.is_null()
                        && !value.as_array().is_some_and(Vec::is_empty)
                    {
                        data[key] = value.clone();
                    }
                }
                let path = format!("/{section}/{i}/{children}/{j}");
                data["path"] = json!(path);
                records.push(Record { path, data });
            }
        }
    }
    for (i, row) in rows(entry, "mindscapes") {
        let path = format!("/mindscapes/{i}");
        let mut data = fields(row, &["id", "level", "name", "description"]);
        data["path"] = json!(path);
        records.push(Record { path, data });
    }
    records
}

fn bangboo_records(entry: &Value) -> Vec<Record> {
    let mut records = Vec::new();
    for (i, skill) in rows(entry, "skills") {
        let mut levels = rows(skill, "levels").peekable();
        if levels.peek().is_none() {
            let path = format!("/skills/{i}");
            records.push(Record {
                data: json!({"read":path}),
                path,
            });
        }
        while let Some((j, first)) = levels.next() {
            let mut data = fields(first, &["name", "description"]);
            let start = first["level"].as_u64();
            let mut end = start;
            // Only the description is shared; numerical skill stats can vary at every rank.
            while let Some((_, next)) = levels.peek() {
                let next_level = next["level"].as_u64();
                if next_level.is_none()
                    || end.and_then(|n| n.checked_add(1)) != next_level
                    || fields(next, &["name", "description"]) != data
                {
                    break;
                }
                end = next_level;
                levels.next();
            }
            data["level"] = first["level"].clone();
            if end != start {
                data["through_level"] = json!(end);
            }
            data["bangboo_skill_type_id"] = skill["bangboo_skill_type_id"].clone();
            let path = format!("/skills/{i}/levels/{j}");
            data["path"] = json!(path);
            records.push(Record { path, data });
        }
    }
    records
}

pub(super) fn page(category: Category, entry: &Value, offset: usize) -> Result<Value> {
    let (header, section, keys): (&[&str], &str, &[&str]) = match category {
        Category::Tutorials => (
            &["name"],
            "pages",
            &["page", "text", "image_path_hash", "video_playlist_url"],
        ),
        Category::WEngines => (
            &[
                "name",
                "rarity_id",
                "specialty_id",
                "base_stat",
                "advanced_stat",
            ],
            "effects",
            &["level", "name", "description"],
        ),
        Category::DriveDiscs => (
            &["name", "tag"],
            "set_effects",
            &["pieces", "description", "stats"],
        ),
        Category::Agents => (
            &["name", "rarity_id", "element_ids", "specialty_id"],
            "skills",
            &[],
        ),
        Category::Bangboo => (&["name", "rarity_id"], "skills", &[]),
    };
    let records = match category {
        Category::Agents => agent_records(entry),
        Category::Bangboo => bangboo_records(entry),
        _ => rows(entry, section)
            .map(|(i, row)| Record {
                path: format!("/{section}/{i}"),
                data: fields(row, keys),
            })
            .collect(),
    };
    record_page(fields(entry, header), section, records, offset)
}

/// Use GachaBase's concise passive descriptions to shortlist partners. Keep the
/// exact skill pointer for checking a trigger or number before drawing a conclusion.
pub(super) fn team(entry: &Value, offset: usize) -> Result<Value> {
    let mut records = Vec::new();
    for (i, core) in rows(entry, "core_skills") {
        for (j, skill) in rows(core, "skills") {
            let path = format!("/core_skills/{i}/skills/{j}");
            let short = skill
                .pointer("/short_description/text")
                .or_else(|| skill.get("short_description"))
                .and_then(Value::as_str);
            let description = if short.is_some_and(|s| !s.trim().is_empty()) {
                "short_description"
            } else {
                "description"
            };
            let mut data = fields(skill, &["name", description]);
            data["path"] = json!(path);
            records.push(Record { path, data });
        }
    }
    record_page(
        fields(
            entry,
            &["name", "specialty_id", "element_ids", "faction_id"],
        ),
        "passives",
        records,
        offset,
    )
}

fn record_page(
    mut result: Value,
    section: &str,
    records: Vec<Record>,
    offset: usize,
) -> Result<Value> {
    let total = records.len();
    ensure!(
        offset < total || (total == 0 && offset == 0),
        "Offset exceeds record count ({total})"
    );
    // Leave room for the next offset and the array delimiters. Large unexpected
    // metadata remains accessible through its pointer as well.
    if result.to_string().chars().count() > PAGE_CHARS / 4 {
        result = json!({"read":"@raw"});
    }
    if total == 0 {
        result["read"] = json!("@raw");
    }
    result[section] = json!([]);
    let mut size = result.to_string().chars().count() + 32;
    let item_limit = PAGE_CHARS - size - 1;
    let mut items = Vec::new();
    for record in records.into_iter().skip(offset) {
        let mut data = record.data;
        let mut length = data.to_string().chars().count();
        if length > item_limit || data.as_object().is_some_and(|o| o.is_empty()) {
            // Do not silently truncate mechanics. A caller can read this record or its fields.
            data = fields(&data, &["id", "page", "level", "through_level", "pieces"]);
            data["read"] = json!(record.path);
            length = data.to_string().chars().count();
            if length > item_limit {
                data = json!({"read":record.path});
                length = data.to_string().chars().count();
            }
        }
        if !items.is_empty() && size + length + 1 > PAGE_CHARS {
            break;
        }
        size += length + 1;
        items.push(data);
    }
    let end = offset + items.len();
    result[section] = Value::Array(items);
    if end < total {
        result["next_offset"] = json!(end);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_view_includes_both_passives_and_links_to_the_full_mechanic() {
        let entry = json!({"core_skills":[{"skills":[
            {"name":{"text":"Core"},"short_description":{"text":"Grant a mark"},"description":{"text":"Full condition"}},
            {"name":{"text":"Additional Ability"},"description":{"text":"Requires another Anomaly agent"}}
        ]}],"skills":[{"name":"Unrelated attack"}]});
        let view = team(&entry, 0).unwrap();
        let rows = view["passives"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["short_description"], "Grant a mark");
        assert_eq!(
            entry.pointer(rows[0]["path"].as_str().unwrap()).unwrap()["description"]["text"],
            "Full condition"
        );
        assert_eq!(rows[1]["description"], "Requires another Anomaly agent");
    }

    #[test]
    fn tutorial_pages_keep_meaning_and_resume_at_record_boundaries() {
        let text = "条件 <color=#FFA9DD>30%</color> <IconMap:Attack>\n".repeat(100);
        let entry = json!({"name":{"key":"k","text":"指南"},"pages":[
            {"page":1,"text":{"key":"p1","text":text}},{"page":2,"text":{"text":text}},
            {"page":3,"text":{"text":"火".repeat(PAGE_CHARS+1)}}]});
        let first = page(Category::Tutorials, &entry, 0).unwrap();
        assert_eq!(
            first["pages"][0]["text"],
            "条件 30% <IconMap:Attack>\n".repeat(100)
        );
        assert_eq!(first["next_offset"], 1);
        let second = page(Category::Tutorials, &entry, 1).unwrap();
        assert_eq!(second["pages"][0]["page"], 2);
        assert_eq!(second["pages"][1]["read"], "/pages/2");
        assert_eq!(
            entry
                .pointer(second["pages"][1]["read"].as_str().unwrap())
                .unwrap()["page"],
            3
        );
        assert!(second.get("next_offset").is_none());
        assert!(page(Category::Tutorials, &entry, 3).is_err());
    }

    #[test]
    fn bangboo_groups_only_consecutive_levels_with_identical_mechanics() {
        let level = |n, text| json!({"level":n,"name":{"text":"Ice"},"description":{"text":text},"stats":[n]});
        let entry = json!({"skills":[{"bangboo_skill_type_id":2,"levels":[
            level(1,"10%"),level(2,"10%"),level(3,"20%"),level(5,"20%")]}]});
        let view = page(Category::Bangboo, &entry, 0).unwrap();
        let skills = view["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 3);
        assert_eq!(skills[0]["through_level"], 2);
        assert_eq!(skills[1]["description"], "20%");
        assert!(skills[1].get("through_level").is_none());
        assert_eq!(
            entry.pointer(skills[2]["path"].as_str().unwrap()).unwrap()["level"],
            5
        );
    }
}
