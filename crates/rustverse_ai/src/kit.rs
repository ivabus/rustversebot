use std::{
    collections::{BTreeSet, HashMap},
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
    change_groups::Groups,
    compute::Calculation,
    gachabase::{Comparison, Snapshots, recent_revisions},
};

pub(crate) const PAGE_CHARS: usize = 4096;

#[derive(Serialize)]
pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) parameters: Value,
}

pub(crate) fn definitions() -> Vec<ToolSpec> {
    let category = json!({"type":"string","enum":["agents","w-engines","drive-discs","bangboo","enemies","tutorials"]});
    let language = json!({"type":"string","enum":["en","cn"]});
    let offset = json!({"type":["integer","null"],"minimum":0,"description":"Start at 0 or null. Copy next_offset to continue."});
    let version = json!({"type":["string","null"],"description":"Exact revision or branch ('release' or 'beta'). For current live endgame use 'release', then its returned revision. Null reuses the pinned revision or defaults to the newest across both branches for questions. Reports start at cur."});
    let reference = json!({"type":"object","additionalProperties":false,"properties":{"source":{"type":"string"},"path":{"type":"string"}},"required":["source","path"]});
    let mut named = reference.clone();
    named["properties"]["name"] = json!({"type":"string","description":"Unique Rhai variable name, e.g. before or hits. Up to 64 bytes."});
    named["required"] = json!(["name", "source", "path"]);
    vec![
        ToolSpec {
            name: "list_recent_revisions",
            description: "Recent GachaBase design revisions, newest first, up to ten each for beta and release.",
            parameters: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        },
        ToolSpec {
            name: "search_gachabase",
            description: "Search by name, alias, slug or ID. For a season boss use category=enemies and the monster ID from Nanoka (or its localized name). For a roster use category=agents, query='', offset=null: returns all matching agents at the pinned revision. Filter agents by specialty, element or both (AND), using catalog names in the selected language. Prefer entries with core skills when names collide.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "category":category,"query":{"type":"string"},"language":language,"offset":offset,"version":version,
                "specialty":{"type":["string","null"]},"element":{"type":["string","null"]}
            },"required":["category","query","language","offset","version","specialty","element"]}),
        },
        ToolSpec {
            name: "get_gachabase_entry",
            description: "Read by ID. For enemies, null selector gives combat and codex descriptions. /enemy_variants contains variant affinities. For agents, @team summarizes Core Passive and Additional Ability; follow a path for exact conditions. Null returns compact pages, empty lists fields, a JSON pointer reads a field, @raw reads the full record. Name/ID or text:<phrase> finds paths. Agent levels default to skill 12 and max core. Use source and JSON pointers for compute. Use @changes only for an agent, language and revision with diff groups in the supplied comparison report input.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "category":category,"id":{"type":"integer","minimum":1},"language":language,
                "version":version,"selector":{"type":["string","null"]},"offset":offset,
                "skill_level":{"type":["integer","null"],"minimum":1,"maximum":16},"core_level":{"type":["integer","null"],"minimum":1}
            },"required":["category","id","language","version","selector","offset","skill_level","core_level"]}),
        },
        ToolSpec {
            name: "compute",
            description: "Run Rhai on this run's completed tool calls and optional numeric bindings. calls is a read-only array in completion order. Each call has name, args and result (including errors and prior compute results). Pass bindings: [] to use calls alone. Example after a season batch: calls[-1].result.results[0].season_id. Filter calls by name and args when reading several. Iterate JSON arrays by value and objects by key; use obj.key or obj[key]. JSON integers retain integer arithmetic; multiply by 1.0 before fractional division. Each explicit binding reads an exact source and JSON pointer, with at most one /*/ for one number per array row. Use its name or x0, x1, ... by position; bound numbers are floats. Return a finite number or a flat array of up to 64 numbers.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{"script":{"type":"string","description":"Rhai expression or short script, up to 2048 bytes."},"bindings":{"type":"array","maxItems":12,"items":{"anyOf":[named,reference]}}},"required":["script","bindings"]}),
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

fn source_key(category: Category, id: u64, language: Language, version: &str) -> String {
    format!("{}/{id}/{}/{version}", category.path(), language.upstream())
}

/// Verified lookups from this turn. The delivered text identifies the chosen team and room.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeasonReference {
    pub mode: String,
    pub season_id: u64,
    pub requested: String,
    pub page: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnReferences {
    pub seasons: BTreeSet<SeasonReference>,
    pub revisions: BTreeSet<String>,
    pub entries: BTreeSet<String>,
}

/// A single tool output or the ordered pages of a batch, including during compaction.
pub(crate) fn lookup_results(result: &Value) -> &[Value] {
    result["results"]
        .as_array()
        .map_or_else(|| std::slice::from_ref(result), Vec::as_slice)
}

impl TurnReferences {
    pub fn is_empty(&self) -> bool {
        self.seasons.is_empty() && self.revisions.is_empty() && self.entries.is_empty()
    }

    fn record(&mut self, name: &str, result: &Value) {
        for result in lookup_results(result) {
            if matches!(name, "get_endgame_season" | "get_endgame_seasons")
                && let (Some(mode), Some(id)) =
                    (result["mode"].as_str(), result["season_id"].as_u64())
            {
                self.seasons.insert(SeasonReference {
                    mode: mode.into(),
                    season_id: id,
                    requested: result["requested_season"].as_str().unwrap_or(mode).into(),
                    page: result["offset"].as_u64().unwrap_or_default() as usize,
                    path: result["path"].as_str().map(str::to_owned),
                });
            }
            if let Some(version) = result["version"].as_str() {
                self.revisions.insert(version.into());
            }
            if let Some(source) = result["source"].as_str() {
                self.entries.insert(source.into());
            }
        }
    }
}

pub(crate) struct KitTools {
    pub references: Arc<Mutex<TurnReferences>>,
    client: Client,
    indexes: HashMap<(Category, Language, String), Index>,
    sources: HashMap<String, Value>,
    calls: Vec<Arc<Value>>,
    report_changes: HashMap<(u64, Language, String), Groups>,
    seasons: Option<crate::seasons::SeasonTools>,
    pinned: Option<String>,
    recent: Option<Vec<String>>,
}

impl KitTools {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            references: Default::default(),
            indexes: HashMap::new(),
            sources: HashMap::new(),
            calls: Vec::new(),
            seasons: None,
            pinned: None,
            report_changes: HashMap::new(),
            recent: None,
        }
    }

    pub fn for_question(client: Client, question: &str) -> Self {
        static EXACT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\b(?:beta|release)_[0-9]+\.[0-9]+\.[0-9]+_[0-9]+\b").unwrap()
        });
        let mut tools = Self::new(client);
        tools.seasons = Some(crate::seasons::SeasonTools::new(nanoka::NanokaClient::new()));
        tools.pinned = EXACT.find(question).map(|v| v.as_str().to_owned());
        tools
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
        self.pinned = Some(page.cur.clone());
        let mut empty = true;
        for (label, language, snapshots) in [("en", Language::En, en), ("cn", Language::Cn, cn)] {
            let mut prepare = |raw: Value, version: &str, side: &str| {
                let key = source_key(Category::Agents, page.id, language, version);
                if raw.is_null() {
                    self.sources.insert(format!("{key}/raw"), Value::Null);
                    return Value::Null;
                }
                let view = crate::diff::snapshot(raw["agent"].clone(), raw["tooltips"].clone());
                self.sources
                    .insert(format!("{key}/raw"), catalog::agent_entry(raw));
                let source = format!("{key}/{}/max", crate::diff::SKILL_LEVEL);
                self.sources
                    .insert(source.clone(), catalog::agent_entry(view.clone()));
                input["sources"][label][side] = json!(source);
                view
            };
            let normalized = Snapshots {
                prev: prepare(snapshots.prev, &page.prev, "before"),
                cur: prepare(snapshots.cur, &page.cur, "after"),
            };
            let changes = normalized.changes();
            empty &= changes.is_empty();
            input[label] = if status == "updated" && serde_json::to_vec(&changes)?.len() <= 8192 {
                json!(changes)
            } else {
                let count = changes.len();
                let groups = Groups::new(changes);
                let index = groups.index();
                self.report_changes
                    .insert((page.id, language, page.cur.clone()), groups);
                json!({"change_count":count,"groups":index})
            };
        }
        if empty && status == "updated" {
            return Ok(None);
        }
        Ok(Some(serde_json::to_string(&input)?))
    }

    pub fn into_tools(self, research_updates: bool) -> Vec<DynamicTool> {
        let mut specs = definitions();
        if self.seasons.is_some() {
            specs.extend(crate::seasons::definitions());
        }
        let shared = Arc::new(Mutex::new(self));
        specs
            .into_iter()
            .map(|mut spec| {
                let shared = Arc::clone(&shared);
                if research_updates {
                    spec.parameters["properties"]["research_status"] = json!({
                        "type":["string","null"],"description":"Announce a new research objective once, in the user's language. Use null while continuing the same objective across lookups, tools and pages."
                    });
                    spec.parameters["required"].as_array_mut().expect("static tool schema").push(json!("research_status"));
                }
                DynamicTool::new(
                    ToolName::new(spec.name).expect("static tool name"),
                    spec.description,
                    spec.parameters,
                    move |args| {
                        let shared = Arc::clone(&shared);
                        Box::pin(async move {
                            let result = Self::dispatch(&shared, spec.name, args).await;
                            Ok(ToolOutput::json(
                                result.unwrap_or_else(|err| json!({"error":err.to_string()})),
                            ))
                        })
                    },
                )
            })
            .collect()
    }

    async fn dispatch(shared: &Mutex<Self>, name: &str, args: Value) -> Result<Value> {
        let mut tool_args = args.clone();
        if let Some(fields) = tool_args.as_object_mut() {
            fields.remove("research_status");
        }
        let result = Self::execute(shared, name, tool_args).await;
        let output = match &result {
            Ok(value) => value.clone(),
            Err(err) => json!({"error":err.to_string()}),
        };
        let record = Value::Object(serde_json::Map::from_iter([
            ("name".into(), Value::from(name)),
            ("args".into(), args),
            ("result".into(), output),
        ]));
        shared.lock().await.calls.push(Arc::new(record));
        result
    }

    async fn execute(shared: &Mutex<Self>, name: &str, args: Value) -> Result<Value> {
        if name != "compute" {
            let mut kit = shared.lock().await;
            let result = kit.call(name, args).await?;
            if kit.seasons.is_some() {
                kit.references.lock().await.record(name, &result);
            }
            return Ok(result);
        }
        let calculation: Calculation = serde_json::from_value(args)?;
        let kit = shared.lock().await;
        let prepared = calculation.prepare(&kit.sources)?;
        let calls = kit.calls.clone();
        drop(kit);
        tokio::task::spawn_blocking(move || prepared.run(calls)).await?
    }

    async fn revisions(&mut self) -> Result<&[String]> {
        if self.recent.is_none() {
            self.recent = Some(recent_revisions(&self.client).await?);
        }
        Ok(self.recent.as_deref().unwrap_or_default())
    }

    async fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        match name {
            "list_endgame_seasons" | "get_endgame_season" | "get_endgame_seasons" => {
                self.seasons
                    .as_mut()
                    .context("Season tools unavailable")?
                    .call(name, args, &mut self.sources)
                    .await
            }
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
                    specialty: Option<String>,
                    element: Option<String>,
                    offset: Option<usize>,
                    version: Option<String>,
                }
                let q: Search = serde_json::from_value(args)?;
                let version = self.version(q.version).await?;
                let index = match self
                    .indexes
                    .entry((q.category, q.language, version.clone()))
                {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => e.insert(
                        Index::load(&self.client, q.category, q.language.upstream(), &version)
                            .await?,
                    ),
                };
                index.search(
                    q.category,
                    &q.query,
                    q.offset.unwrap_or_default(),
                    q.specialty.as_deref(),
                    q.element.as_deref(),
                )
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
                    skill_level: Option<i64>,
                    core_level: Option<u64>,
                }
                let q: Read = serde_json::from_value(args)?;
                ensure!(q.id > 0, "Expected positive ID");
                let version = self.version(q.version).await?;
                if q.category == Category::Agents
                    && let Some(selector) =
                        q.selector.as_deref().filter(|s| s.starts_with("@changes"))
                {
                    let changes = self
                        .report_changes
                        .get(&(q.id, q.language, version.clone()))
                        .context("No paged diff for this entry and version")?;
                    return Ok(
                        json!({"id":q.id,"version":version,"result":changes.read(selector,q.offset.unwrap_or_default())?}),
                    );
                }
                let key = source_key(q.category, q.id, q.language, &version);
                let raw_key = format!("{key}/raw");
                if !self.sources.contains_key(&raw_key) {
                    crate::beta::revision_group(&version)?;
                    let value = catalog::detail(
                        &self.client,
                        q.category,
                        q.id,
                        &version,
                        q.language.upstream(),
                    )
                    .await?;
                    self.sources.insert(raw_key.clone(), value);
                }
                ensure!(
                    !self.sources[&raw_key].is_null(),
                    "Entry absent in this revision"
                );
                let source =
                    if q.category != Category::Agents || q.selector.as_deref() == Some("@raw") {
                        ensure!(
                            q.skill_level.is_none() && q.core_level.is_none(),
                            "Levels apply to projected agent views only"
                        );
                        raw_key
                    } else {
                        let level = q.skill_level.unwrap_or(crate::diff::SKILL_LEVEL);
                        ensure!((1..=16).contains(&level), "Skill level must be 1..16");
                        let source = format!(
                            "{key}/{level}/{}",
                            q.core_level.map_or("max".to_owned(), |n| n.to_string())
                        );
                        if !self.sources.contains_key(&source) {
                            let raw = &self.sources[&raw_key];
                            if let Some(rank) = q.core_level {
                                ensure!(
                                    raw["core_skills"].as_array().is_some_and(|rows| rows
                                        .iter()
                                        .any(|r| r["level"].as_u64() == Some(rank))),
                                    "Core rank absent in this revision"
                                );
                            }
                            let mut agent = raw.clone();
                            let tips = agent
                                .as_object_mut()
                                .context("Invalid agent")?
                                .remove("tooltips")
                                .unwrap_or(Value::Null);
                            let view = crate::diff::snapshot_at(agent, tips, level, q.core_level);
                            self.sources
                                .insert(source.clone(), catalog::agent_entry(view));
                        }
                        source
                    };
                let entry = &self.sources[&source];
                let offset = q.offset.unwrap_or_default();
                let result = match q.selector {
                    Some(selector) if selector == "@team" && q.category == Category::Agents => {
                        crate::entry_view::team(entry, offset)?
                    }
                    Some(selector) => fragment(entry, &selector, offset)?,
                    None => crate::entry_view::page(q.category, entry, offset)?,
                };
                Ok(
                    json!({"category":q.category,"id":q.id,"version":version,"language":match q.language {Language::En=>"en",Language::Cn=>"cn"},"source":source,"result":result}),
                )
            }
            _ => bail!("Unknown tool"),
        }
    }

    async fn version(&mut self, requested: Option<String>) -> Result<String> {
        let version = match requested.or_else(|| self.pinned.clone()) {
            None => self
                .revisions()
                .await?
                .first()
                .context("No beta or release revisions")?
                .clone(),
            Some(branch) if matches!(branch.as_str(), "beta" | "release") => self
                .revisions()
                .await?
                .iter()
                .find(|s| s.split('_').next() == Some(branch.as_str()))
                .context("No revision for requested branch")?
                .clone(),
            Some(exact) => exact,
        };
        crate::beta::revision_group(&version)?;
        self.pinned.get_or_insert_with(|| version.clone());
        Ok(version)
    }
}

