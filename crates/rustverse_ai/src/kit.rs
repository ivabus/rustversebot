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
    gachabase::{Comparison, Snapshots, Timeline, TimelineQuery},
};

pub(crate) const PAGE_CHARS: usize = 4096;

#[derive(Serialize)]
pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) parameters: Value,
}

pub(crate) fn version_parameter() -> Value {
    json!({"type":["string","null"],"description":"Exact revision (beta_X.Y.Z_N, creator_X.Y.Z_N, release_X.Y.Z_N) or branch ('release' or 'beta'). For current live endgame use 'release'. Null reuses the pinned version or defaults to the newest. Reuse the returned version for later pages."})
}

pub(crate) fn definitions() -> Vec<ToolSpec> {
    let category = json!({"type":"string","enum":["agents","w-engines","drive-discs","bangboo","enemies","tutorials"]});
    let language = json!({"type":"string","enum":["en","cn","ru"],"description":"Use en or cn for game mechanics. Use ru for Russian terminology."});
    let offset = json!({"type":["integer","null"],"minimum":0,"description":"Start at 0 or null. Copy next_offset to continue."});
    let version = version_parameter();
    let mut search_version = version.clone();
    search_version["description"] = json!(
        "For an unversioned build or stat goal, look up the agent by name with 'release_then_beta': check release first, then beta on a miss. For an explicitly requested branch, live endgame or an exact patch, use the requested branch or revision. Null reuses the pinned revision or the newest available."
    );
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
            name: "list_revision_timeline",
            description: "Find historical GachaBase design revisions by branch. Null version or a series (1.x, 1.4) lists versions with first and last archived builds. An exact patch (1.4.0) lists its individual revisions. Entries start with the oldest. Follow next_offset. Pass an exact revision to search_gachabase or get_gachabase_entry. design_at is the UTC timestamp of the design build.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "branch":{"type":"string","enum":["release","beta"]},
                "version":{"type":["string","null"]},"offset":offset
            },"required":["branch","version","offset"]}),
        },
        ToolSpec {
            name: "search_gachabase",
            description: "Search by name, alias, slug or ID. For a season boss use category=enemies and the monster ID from Nanoka (or its localized name). For a roster use category=agents, query='', offset=null: returns all matching agents at the selected revision. Each row has beta_only: false if its ID is in release_revision, true if present only in the selected beta compared with the latest release, null if the comparison is unavailable. Filter agents by specialty, element or both (AND), using catalog names in the selected language. Prefer entries with core skills when names collide.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "category":category,"query":{"type":"string"},"language":language,"offset":offset,"version":search_version,
                "specialty":{"type":["string","null"]},"element":{"type":["string","null"]}
            },"required":["category","query","language","offset","version","specialty","element"]}),
        },
        ToolSpec {
            name: "get_gachabase_entry",
            description: "Read by ID. For enemies, null selector gives combat and codex descriptions. /enemy_variants contains variant affinities. For agents, @team summarizes Core Passive and Additional Ability; follow a path for exact conditions. Null returns compact pages, empty lists fields, a JSON pointer reads a field, @raw reads the full record. Name/ID or text:<phrase> finds paths. Agent levels default to skill 12 and max core. Use source and JSON pointers for compute.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{
                "category":category,"id":{"type":"integer","minimum":1},"language":language,
                "version":version,"selector":{"type":["string","null"]},"offset":offset,
                "skill_level":{"type":["integer","null"],"minimum":1,"maximum":16},"core_level":{"type":["integer","null"],"minimum":1}
            },"required":["category","id","language","version","selector","offset","skill_level","core_level"]}),
        },
        ToolSpec {
            name: "compute",
            description: "Run Rhai on this run's completed tool calls and optional numeric bindings. calls is a read-only array in completion order. Each call has name, args and result (including errors and prior compute results). Pass bindings: [] to use calls alone. Example after a season batch: calls[-1].result.results[0].season_id. Filter calls by name and args when reading several. Iterate JSON arrays by value and objects by key. obj.keys() lists keys; obj[key] reads a field. Local fn and array.push(value) are available. JSON integers retain integer arithmetic; multiply by 1.0 before fractional division. Each explicit binding reads an exact source and JSON pointer, with * segments to collect numbers across nested arrays (index order) and objects (key order), e.g. /rows/*/stats/*/value. Wildcard inputs include concrete paths and values in matches. For a GachaBase fragment, append the path inside its value to result.path (e.g. /base_stats/6/value). Bindings address the source document, so omit the response wrapper /result/value. Use its name or x0, x1, ... by position; bound numbers are floats. Return a finite number or a flat array of up to 64 numbers.",
            parameters: json!({"type":"object","additionalProperties":false,"properties":{"script":{"type":"string","description":"Rhai expression or short script, up to 2048 bytes."},"bindings":{"type":"array","maxItems":12,"items":{"anyOf":[named,reference]}}},"required":["script","bindings"]}),
        },
    ]
}

