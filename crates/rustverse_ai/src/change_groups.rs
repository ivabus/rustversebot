use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{
    diff::Change,
    kit::{PAGE_CHARS, json_page},
};

#[derive(Default)]
pub(crate) struct Groups(BTreeMap<String, Vec<Change>>);

impl Groups {
    pub fn new(changes: Vec<Change>) -> Self {
        let mut groups = Self::default();
        for change in changes {
            let path = change.path.strip_prefix("agent.").unwrap_or(&change.path);
            let end = path.find(['[', '.', ' ']).unwrap_or(path.len());
            let section = &path[..end];
            let key = match path[end..].strip_prefix('[') {
                Some(rest) => format!(
                    "{section}[{}]",
                    rest.split([']', ' ']).next().unwrap_or(rest)
                ),
                None => section.to_owned(),
            };
            groups.0.entry(key).or_default().push(change);
        }
        groups
    }

    pub fn index(&self) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|(name, records)| {
                    let mut group =
                        json!({"selector":format!("@changes:{name}"),"count":records.len()});
                    // A scalar-only tweak can often be reviewed without a tool round trip.
                    if records.len() <= 3
                        && serde_json::to_vec(records)
                            .expect("serializable changes")
                            .len()
                            <= 384
                    {
                        group["changes"] = json!(records);
                    } else {
                        group["first_path"] = json!(records[0].path);
                    }
                    group
                })
                .collect(),
        )
    }

    pub fn read(&self, selector: &str, offset: usize) -> Result<Value> {
        if selector == "@changes" {
            return Ok(self.index());
        }
        let selector = selector
            .strip_prefix("@changes:")
            .context("Invalid group selector")?;
        let (key, field) = selector
            .split_once('/')
            .map_or((selector, None), |(key, field)| (key, Some(field)));
        let group = self.0.get(key).context("Unknown change group")?;
        if let Some(field) = field {
            let (index, side) = field.split_once('/').context("Missing change side")?;
            let record = group
                .get(index.parse::<usize>()?)
                .context("Unknown change")?;
            let value = match side {
                "before" => &record.before,
                "after" => &record.after,
                _ => anyhow::bail!("Unknown side"),
            };
            return json_page(
                &serde_json::to_string(value.as_ref().context("Absent side")?)?,
                offset,
            );
        }
        ensure!(offset < group.len(), "Offset exceeds group length");
        let mut records = Vec::new();
        let mut size = 0;
        for (i, change) in group.iter().enumerate().skip(offset) {
            let mut record = serde_json::to_value(change)?;
            if record.to_string().len() > PAGE_CHARS {
                let mut sides = ["before", "after"];
                sides.sort_by_cached_key(|side| {
                    std::cmp::Reverse(record.get(*side).map_or(0, |v| v.to_string().len()))
                });
                for side in sides {
                    if record.get(side).is_none() {
                        continue;
                    }
                    record[side] = json!({"read":format!("@changes:{key}/{i}/{side}")});
                    if record.to_string().len() <= PAGE_CHARS {
                        break;
                    }
                }
            }
            let len = record.to_string().len();
            if !records.is_empty() && size + len > PAGE_CHARS {
                break;
            }
            records.push(record);
            size += len;
        }
        let next = offset + records.len();
        Ok(json!({"changes":records,"next_offset":(next < group.len()).then_some(next)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::changes;

    #[test]
    fn groups_preserve_identity_and_resume_at_record_boundaries() {
        let before = json!({"agent":{"mindscapes":[{"id":42,"level":4,"a":"a".repeat(1200),"b":"b".repeat(1200)},{"id":77,"text":"old"}]}});
        let after = json!({"agent":{"mindscapes":[{"id":42,"level":4,"a":"c".repeat(1200),"b":"d".repeat(1200)},{"id":77,"text":"改".repeat(4500)}]}});
        let groups = Groups::new(changes(&before, &after));
        let index = groups.read("@changes", 0).unwrap();
        assert_eq!(index.as_array().unwrap().len(), 2);
        let key = "@changes:mindscapes[id=42]";
        let first = groups.read(key, 0).unwrap();
        assert_eq!(first["next_offset"], 1);
        assert_eq!(
            first["changes"][0]["before"],
            before["agent"]["mindscapes"][0]["a"]
        );
        let last = groups.read(key, 1).unwrap();
        assert!(last["next_offset"].is_null());
        assert_eq!(
            last["changes"][0]["after"],
            after["agent"]["mindscapes"][0]["b"]
        );
        let big = groups.read("@changes:mindscapes[id=77]", 0).unwrap();
        assert_eq!(big["changes"][0]["before"], "old");
        let selector = big["changes"][0]["after"]["read"].as_str().unwrap();
        let page = groups.read(selector, 0).unwrap();
        let tail = groups
            .read(selector, page["next_offset"].as_u64().unwrap() as usize)
            .unwrap();
        let joined = format!(
            "{}{}",
            page["json_fragment"].as_str().unwrap(),
            tail["json_fragment"].as_str().unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&joined).unwrap(),
            after["agent"]["mindscapes"][1]["text"]
        );
    }
}
