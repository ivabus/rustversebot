use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use regex::{Captures, Regex};
use serde::Serialize;
use serde_json::{Value, json};

static TERM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<Term:(\d+)></Term>").unwrap());
const SKILL_LEVEL: i64 = 12;
static LINEAR_SKILL_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{CAL:([\d.]+)\+AvatarSkillLevel\(([0-9]+)\)\*([\d.]+),([\d.]+),([0-9])\}")
        .unwrap()
});

// In a CAL expression AvatarSkillLevel is the level itself. In a skill_data
// record the base is already the level-1 value, so only eleven steps are added.
fn skill_value(caps: &Captures<'_>, core_levels: &BTreeMap<u64, i64>) -> String {
    let number = |i: usize| caps[i].parse::<f64>().ok();
    let (Some(base), Ok(skill_type), Some(step), Some(scale)) =
        (number(1), caps[2].parse::<u64>(), number(3), number(4))
    else {
        return caps[0].to_owned();
    };
    let level = core_levels.get(&skill_type).copied().unwrap_or(SKILL_LEVEL);
    let value = (base + level as f64 * step) * scale;
    let precision: usize = caps[5].parse().expect("single decimal digit");
    let formatted = format!("{value:.precision$}");
    if precision == 0 {
        formatted
    } else {
        formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn select_report_levels(agent: &mut Value) {
    if let Some(core) = agent.get_mut("core_skills").and_then(Value::as_array_mut)
        && let Some(max) = core.iter().filter_map(|v| v["level"].as_u64()).max()
    {
        core.retain(|v| v["level"].as_u64() == Some(max) || v.get("level").is_none());
    }
    for section in ["skill_data", "unmapped_skill_data"] {
        let Some(rows) = agent.get_mut(section).and_then(Value::as_array_mut) else {
            continue;
        };
        for row in rows.iter_mut().filter_map(Value::as_object_mut) {
            let values: Vec<_> = row
                .keys()
                .filter_map(|key| {
                    let stat = key.strip_suffix("_step")?;
                    let base = row.get(&format!("{stat}_base"))?.as_i64()?;
                    let step = row[key].as_i64()?;
                    Some((
                        stat.to_owned(),
                        base.checked_add(step.checked_mul(SKILL_LEVEL - 1)?)?,
                    ))
                })
                .collect();
            for (stat, value) in values {
                row.remove(&format!("{stat}_base"));
                row.remove(&format!("{stat}_step"));
                if matches!(stat.as_str(), "damage_multiplier" | "daze_multiplier") {
                    row.insert(format!("{stat}_percent"), json!(value as f64 / 100.0));
                } else {
                    row.insert(stat, json!(value));
                }
            }
        }
    }
}

pub(crate) fn snapshot(mut agent: Value, mut tooltips: Value) -> Value {
    select_report_levels(&mut agent);
    let core_levels = agent["core_skills"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|core| Some((core["skill_type_id"].as_u64()?, core["level"].as_i64()?)))
        .collect();
    let names = tooltips
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, entry)| {
            Some((
                id.clone(),
                entry.pointer("/name/text")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    fn expand(
        value: &mut Value,
        names: &BTreeMap<String, String>,
        core_levels: &BTreeMap<u64, i64>,
    ) {
        match value {
            Value::String(s) => {
                if s.contains("AvatarSkillLevel(") {
                    *s = LINEAR_SKILL_VALUE
                        .replace_all(s, |caps: &Captures<'_>| skill_value(caps, core_levels))
                        .into_owned();
                }
                if s.contains("<Term:") {
                    *s = TERM
                        .replace_all(s, |m: &Captures<'_>| {
                            names
                                .get(&m[1])
                                .map(String::as_str)
                                .unwrap_or(&m[0])
                                .to_owned()
                        })
                        .into_owned();
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| expand(v, names, core_levels)),
            Value::Object(fields) => fields
                .values_mut()
                .for_each(|v| expand(v, names, core_levels)),
            _ => {}
        }
    }
    expand(&mut agent, &names, &core_levels);
    expand(&mut tooltips, &names, &core_levels);
    json!({"agent": agent, "tooltips": tooltips})
}

#[derive(Serialize, Debug)]
pub(crate) struct Change {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

pub(crate) fn changes(before: &Value, after: &Value) -> Vec<Change> {
    let mut result = Vec::new();
    compare("", Some(before), Some(after), &mut result);
    result
}

fn compare(path: &str, before: Option<&Value>, after: Option<&Value>, out: &mut Vec<Change>) {
    if before == after {
        return;
    }
    match (before, after) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            for key in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
                let field = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                compare(&field, a.get(key), b.get(key), out);
            }
        }
        (Some(Value::Array(a)), Some(Value::Array(b))) => {
            // Match records by ID so an insertion does not rewrite every later skill.
            fn by_id(items: &[Value]) -> Option<BTreeMap<u64, &Value>> {
                items.iter().map(|v| Some((v["id"].as_u64()?, v))).collect()
            }
            if let (Some(left), Some(right)) = (by_id(a), by_id(b))
                && left.len() == a.len()
                && right.len() == b.len()
            {
                let old_order: Vec<_> = a.iter().map(|v| &v["id"]).collect();
                let new_order: Vec<_> = b.iter().map(|v| &v["id"]).collect();
                if old_order != new_order {
                    out.push(Change {
                        path: format!("{path} (ID order)"),
                        before: Some(json!(old_order)),
                        after: Some(json!(new_order)),
                    });
                }
                for id in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
                    let old = left.get(id).copied();
                    let new = right.get(id).copied();
                    let record = new.or(old).unwrap();
                    let name = record
                        .pointer("/name/text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let level = record["level"]
                        .as_u64()
                        .map(|n| format!(" level={n}"))
                        .unwrap_or_default();
                    compare(&format!("{path}[id={id}{level} {name}]"), old, new, out);
                }
            } else if a.len() == b.len() {
                for (index, (old, new)) in a.iter().zip(b).enumerate() {
                    compare(&format!("{path}[{index}]"), Some(old), Some(new), out);
                }
            } else {
                out.push(Change {
                    path: path.into(),
                    before: before.cloned(),
                    after: after.cloned(),
                });
            }
        }
        _ => out.push(Change {
            path: path.into(),
            before: before.cloned(),
            after: after.cloned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_max_core_and_skill_level_twelve() {
        let raw = json!({
            "core_skills": [{"level":7,"skill_type_id":5,"value":"{CAL:390+AvatarSkillLevel(5)*10,1,2}%"},{"level":1,"value":160}],
            "skill_data": [{"damage_multiplier_base":7370,"damage_multiplier_step":670}],
            "skills": [{"description":"{CAL:16.4+AvatarSkillLevel(0)*1.3,100,2}%"}],
            "mindscapes":[{"level":1},{"level":6}]
        });
        let selected = snapshot(raw.clone(), Value::Null);
        let agent = &selected["agent"];
        assert_eq!(
            agent["core_skills"],
            json!([{"level":7,"skill_type_id":5,"value":"460%"}])
        );
        assert_eq!(
            agent["skill_data"][0],
            json!({"damage_multiplier_percent":147.4})
        );
        assert_eq!(agent["skills"][0]["description"], "3200%");
        assert_eq!(agent["mindscapes"], raw["mindscapes"]);
        let mut other = raw;
        other["core_skills"][1]["value"] = json!(232);
        other["skill_data"][0] =
            json!({"damage_multiplier_base":7381,"damage_multiplier_step":669});
        other["skills"][0]["description"] = json!("{CAL:1640+AvatarSkillLevel(0)*130,1,2}%");
        assert!(changes(&selected, &snapshot(other, Value::Null)).is_empty());
    }

    #[test]
    fn insertion_preserves_skill_identity_and_direction() {
        let old = json!([{"id":1, "damage":25}]);
        let new = json!([{"id":2, "damage":80}, {"id":1, "damage":30}]);
        let diff = changes(&old, &new);
        assert!(diff.iter().any(|c| c.path.contains("id=1")
            && c.before == Some(json!(25))
            && c.after == Some(json!(30))));
        assert!(diff.iter().any(|c| c.path.contains("id=2")
            && c.before.is_none()
            && c.after == Some(new[0].clone())));
        assert_eq!(changes(&new, &json!([new[1], new[0]])).len(), 1);
    }
}