pub(crate) fn json_page(serialized: &str, offset: usize) -> Result<Value> {
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
    if selector == "@raw" {
        return json_page(&serde_json::to_string(snapshot)?, offset);
    }
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
mod run_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "https://zzz.gachabase.net/diff/agents/1601/sunbringer?prev=beta_3.4.5_20694646&cur=beta_3.4.5_20699967";

    #[tokio::test]
    async fn enemy_search_and_combat_description_use_the_requested_release() {
        let version = "release_3.4.0_42";
        let layout = json!({"type":"data","nodes":[{"data":[{"language":1,"revisions":2},"en",
            {"current":3},{"branch":4,"version":5,"design_revision":6},"release","3.4.0",42]}]});
        let rows = json!({"type":"chunk","data":[{"data":1},{"entries":2},[3],
            {"id":4,"slug":5,"name":6,"search_data":7},40009,"integrated-girtablullu",{"text":7},"Integrated - Girtablullu"]});
        let index = Index::decode(&format!("{layout}\n{rows}"), "en", version).unwrap();
        let mut kit = KitTools::for_question(Client::new(), "Текущий штурм");
        kit.recent = Some(vec!["beta_3.5.0_99".into(), version.into()]);
        kit.indexes
            .insert((Category::Enemies, Language::En, version.into()), index);
        let source = format!("enemies/40009/en/{version}/raw");
        kit.sources.insert(source.clone(),json!({"id":40009,"name":{"text":"Integrated - Girtablullu"},
            "skill_description":{"text":"Dodge the attack after the teleport."},"description":{"text":"Codex entry."}}));
        for query in ["40009", "integrated girtablullu"] {
            let matches = kit
                .call(
                    "search_gachabase",
                    json!({"category":"enemies","query":query,
                "language":"en","version":"release"}),
                )
                .await
                .unwrap();
            assert_eq!(matches["matches"][0]["id"], 40009);
            let entry = kit
                .call(
                    "get_gachabase_entry",
                    json!({"category":"enemies","id":matches["matches"][0]["id"],
                "language":"en","version":matches["version"]}),
                )
                .await
                .unwrap();
            assert_eq!(entry["source"], source);
            assert_eq!(
                entry["result"]["details"][0]["skill_description"],
                "Dodge the attack after the teleport."
            );
        }
    }

    #[tokio::test]
    async fn questions_pin_the_newest_or_the_requested_revision() {
        let beta = "beta_3.4.5_99";
        let release = "release_3.3.0_10";
        for (newest, older, branch) in [(beta, release, "release"), (release, beta, "beta")] {
            let mut kit = KitTools::for_question(Client::new(), "Как работает навык?");
            kit.recent = Some(vec![newest.into(), older.into()]);
            assert_eq!(kit.version(None).await.unwrap(), newest);
            assert_eq!(kit.version(Some(branch.into())).await.unwrap(), older);
            assert_eq!(kit.version(None).await.unwrap(), newest);
        }
        let mut kit = KitTools::for_question(Client::new(), "Проверь beta_3.4.4_42.");
        assert_eq!(kit.version(None).await.unwrap(), "beta_3.4.4_42");
    }

    #[tokio::test]
    async fn level_views_keep_originals_and_supply_numeric_compute_inputs() {
        let mut kit = KitTools::for_question(Client::new(), "beta_3.4.4_42");
        let key = "agents/1/en/beta_3.4.4_42/raw";
        let raw = json!({"core_skills":[{"level":3,"skill_type_id":5,"text":"{CAL:1+AvatarSkillLevel(5)*2,1,0}"},{"level":7,"skill_type_id":5}],
            "skill_data":[{"damage_multiplier_base":100,"damage_multiplier_step":20},{"damage_multiplier_base":200,"damage_multiplier_step":10}]});
        kit.sources.insert(key.into(), raw.clone());
        let mut args = json!({"id":1,"category":"agents","language":"en","skill_level":2,"core_level":3,"selector":"/core_skills"});
        let view = kit.call("get_gachabase_entry", args.clone()).await.unwrap();
        assert_eq!(
            view["result"]["value"],
            json!([{"level":3,"skill_type_id":5,"text":"7"}])
        );
        let bindings =
            json!([{"source":view["source"],"path":"/skill_data/*/damage_multiplier_percent"}]);
        let shared = Mutex::new(kit);
        let value = KitTools::dispatch(
            &shared,
            "compute",
            json!({"script":"sum(x0)","bindings":bindings}),
        )
        .await
        .unwrap();
        assert_eq!(value["result"], 3.3);
        args["core_level"] = json!(4);
        assert!(
            KitTools::dispatch(&shared, "get_gachabase_entry", args.clone())
                .await
                .is_err()
        );
        args["core_level"] = Value::Null;
        args["skill_level"] = Value::Null;
        args["selector"] = json!("@raw");
        let original = KitTools::dispatch(&shared, "get_gachabase_entry", args)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(original["result"]["json_fragment"].as_str().unwrap())
                .unwrap(),
            raw
        );
        for script in ["bad_function(x0)", "1.0 / 0.0", "while true { }", "[[]]"] {
            assert!(
                KitTools::dispatch(
                    &shared,
                    "compute",
                    json!({"script":script,"bindings":bindings})
                )
                .await
                .is_err(),
                "{script}"
            );
        }
    }

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
        let mut args = json!({"category":"tutorials","id":301124,"language":"en","version":page.cur,"selector":null});
        let entry = json!({"name":{"key":"title","text":"Refringe"},"pages":[{"page":1,"text":{"text":"<color=#FFA9DD>Refringe</color>"}}]});
        tools.sources.insert(
            format!("tutorials/301124/en/{}/raw", page.cur),
            entry.clone(),
        );
        let compact = tools
            .call("get_gachabase_entry", args.clone())
            .await
            .unwrap();
        assert_eq!(
            compact["result"],
            json!({"name":"Refringe","pages":[{"page":1,"text":"Refringe"}]})
        );
        args["selector"] = json!("@raw");
        let full = tools.call("get_gachabase_entry", args).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(full["result"]["json_fragment"].as_str().unwrap())
                .unwrap(),
            entry
        );
    }
}
