use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use rig_core::{
    message::ToolName,
    tool::{DynamicTool, ToolOutput},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    catalog::{self, Category, Index},
    gachabase::{Comparison, Snapshots, recent_revisions},
};

const PAGE_CHARS: usize = 4096;

#[derive(Serialize)]
pub(crate) struct ToolSpec {
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

pub(crate) fn definitions() -> Vec<ToolSpec> {
    let category =
        json!({"type":"string","enum":["agents","w-engines","drive-discs","bangboo","tutorials"]});
    let language = json!({"type":"string","enum":["en","cn"]});
    let offset = json!({"type":["integer","null"],"minimum":0,"description":"Start at 0 or null. Copy next_offset to continue."});
    vec![
        ToolSpec {
            name: "list_recent_revisions",
            description: "Recent GachaBase design revisions, newest first, up to ten each for beta and release.",
            parameters: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        },
        ToolSpec {
            name: "search_gachabase",
            description: "Search the latest beta catalog by localized name, alias, slug or ID. Empty query browses a category. Pass category and a result ID to get_gachabase_entry.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "category":category,"query":{"type":"string"},"language":language,"offset":offset
            },"required":["category","query","language","offset"]}),
        },
        ToolSpec {
            name: "get_gachabase_entry",
            description: "Read a GachaBase record by category and ID. Null version reads the latest beta; pass an exact revision for history. Null selector reads the whole paginated entry. An empty string lists fields; JSON pointer reads a field, name or sub-record ID finds paths, text:<phrase> finds descriptions. Agent values are normalized at report levels.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "category":category,"id":{"type":"integer","minimum":1},"language":language,
                "version":{"type":["string","null"]},"selector":{"type":["string","null"]},"offset":offset
            },"required":["category","id","language","version","selector","offset"]}),
        },
    ]
}

#[derive(Clone, Copy, Hash, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Language {
    En,
    Cn,
}

impl Language {
    fn upstream(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Cn => "chs",
        }
    }
}

pub(crate) struct KitTools {
    client: Client,
    indexes: HashMap<(Category, Language), Index>,
    entries: HashMap<(Category, u64, Language, String), Value>,
    // Only large diffs are retained for paged access from the report's target entry.
    report_changes: HashMap<(u64, Language, String), String>,
    recent: Option<Vec<String>>,
}

