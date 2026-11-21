use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::gachabase::{Comparison, Snapshots, recent_revisions};

const SECTIONS: &[&str] = &[
    "skills",
    "mindscapes",
    "core_skills",
    "tooltips",
    "moves",
    "base_stats",
    "skill_data",
    "unmapped_skill_data",
    "promotions",
    "potentials",
    "forms",
    "entity_skills",
    "attack_properties",
    "animator_patterns",
];

pub(crate) fn definitions() -> Value {
    json!([{
        "type": "function", "name": "read_kit", "strict": true,
        "description": "Read part of any ZZZ agent's kit from GachaBase at the report levels. /core_skills contains the highest rank starting at index 0; other skill values use level 12. Use the input diff's agent_id for the character under analysis. Search by localized name or numeric entry ID to find JSON pointers, then read a pointer. An empty selector lists sections.",
        "parameters": {"type":"object","additionalProperties":false,
            "properties":{
                "agent_id":{"type":"integer","description":"Numeric GachaBase agent ID (not a skill ID)."},
                "revision":{"type":"string","description":"current or previous relative to the input diff, or an exact revision name such as beta_3.4.5_20699967."},
                "language":{"type":"string","enum":["en","cn"]},
                "selector":{"type":"string","description":"Name substring, exact record ID, or a JSON pointer such as /mindscapes/1/description/text. Array indices are zero-based."},
                "offset":{"type":"integer","description":"Start at 0. To continue, copy next_offset from the result."}
            }, "required":["agent_id","revision","language","selector","offset"]}
    },{
        "type":"function","name":"list_recent_revisions","strict":true,
        "description":"List the ten latest distinct GachaBase beta design revision names, newest first. Pass a name directly to read_kit.",
        "parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}
    }])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KitQuery {
    agent_id: u64,
    revision: String,
    language: String,
    selector: String,
    offset: usize,
}

pub(crate) struct KitTools<'a> {
    pub client: &'a Client,
    pub comparison: &'a Comparison,
    pub en: &'a Snapshots,
    pub cn: &'a Snapshots,
    cache: HashMap<(u64, String, String), Value>,
    recent: Option<Vec<String>>,
}

impl<'a> KitTools<'a> {
    pub fn new(
        client: &'a Client,
        comparison: &'a Comparison,
        en: &'a Snapshots,
        cn: &'a Snapshots,
    ) -> Self {
        Self {
            client,
            comparison,
            en,
            cn,
            cache: HashMap::new(),
            recent: None,
        }
    }

    pub async fn call(&mut self, name: &str, arguments: &str) -> Result<Value> {
        match name {
            "list_recent_revisions" => {
                ensure!(
                    serde_json::from_str::<Value>(arguments)? == json!({}),
                    "Expected empty arguments"
                );
                if self.recent.is_none() {
                    self.recent = Some(recent_revisions(self.client).await?);
                }
                Ok(json!({"revisions":self.recent}))
            }
            "read_kit" => self.read(serde_json::from_str(arguments)?).await,
            _ => bail!("Unknown tool: {name}"),
        }
    }

    async fn read(&mut self, query: KitQuery) -> Result<Value> {
        let (localized, lang) = match query.language.as_str() {
            "en" => (self.en, "en"),
            "cn" => (self.cn, "chs"),
            _ => bail!("Expected language en or cn"),
        };
        let revision = match query.revision.as_str() {
            "current" => self.comparison.cur.as_str(),
            "previous" => self.comparison.prev.as_str(),
            name => name,
        };
        ensure!(
            !revision.is_empty() && query.agent_id > 0,
            "Expected an agent ID and revision"
        );
        let snapshot = if query.agent_id == self.comparison.id && revision == self.comparison.cur {
            &localized.cur
        } else if query.agent_id == self.comparison.id && revision == self.comparison.prev {
            &localized.prev
        } else {
            let key = (query.agent_id, revision.to_owned(), lang.to_owned());
            match self.cache.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => entry.insert(
                    self.comparison
                        .agent_snapshot(self.client, query.agent_id, revision, lang)
                        .await?,
                ),
            }
        };
        Ok(
            json!({"agent_id":query.agent_id,"agent":snapshot.pointer("/agent/name/text"),
            "revision":revision,"language":query.language,"result":fragment(snapshot, &query.selector, query.offset)?}),
        )
    }
}

fn section<'a>(snapshot: &'a Value, key: &str) -> Option<&'a Value> {
    if key == "tooltips" {
        snapshot.get(key)
    } else {
        snapshot["agent"].get(key)
    }
}