#[derive(Clone, Copy, Hash, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Language {
    En,
    Cn,
    Ru,
}

impl Language {
    fn upstream(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Cn => "chs",
            Self::Ru => "ru",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
                    version: result["version"].as_str().map(str::to_owned),
                    page: result["offset"].as_u64().unwrap_or_default() as usize,
                    path: result["path"].as_str().map(str::to_owned),
                });
            }
            self.revisions.extend(
                ["version", "release_revision"]
                    .iter()
                    .filter_map(|k| result[k].as_str().map(str::to_owned)),
            );
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
    pinned_season_version: Option<String>,
    pinned: Option<String>,
    timeline: Option<Timeline>,
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
            pinned_season_version: None,
            pinned: None,
            report_changes: HashMap::new(),
            timeline: None,
        }
    }

    pub fn for_question(client: Client, question: &str) -> Self {
        static EXACT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\b(?:beta|release|creator)_[0-9]+\.[0-9]+\.[0-9]+_[0-9]+\b")
                .unwrap()
        });
        let mut tools = Self::new(client);
        tools.pinned = EXACT.find(question).map(|v| v.as_str().to_owned());
        tools.seasons = Some(crate::seasons::SeasonTools::new(nanoka::NanokaClient::new()));
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
        let input = json!({"agent_id":page.id,"agent":page.slug,"prev":page.prev,"cur":page.cur,"status":status});
        self.pinned = Some(page.cur.clone());
        let mut empty = true;
        for (language, snapshots) in [(Language::En, en), (Language::Cn, cn)] {
            let mut prepare = |raw: Value, version: &str| {
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
                    .insert(source, catalog::agent_entry(view.clone()));
                view
            };
            let normalized = Snapshots {
                prev: prepare(snapshots.prev, &page.prev),
                cur: prepare(snapshots.cur, &page.cur),
            };
            let changes = normalized.changes();
            empty &= changes.is_empty();
            self.report_changes
                .insert((page.id, language, page.cur.clone()), Groups::new(changes));
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

    async fn timeline(&mut self) -> Result<&Timeline> {
        if self.timeline.is_none() {
            self.timeline = Some(Timeline::load(&self.client).await?);
        }
        Ok(self.timeline.as_ref().unwrap())
    }

    async fn index(
        &mut self,
        category: Category,
        language: Language,
        revision: &str,
    ) -> Result<&Index> {
        Ok(
            match self
                .indexes
                .entry((category, language, revision.to_owned()))
            {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(
                    Index::load(&self.client, category, language.upstream(), revision).await?,
                ),
            },
        )
    }

    async fn latest_release_agents(&mut self, language: Language) -> Result<&Index> {
        let revision = self
            .timeline()
            .await?
            .latest(Some("release"))
            .context("No release revisions")?;
        self.index(Category::Agents, language, &revision).await
    }

    async fn find_build_agent(&mut self, query: &str, language: Language) -> Result<Value> {
        let release = self.latest_release_agents(language).await?;
        let mut result = release.search(Category::Agents, query, 0, None, None)?;
        let release_revision = release.revision.clone();
        if result["total"] == 0 {
            let beta = self
                .timeline()
                .await?
                .latest(Some("beta"))
                .context("No beta revisions")?;
            result = self
                .index(Category::Agents, language, &beta)
                .await?
                .search(Category::Agents, query, 0, None, None)?;
        }
        if result["total"] != 0 {
            self.pinned.get_or_insert_with(|| {
                result["version"]
                    .as_str()
                    .expect("catalog revision")
                    .to_owned()
            });
        }
        result["release_revision"] = json!(release_revision);
        Ok(result)
    }

    async fn season_request(
        &mut self,
        name: &str,
        mut args: Value,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Value> {
        let version = args
            .as_object_mut()
            .context("Expected season request")?
            .remove("version");
        let mut version: Option<String> = serde_json::from_value(version.unwrap_or_default())?;
        if version.as_deref() == Some("beta") {
            // Nanoka's manifest lists build numbers without their beta/creator branch.
            version = Some(
                self.timeline()
                    .await?
                    .latest(Some("beta"))
                    .context("No beta revisions")?,
            );
        }
        let requested = version
            .as_ref()
            .or(self.pinned_season_version.as_ref())
            .or(self.pinned.as_ref());
        let (resolved, snapshot) = self
            .seasons
            .as_mut()
            .context("Season tools unavailable")?
            .snapshot(requested.map(String::as_str))
            .await?;
        let mut result = snapshot.call(name, args, &mut self.sources, now).await?;
        result["version"] = json!(resolved);
        result["nanoka_version"] = json!(snapshot.version);
        self.pinned_season_version
            .get_or_insert_with(|| resolved.clone());
        if resolved.contains('_') {
            self.pinned.get_or_insert(resolved);
        }
        Ok(result)
    }

    async fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        match name {
            "list_endgame_seasons" | "get_endgame_season" => {
                self.season_request(name, args, chrono::Utc::now()).await
            }
            "get_endgame_seasons" => {
                let mut results = Vec::new();
                let now = chrono::Utc::now();
                for request in crate::seasons::batch_requests(args)? {
                    let result = self
                        .season_request("get_endgame_season", request.clone(), now)
                        .await
                        .unwrap_or_else(|err| json!({"request":request,"error":err.to_string()}));
                    results.push(result);
                }
                Ok(json!({"results":results}))
            }
            "list_recent_revisions" => {
                ensure!(args == json!({}), "Expected empty arguments");
                Ok(json!(self.timeline().await?.recent()))
            }
            "list_revision_timeline" => {
                let query: TimelineQuery = serde_json::from_value(args)?;
                self.timeline().await?.page(query)
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
                if q.version.as_deref() == Some("release_then_beta") {
                    ensure!(
                        q.category == Category::Agents
                            && !q.query.trim().is_empty()
                            && q.offset.unwrap_or_default() == 0
                            && q.specialty.is_none()
                            && q.element.is_none(),
                        "release_then_beta requires an unfiltered first-page agent name or ID; use the returned revision for later pages"
                    );
                    return self.find_build_agent(&q.query, q.language).await;
                }
                let version = self.version(q.version).await?;
                let index = self.index(q.category, q.language, &version).await?;
                let mut result = index.search(
                    q.category,
                    &q.query,
                    q.offset.unwrap_or_default(),
                    q.specialty.as_deref(),
                    q.element.as_deref(),
                )?;
                if q.category == Category::Agents && q.query.trim().is_empty() {
                    let release = if version.starts_with("release_") {
                        Some(index)
                    } else {
                        self.latest_release_agents(q.language)
                            .await
                            .inspect_err(|err| {
                                log::warn!(
                                    "Could not compare the beta roster against release: {err}"
                                );
                            })
                            .ok()
                    };
                    result["release_revision"] = json!(release.map(|r| &r.revision));
                    for agent in result["matches"]
                        .as_array_mut()
                        .expect("catalog matches are an array")
                    {
                        let id = agent["id"]
                            .as_u64()
                            .expect("catalog entry has a numeric ID");
                        agent["beta_only"] = json!(release.map(|r| !r.contains(id)));
                    }
                }
                Ok(result)
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
                        json!({"id":q.id,"version":version,"language":q.language,"selector":selector,
                               "result":changes.read(selector,q.offset.unwrap_or_default())?}),
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
                    json!({"category":q.category,"id":q.id,"version":version,"language":q.language,"source":source,"result":result}),
                )
            }
            _ => bail!("Unknown tool"),
        }
    }

    async fn version(&mut self, requested: Option<String>) -> Result<String> {
        let version = match requested.or_else(|| self.pinned.clone()) {
            None => self
                .timeline()
                .await?
                .latest(None)
                .context("No beta or release revisions")?,
            Some(branch) if matches!(branch.as_str(), "beta" | "release") => self
                .timeline()
                .await?
                .latest(Some(&branch))
                .context("No revision for requested branch")?,
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

    fn fixture_timeline(newest_first: &[&str]) -> Timeline {
        let rows: Vec<_> = newest_first.iter().rev().enumerate().map(|(time, name)| {
            let parts: Vec<_> = name.split('_').collect();
            json!({"branch":parts[0],"version":parts[1],"design_revision":parts[2].parse::<u64>().unwrap(),"design_timestamp":time})
        }).collect();
        Timeline::from_rows(&json!(rows)).unwrap()
    }

    #[tokio::test]
    async fn historical_timeline_pages_versions_and_keeps_the_current_release() {
        let mut rows: Vec<_> = (1..=26)
            .map(|id| {
                json!({
                    "branch":"release","version":"1.4.0","design_revision":id,"design_timestamp":id
                })
            })
            .collect();
        for (branch, version, id) in [
            ("release", "1.4.0", 1),
            ("release", "1.40.0", 18),
            ("beta", "1.4.0", 27),
            ("creator", "1.4.0", 28),
            ("release", "3.3.0", 30),
        ] {
            rows.push(json!({"branch":branch,"version":version,"design_revision":id,"design_timestamp":id}));
        }
        let mut kit = KitTools::for_question(Client::new(), "История 1.x");
        kit.timeline = Some(Timeline::from_rows(&json!(rows)).unwrap());
        let query = |version: &str, offset: usize| json!({"branch":"release","version":version,"offset":offset});
        let tool = "list_revision_timeline";
        let series = kit.call(tool, query("1.x", 0)).await.unwrap();
        assert_eq!(series["total"], 2);
        let patch = kit.call(tool, query("1.4", 0)).await.unwrap();
        assert_eq!(patch["total"], 1);
        assert_eq!(
            patch["entries"][0],
            json!({
                "version":"1.4.0","revision_count":26,
                "first":{"revision":"release_1.4.0_1","design_at":"1970-01-01T00:00:01Z"},
                "last":{"revision":"release_1.4.0_26","design_at":"1970-01-01T00:00:26Z"}
            })
        );
        let first = kit.call(tool, query("1.4.0", 0)).await.unwrap();
        assert_eq!(first["next_offset"], 24);
        let second = kit.call(tool, query("1.4.0", 24)).await.unwrap();
        assert_eq!(second["entries"][0]["revision"], "release_1.4.0_25");
        assert!(second["next_offset"].is_null());
        assert_eq!(kit.call(tool, query("9.x", 0)).await.unwrap()["total"], 0);
        assert!(kit.call(tool, query("1..4", 0)).await.is_err());
        assert!(kit.call(tool, query("1.4.0", 27)).await.is_err());
        assert_eq!(
            kit.version(Some("release".into())).await.unwrap(),
            "release_3.3.0_30"
        );
    }

    #[tokio::test]
    async fn enemy_search_and_combat_description_use_the_requested_release() {
        let version = "release_3.4.0_42";
        let layout = json!({"type":"data","nodes":[{"data":[{"language":1,"revisions":2},"en",
            {"current":3},{"branch":4,"version":5,"design_revision":6},"release","3.4.0",42]}]});
        let rows = json!({"type":"chunk","data":[{"data":1},{"entries":2},[3],
            {"id":4,"slug":5,"name":6,"search_data":7},40009,"integrated-girtablullu",{"text":7},"Integrated - Girtablullu"]});
        let index = Index::decode(&format!("{layout}\n{rows}"), "en", version).unwrap();
        let mut kit = KitTools::for_question(Client::new(), "Текущий штурм");
        kit.timeline = Some(fixture_timeline(&["beta_3.5.0_99", version]));
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
    async fn unversioned_build_search_reads_release_first_and_resolves_beta_only_agents() {
        let beta = "beta_3.4.5_42";
        let release = "release_3.3.0_10";
        let mut kit = KitTools::for_question(Client::new(), "цель билда робин");
        kit.timeline = Some(fixture_timeline(&[beta, release]));
        for (branch, patch, id, revision, entries) in [
            ("release", "3.3.0", 10, release, vec![3]),
            ("beta", "3.4.5", 42, beta, vec![3, 4]),
        ] {
            let layout = json!({"type":"data","nodes":[{"data":[{"language":1,"revisions":2},"en",
                {"current":3},{"branch":4,"version":5,"design_revision":6},branch,patch,id]}]});
            let rows = json!({"type":"chunk","data":[{"data":1},{"entries":2},entries,
                {"id":5,"name":7,"slug":8,"search_data":8},{"id":6,"name":9,"slug":10,"search_data":10},
                1641,1661,{"text":11},"phoenix",{"text":12},"robin","Phoenix","Robin"]});
            kit.indexes.insert(
                (Category::Agents, Language::En, revision.into()),
                Index::decode(&format!("{layout}\n{rows}"), "en", revision).unwrap(),
            );
        }
        let search = |query: &str, version: &str| json!({"category":"agents","language":"en","query":query,"version":version});
        let tool = "search_gachabase";
        let found = kit
            .call(tool, search("Robin", "release_then_beta"))
            .await
            .unwrap();
        assert_eq!(found["release_revision"], release);
        assert_eq!(found["version"], beta);
        assert_eq!(found["matches"][0]["id"], 1661);
        assert_eq!(kit.version(None).await.unwrap(), beta);
        for version in ["release", release] {
            let miss = kit.call(tool, search("Robin", version)).await.unwrap();
            assert_eq!(miss["version"], release);
            assert_eq!(miss["total"], 0);
        }
        let neither = kit
            .call(tool, search("Nobody", "release_then_beta"))
            .await
            .unwrap();
        assert_eq!(neither["total"], 0);
        assert_eq!(neither["version"], beta);
        // A release hit still works when no beta is available.
        kit.timeline = Some(fixture_timeline(&[release]));
        let common = kit
            .call(tool, search("Phoenix", "release_then_beta"))
            .await
            .unwrap();
        assert_eq!(common["version"], release);
        assert_eq!(common["matches"][0]["id"], 1641);
    }

    #[tokio::test]
    async fn roster_distinguishes_beta_only_ids_and_reports_missing_release() {
        let beta = "beta_3.4.5_42";
        let release = "release_3.3.0_10";
        let mut kit = KitTools::for_question(Client::new(), "Полный ростер");
        // The same display name can occur on distinct IDs and must not merge them.
        for (branch, patch, number, revision, entries) in [
            ("beta", "3.4.5", 42, beta, vec![3, 4]),
            ("release", "3.3.0", 10, release, vec![3]),
        ] {
            let layout = json!({"type":"data","nodes":[{"data":[{"language":1,"revisions":2},"en",
                {"current":3},{"branch":4,"version":5,"design_revision":6},branch,patch,number]}]});
            let rows = json!({"type":"chunk","data":[{"data":1},{"entries":2},entries,
                {"id":5,"name":7,"slug":8,"search_data":8},{"id":6,"name":7,"slug":8,"search_data":8},
                1641,9999,{"text":9},"phoenix","Phoenix"]});
            let index = Index::decode(&format!("{layout}\n{rows}"), "en", revision).unwrap();
            kit.indexes
                .insert((Category::Agents, Language::En, revision.into()), index);
        }
        let roster = |version: &str| json!({"category":"agents","query":"","language":"en","version":version});
        kit.timeline = Some(fixture_timeline(&[beta, release]));
        let result = kit.call("search_gachabase", roster("beta")).await.unwrap();
        assert_eq!(result["version"], beta);
        assert_eq!(result["release_revision"], release);
        assert_eq!(result["matches"][0]["id"], 1641);
        assert_eq!(result["matches"][0]["beta_only"], false);
        assert_eq!(result["matches"][1]["beta_only"], true);
        assert_eq!(kit.version(None).await.unwrap(), beta);
        let result = kit
            .call("search_gachabase", roster("release"))
            .await
            .unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["matches"][0]["beta_only"], false);
        kit.timeline = Some(fixture_timeline(&[beta]));
        let result = kit.call("search_gachabase", roster(beta)).await.unwrap();
        assert_eq!(result["total"], 2);
        assert_eq!(result.get("release_revision"), Some(&Value::Null));
        assert_eq!(result["matches"][0].get("beta_only"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn questions_pin_the_newest_or_the_requested_revision() {
        let beta = "beta_3.4.5_99";
        let release = "release_3.3.0_10";
        for (newest, older, branch) in [(beta, release, "release"), (release, beta, "beta")] {
            let mut kit = KitTools::for_question(Client::new(), "Как работает навык?");
            kit.timeline = Some(fixture_timeline(&[newest, older]));
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
        assert!(!input.contains("older effect") && !input.contains("new effect"));
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
        let ru_source = format!("tutorials/301124/ru/{}/raw", page.cur);
        tools.sources.insert(
            ru_source.clone(),
            json!({"name":{"text":"Название из перевода"},"pages":[]}),
        );
        let translated = tools
            .call(
                "get_gachabase_entry",
                json!({"category":"tutorials","id":301124,"language":"ru","version":page.cur}),
            )
            .await
            .unwrap();
        assert_eq!(translated["language"], "ru");
        assert_eq!(translated["source"], ru_source);
        assert_eq!(translated["result"]["name"], "Название из перевода");
    }
}
