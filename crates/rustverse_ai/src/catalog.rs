use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gachabase::{field, referenced, resolve};

const ORIGIN: &str = "https://zzz.gachabase.net";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Category {
    Agents,
    WEngines,
    DriveDiscs,
    Bangboo,
    Tutorials,
}

impl Category {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::WEngines => "w-engines",
            Self::DriveDiscs => "drive-discs",
            Self::Bangboo => "bangboo",
            Self::Tutorials => "tutorials",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Entry {
    id: u64,
    name: String,
    slug: String,
    #[serde(skip)]
    search: String,
}

pub(crate) struct Index {
    pub revision: String,
    entries: Vec<Entry>,
}

impl Index {
    pub async fn load(
        client: &Client,
        category: Category,
        lang: &str,
        revision: &str,
    ) -> Result<Self> {
        crate::beta::revision_group(revision)?;
        let path = if category == Category::Tutorials {
            "tutorials/all"
        } else {
            category.path()
        };
        let body = client
            .get(format!(
                "{ORIGIN}/{path}/{}/__data.json",
                revision.replace('_', "/")
            ))
            .query(&[("lang", lang)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Self::decode(&body, lang, revision)
    }

    fn decode(body: &str, lang: &str, expected: &str) -> Result<Self> {
        let mut revision = None;
        for frame in serde_json::Deserializer::from_str(body).into_iter::<Value>() {
            let frame = frame?;
            if frame["type"] == "data" {
                let layout = layout(&frame, lang)?;
                let revisions = field(layout, &layout[0], "revisions")?;
                let current = revisions
                    .get("current")
                    .map(|index| referenced(layout, index))
                    .transpose()?
                    .unwrap_or(&Value::Null);
                let data = resolve(
                    if current.is_null() {
                        field(layout, revisions, "latest")?
                    } else {
                        current
                    },
                    layout,
                    0,
                )?;
                let actual = format!(
                    "{}_{}_{}",
                    data["branch"].as_str().context("missing branch")?,
                    data["version"].as_str().context("missing version")?,
                    data["design_revision"]
                        .as_u64()
                        .context("missing design revision")?
                );
                ensure!(
                    actual == expected,
                    "Catalog revision mismatch: expected {expected}, got {actual}"
                );
                revision = Some(actual);
            }
            if frame["type"] != "chunk" {
                continue;
            }
            let t = frame["data"].as_array().context("missing catalog chunk")?;
            let root = t.first().context("empty catalog chunk")?;
            let data = field(t, root, "data")?;
            if data.get("entries").is_none() {
                continue;
            }
            let mut entries = Vec::new();
            for index in field(t, data, "entries")?
                .as_array()
                .context("invalid entries")?
            {
                let row = referenced(t, index)?;
                let name = field(t, row, "name")?;
                let text = field(t, name, "text")?.as_str().unwrap_or_default();
                let slug = field(t, row, "slug")?.as_str().unwrap_or_default();
                let search = field(t, row, "search_data")?.as_str().unwrap_or_default();
                entries.push(Entry {
                    id: field(t, row, "id")?.as_u64().context("missing entry ID")?,
                    name: text.into(),
                    slug: slug.into(),
                    search: format!("{text} {slug} {search}").to_lowercase(),
                });
            }
            return Ok(Self {
                entries,
                revision: revision.context("missing catalog revision")?,
            });
        }
        bail!("GachaBase did not return a catalog")
    }

    pub fn search(&self, category: Category, query: &str, offset: usize) -> Result<Value> {
        let terms = query.trim().to_lowercase();
        let id = terms.parse::<u64>().ok();
        let mut hits: Vec<_> = self
            .entries
            .iter()
            .filter(|e| Some(e.id) == id || terms.split_whitespace().all(|s| e.search.contains(s)))
            .collect();
        hits.sort_by_key(|e| {
            (
                Some(e.id) != id && e.name.to_lowercase() != terms && e.slug != terms,
                e.id,
            )
        });
        let end = offset.saturating_add(16).min(hits.len());
        ensure!(offset <= end, "Offset exceeds match count");
        Ok(
            json!({"category":category,"version":self.revision,"total":hits.len(),
            "matches":hits[offset..end],"next_offset":(end<hits.len()).then_some(end)}),
        )
    }
}

fn layout<'a>(frame: &'a Value, lang: &str) -> Result<&'a [Value]> {
    let t = frame
        .pointer("/nodes/0/data")
        .and_then(Value::as_array)
        .context("missing page layout")?;
    ensure!(
        field(t, t.first().context("empty layout")?, "language")? == lang,
        "GachaBase returned another language"
    );
    Ok(t)
}

pub(crate) fn agent_entry(snapshot: Value) -> Value {
    let Value::Object(mut fields) = snapshot else {
        return Value::Null;
    };
    let mut dto = fields.remove("agent").unwrap_or(Value::Null);
    if let Some(tips) = fields.remove("tooltips").filter(|v| !v.is_null()) {
        dto["tooltips"] = tips;
    }
    dto
}

pub(crate) async fn detail(
    client: &Client,
    category: Category,
    id: u64,
    version: &str,
    lang: &str,
) -> Result<Value> {
    let mut url = Url::parse(&format!(
        "{ORIGIN}/diff/{}/{id}/__data.json",
        category.path()
    ))?;
    for _ in 0..3 {
        let page: Value = client
            .get(url.clone())
            .query(&[("prev", version), ("cur", version), ("lang", lang)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if page["type"] == "redirect" {
            let canonical = url.join(
                page["location"]
                    .as_str()
                    .context("missing canonical location")?,
            )?;
            let prefix = format!("/diff/{}/", category.path());
            ensure!(
                canonical.origin() == url.origin()
                    && canonical.username().is_empty()
                    && canonical.password().is_none()
                    && canonical.path().starts_with(&prefix),
                "Invalid GachaBase redirect"
            );
            url = canonical;
            url.set_query(None);
            url.path_segments_mut()
                .unwrap()
                .pop_if_empty()
                .push("__data.json");
            continue;
        }
        layout(&page, lang)?;
        for t in page["nodes"]
            .as_array()
            .context("missing nodes")?
            .iter()
            .filter_map(|n| n["data"].as_array())
        {
            let Some(root) = t.first().filter(|v| v.get("data").is_some()) else {
                continue;
            };
            let wrapped = field(t, root, "data")?;
            let data = field(t, wrapped, "data")?;
            let ident = resolve(field(t, data, "targetIdent")?, t, 0)?;
            let actual = format!(
                "{}_{}_{}",
                ident["branch"].as_str().context("missing branch")?,
                ident["version"].as_str().context("missing version")?,
                ident["revision"].as_u64().context("missing revision")?
            );
            ensure!(
                actual == version && ident["id"] == id,
                "Requested entry or revision unavailable"
            );
            let target = field(t, data, "target")?;
            let dto = resolve(field(t, target, "dto")?, t, 0)?;
            ensure!(dto["id"] == id, "Entry absent in this revision");
            let refs = field(t, target, "refs")?;
            let tips = if refs.get("tooltips").is_some() {
                resolve(field(t, refs, "tooltips")?, t, 0)?
            } else {
                Value::Null
            };
            let mut dto = dto;
            if !tips.is_null() {
                dto["tooltips"] = tips;
            }
            return Ok(dto);
        }
        bail!("Entry unavailable for this version")
    }
    bail!("Too many GachaBase redirects")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_language_exact_id_and_localized_search() {
        let layout = json!({"type":"data","nodes":[{"data":[{"language":1,"revisions":2},"chs",{"current":3,"latest":3},{"branch":4,"version":5,"design_revision":6},"beta","3.4.5",42]}]});
        let chunk = json!({"type":"chunk","data":[{"data":1},{"entries":2},[3],{"id":4,"slug":5,"name":6,"search_data":8},1011,"anby",{"text":7},"安比","alias"]});
        let body = format!("{layout}\n{chunk}");
        assert!(Index::decode(&body, "en", "beta_3.4.5_42").is_err());
        assert!(Index::decode(&body, "chs", "release_3.4.5_42").is_err());
        let index = Index::decode(&body, "chs", "beta_3.4.5_42").unwrap();
        for query in ["1011", "安比", "ALIAS", "anby"] {
            assert_eq!(
                index.search(Category::Agents, query, 0).unwrap()["matches"][0]["id"],
                1011
            );
        }
    }
}