fn fragment(snapshot: &Value, selector: &str, offset: usize) -> Result<Value> {
    const PAGE_CHARS: usize = 4096;
    const PAGE_MATCHES: usize = 12;
    if selector.is_empty() {
        let sections: Vec<_> = SECTIONS
            .iter()
            .filter(|key| section(snapshot, key).is_some())
            .map(|key| format!("/{key}"))
            .collect();
        return Ok(json!({"sections":sections}));
    }
    if let Some(path) = selector.strip_prefix('/') {
        let (key, _) = path.split_once('/').unwrap_or((path, ""));
        ensure!(SECTIONS.contains(&key), "Unknown kit section {key}");
        let value = section(snapshot, key)
            .and_then(|s| s.pointer(&path[key.len()..]))
            .context("Kit path does not exist in this revision")?;
        let serialized = serde_json::to_string(value)?;
        let len = serialized.chars().count();
        ensure!(
            offset < len,
            "Offset exceeds the length of this fragment ({len})"
        );
        if offset == 0 && len <= PAGE_CHARS {
            return Ok(json!({"path":selector,"value":value}));
        }
        let part: String = serialized.chars().skip(offset).take(PAGE_CHARS).collect();
        let end = offset.saturating_add(PAGE_CHARS);
        return Ok(
            json!({"path":selector,"json_fragment":part,"next_offset":(end < len).then_some(end)}),
        );
    }
    let mut matches = Vec::new();
    let term = selector.to_lowercase();
    for key in SECTIONS {
        if let Some(value) = section(snapshot, key) {
            find_entries(value, &format!("/{key}"), &term, &mut matches);
        }
    }
    let end = offset.saturating_add(PAGE_MATCHES).min(matches.len());
    ensure!(offset <= end, "Offset exceeds the number of matches");
    Ok(json!({"matches":matches[offset..end],"next_offset":(end < matches.len()).then_some(end)}))
}

fn find_entries(v: &Value, path: &str, term: &str, matches: &mut Vec<Value>) {
    match v {
        Value::Object(fields) => {
            let name = v
                .pointer("/name/text")
                .or_else(|| v.get("name"))
                .and_then(Value::as_str);
            if name.is_some_and(|s| s.to_lowercase().contains(term))
                || v["id"].as_u64().is_some_and(|id| id.to_string() == term)
            {
                matches
                    .push(json!({"path":path,"name":name,"id":v.get("id"),"level":v.get("level")}));
            }
            for (key, child) in fields {
                if child.is_array() || child.is_object() {
                    let key = key.replace('~', "~0").replace('/', "~1");
                    find_entries(child, &format!("{path}/{key}"), term, matches);
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                find_entries(child, &format!("{path}/{i}"), term, matches);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "https://zzz.gachabase.net/diff/agents/1601/sunbringer?prev=beta_3.4.5_20694646&cur=beta_3.4.5_20699967";

    #[test]
    fn search_results_are_readable_and_unicode_pages_join_without_loss() {
        let text = "愿火".repeat(2200);
        let snapshot =
            json!({"tooltips":{"a/b":{"name":{"text":"Wishfire"},"id":57,"description":text}}});
        let search = fragment(&snapshot, "wish", 0).unwrap();
        assert_eq!(fragment(&snapshot, "57", 0).unwrap(), search);
        let path = search["matches"][0]["path"].as_str().unwrap();
        let path = format!("{path}/description");
        let first = fragment(&snapshot, &path, 0).unwrap();
        let next = first["next_offset"].as_u64().unwrap() as usize;
        let second = fragment(&snapshot, &path, next).unwrap();
        let combined = format!(
            "{}{}",
            first["json_fragment"].as_str().unwrap(),
            second["json_fragment"].as_str().unwrap()
        );
        assert_eq!(serde_json::from_str::<String>(&combined).unwrap(), text);
        assert!(second["next_offset"].is_null());
    }

    #[tokio::test]
    async fn tool_outputs_match_call_ids_revisions_and_languages() {
        let client = Client::new();
        let page = Comparison::parse(PAGE).unwrap();
        let snapshot = |text| json!({"agent":{"mindscapes":[{"description":text}]}});
        let en = Snapshots {
            prev: snapshot("before"),
            cur: snapshot("after"),
        };
        let cn = Snapshots {
            prev: snapshot("以前"),
            cur: snapshot("目前"),
        };
        let mut tools = KitTools::new(&client, &page, &en, &cn);
        let call = |id, revision: &str, lang| {
            json!({"type":"function_call","call_id":id,"name":"read_kit",
            "arguments":json!({"agent_id":1601,"revision":revision,"language":lang,
                "selector":"/mindscapes/0/description","offset":0}).to_string()})
        };
        let output = [
            call("a", "previous", "cn"),
            call("b", "current", "en"),
            call("c", &page.prev, "en"),
            call("d", "current", "unknown"),
        ];
        let results = crate::tool_outputs(&output, &mut tools).await.unwrap();
        for (result, (id, text)) in
            results
                .iter()
                .zip([("a", "以前"), ("b", "after"), ("c", "before")])
        {
            assert_eq!(result["type"], "function_call_output");
            assert_eq!(result["call_id"], id);
            let body: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
            assert_eq!(body["result"]["value"], text);
            assert_eq!(
                body["revision"],
                if id == "b" { &page.cur } else { &page.prev }.as_str()
            );
        }
        assert_eq!(results[3]["call_id"], "d");
        assert!(results[3]["output"].as_str().unwrap().contains("error"));
    }
}