impl KitTools {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            indexes: HashMap::new(),
            entries: HashMap::new(),
            report_changes: HashMap::new(),
            recent: None,
        }
    }

    pub fn report_input(
        &mut self,
        page: &Comparison,
        en: Snapshots,
        cn: Snapshots,
    ) -> Result<Option<String>> {
        let status = if en.prev.is_null() {
            "added"
        } else if en.cur.is_null() {
            "removed"
        } else {
            "updated"
        };
        let mut input = json!({"agent_id":page.id,"agent":page.slug,"prev":page.prev,"cur":page.cur,"status":status});
        let mut empty = true;
        for (label, language, snapshots) in [("en", Language::En, en), ("cn", Language::Cn, cn)] {
            let changes = snapshots.changes();
            empty &= changes.is_empty();
            input[label] = if status == "updated" && serde_json::to_vec(&changes)?.len() <= 8192 {
                json!(changes)
            } else {
                let mut sections = BTreeMap::new();
                for change in &changes {
                    let path = change.path.strip_prefix("agent.").unwrap_or(&change.path);
                    let section = path.split(['.', '[', ' ']).next().unwrap_or(path);
                    *sections.entry(section).or_insert(0usize) += 1;
                }
                self.report_changes.insert(
                    (page.id, language, page.cur.clone()),
                    serde_json::to_string(&changes)?,
                );
                json!({"change_count":changes.len(),"sections":sections,"selector":"@changes",
                    "sample_paths":changes.iter().take(32).map(|c| &c.path).collect::<Vec<_>>()})
            };
            for (version, snapshot) in [(&page.prev, snapshots.prev), (&page.cur, snapshots.cur)] {
                self.entries.insert(
                    (Category::Agents, page.id, language, version.clone()),
                    catalog::agent_entry(snapshot),
                );
            }
        }
        if empty && status == "updated" {
            return Ok(None);
        }
        Ok(Some(serde_json::to_string(&input)?))
    }

    pub fn into_tools(self) -> Vec<DynamicTool> {
        let shared = Arc::new(Mutex::new(self));
        definitions()
            .into_iter()
            .map(|spec| {
                let shared = Arc::clone(&shared);
                DynamicTool::new(
                    ToolName::new(spec.name).expect("static tool name"),
                    spec.description,
                    spec.parameters,
                    move |args| {
                        let shared = Arc::clone(&shared);
                        Box::pin(async move {
                            let result = shared.lock().await.call(spec.name, args).await;
                            Ok(ToolOutput::json(
                                result.unwrap_or_else(|err| json!({"error":err.to_string()})),
                            ))
                        })
                    },
                )
            })
            .collect()
    }

    async fn revisions(&mut self) -> Result<&[String]> {
        if self.recent.is_none() {
            self.recent = Some(recent_revisions(&self.client).await?);
        }
        Ok(self.recent.as_deref().unwrap_or_default())
    }

    async fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        match name {
            "list_recent_revisions" => {
                ensure!(args == json!({}), "Expected empty arguments");
                Ok(json!(self.revisions().await?))
            }
            "search_gachabase" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Search {
                    category: Category,
                    language: Language,
                    query: String,
                    offset: Option<usize>,
                }
                let q: Search = serde_json::from_value(args)?;
                let index = match self.indexes.entry((q.category, q.language)) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => e.insert(
                        Index::load(&self.client, q.category, q.language.upstream()).await?,
                    ),
                };
                index.search(q.category, &q.query, q.offset.unwrap_or_default())
            }
            "get_gachabase_entry" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Read {
                    category: Category,
                    id: u64,
                    language: Language,
                    version: Option<String>,
                    selector: Option<String>,
                    offset: Option<usize>,
                }
                let q: Read = serde_json::from_value(args)?;
                ensure!(q.id > 0, "Expected positive ID");
                let version = match q.version {
                    Some(v) => v,
                    None if self.indexes.contains_key(&(q.category, q.language)) => {
                        self.indexes[&(q.category, q.language)].revision.clone()
                    }
                    None => self
                        .revisions()
                        .await?
                        .iter()
                        .find(|name| name.starts_with("beta_"))
                        .context("No beta versions")?
                        .clone(),
                };
                let key = (q.category, q.id, q.language, version.clone());
                if q.category == Category::Agents && q.selector.as_deref() == Some("@changes") {
                    let changes = self
                        .report_changes
                        .get(&(q.id, q.language, version.clone()))
                        .context("No paged diff for this entry and version")?;
                    return Ok(
                        json!({"id":q.id,"version":version,"result":json_page(changes,q.offset.unwrap_or_default())?}),
                    );
                }
                let entry = match self.entries.entry(key) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        crate::beta::revision_group(&version)?;
                        e.insert(
                            catalog::detail(
                                &self.client,
                                q.category,
                                q.id,
                                &version,
                                q.language.upstream(),
                            )
                            .await?,
                        )
                    }
                };
                ensure!(!entry.is_null(), "Entry absent in this revision");
                let offset = q.offset.unwrap_or_default();
                let result = match q.selector {
                    Some(selector) => fragment(entry, &selector, offset)?,
                    None => json_page(&serde_json::to_string(entry)?, offset)?,
                };
                Ok(
                    json!({"category":q.category,"id":q.id,"version":version,"language":match q.language {Language::En=>"en",Language::Cn=>"cn"},"result":result}),
                )
            }
            _ => bail!("Unknown tool"),
        }
    }
}

fn json_page(serialized: &str, offset: usize) -> Result<Value> {
    let mut chars = serialized.chars().skip(offset).peekable();
    ensure!(
        chars.peek().is_some(),
        "Offset exceeds the length of this fragment"
    );
    let part: String = chars.by_ref().take(PAGE_CHARS).collect();
    Ok(json!({"json_fragment":part,"next_offset":chars.next().map(|_| offset + PAGE_CHARS)}))
}

fn pointer_key(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn fragment(snapshot: &Value, selector: &str, offset: usize) -> Result<Value> {
    const PAGE_MATCHES: usize = 12;
    ensure!(!snapshot.is_null(), "Character absent in this revision");
    if selector.is_empty() {
        let sections: Vec<_> = snapshot
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .map(|key| format!("/{}", pointer_key(key)))
            .collect();
        return Ok(json!({"sections":sections}));
    }
    if let Some(path) = selector.strip_prefix('/') {
        let (key, _) = path.split_once('/').unwrap_or((path, ""));
        let decoded = key.replace("~1", "/").replace("~0", "~");
        let value = snapshot
            .get(&decoded)
            .and_then(|s| s.pointer(&path[key.len()..]))
            .context("Kit path does not exist in this revision")?;
        let serialized = serde_json::to_string(value)?;
        if offset == 0 && serialized.chars().take(PAGE_CHARS + 1).count() <= PAGE_CHARS {
            return Ok(json!({"path":selector,"value":value}));
        }
        let mut page = json_page(&serialized, offset)?;
        page["path"] = json!(selector);
        return Ok(page);
    }
    let mut matches = Vec::new();
    let text = selector.strip_prefix("text:");
    let term = text.unwrap_or(selector).trim().to_lowercase();
    ensure!(!term.is_empty(), "Expected a non-empty search term");
    if let Some(fields) = snapshot.as_object() {
        for (key, value) in fields {
            find_entries(
                value,
                &format!("/{}", pointer_key(key)),
                &term,
                text.is_some(),
                &mut matches,
            );
        }
    }
    let end = offset.saturating_add(PAGE_MATCHES).min(matches.len());
    ensure!(offset <= end, "Offset exceeds the number of matches");
    Ok(json!({"matches":matches[offset..end],"next_offset":(end < matches.len()).then_some(end)}))
}

fn find_entries(v: &Value, path: &str, term: &str, full_text: bool, matches: &mut Vec<Value>) {
    match v {
        Value::Object(fields) => {
            let name = v
                .pointer("/name/text")
                .or_else(|| v.get("name"))
                .and_then(Value::as_str);
            if !full_text
                && (name.is_some_and(|s| s.to_lowercase().contains(term))
                    || v["id"].as_u64().is_some_and(|id| id.to_string() == term))
            {
                matches
                    .push(json!({"path":path,"name":name,"id":v.get("id"),"level":v.get("level")}));
            }
            for (key, child) in fields {
                if full_text || child.is_array() || child.is_object() {
                    find_entries(
                        child,
                        &format!("{path}/{}", pointer_key(key)),
                        term,
                        full_text,
                        matches,
                    );
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                find_entries(child, &format!("{path}/{i}"), term, full_text, matches);
            }
        }
        Value::String(s) if full_text && s.to_lowercase().contains(term) => {
            matches.push(json!({"path":path}));
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
    async fn read_versioned_character_fields_without_network() {
        let page = Comparison::parse(PAGE).unwrap();
        let snapshots = || Snapshots {
            prev: json!({"agent":{"skills":[{"id":8,"text":"older effect"}]}}),
            cur: json!({"agent":{"skills":[{"id":8,"text":"new effect"}]}}),
        };
        let mut tools = KitTools::new(Client::new());
        let input = tools
            .report_input(&page, snapshots(), snapshots())
            .unwrap()
            .unwrap();
        assert!(input.contains("older effect"));
        let args = json!({"category":"agents","id":1601,"language":"cn","version":page.prev,"selector":"text:OLDER"});
        let result = tools.call("get_gachabase_entry", args).await.unwrap();
        assert_eq!(result["result"]["matches"][0]["path"], "/skills/0/text");
        let args = json!({"category":"agents","id":1601,"language":"en","version":page.cur,"selector":"/skills/0/text"});
        assert_eq!(
            tools.call("get_gachabase_entry", args).await.unwrap()["result"]["value"],
            "new effect"
        );
    }
}
