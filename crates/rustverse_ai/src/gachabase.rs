use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Url};
use serde_json::Value;

use crate::diff::{Change, changes};

pub(crate) struct Snapshots {
    pub prev: Value,
    pub cur: Value,
}

impl Snapshots {
    pub fn changes(&self) -> Vec<Change> {
        changes(&self.prev, &self.cur)
    }
}

pub(crate) struct Comparison {
    endpoint: Url,
    pub id: u64,
    pub slug: String,
    pub prev: String,
    pub cur: String,
}

impl Comparison {
    pub fn parse(input: &str) -> Result<Self> {
        let mut url = Url::parse(input).context("Некорректная ссылка GachaBase")?;
        ensure!(
            url.origin().ascii_serialization() == "https://zzz.gachabase.net"
                && url.username().is_empty()
                && url.password().is_none(),
            "Ожидается ссылка https://zzz.gachabase.net"
        );
        let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
        let ["", "diff", "agents", id, slug] = parts.as_slice() else {
            bail!("Ожидается путь /diff/agents/<id>/<slug>");
        };
        let id = id.parse().context("Некорректный ID агента")?;
        ensure!(!slug.is_empty(), "Отсутствует slug агента");
        let slug = slug.to_string();
        let mut prev = None;
        let mut cur = None;
        for (key, value) in url.query_pairs() {
            let field = match key.as_ref() {
                "prev" => &mut prev,
                "cur" => &mut cur,
                "lang" => continue,
                _ => bail!("Неподдерживаемый параметр ссылки: {key}"),
            };
            ensure!(
                field.is_none() && !value.is_empty(),
                "Некорректный параметр {key}"
            );
            *field = Some(value.into_owned());
        }
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push("__data.json");
        url.set_query(None);
        url.set_fragment(None);
        Ok(Self {
            endpoint: url,
            id,
            slug,
            prev: prev.context("Отсутствует prev")?,
            cur: cur.context("Отсутствует cur")?,
        })
    }

    pub async fn fetch(&self, client: &Client, lang: &str) -> Result<Snapshots> {
        let page = self.page(client, &self.endpoint, lang).await?;
        self.extract(&page, lang)
            .with_context(|| format!("Не удалось прочитать дифф GachaBase ({lang})"))
    }

    async fn page(&self, client: &Client, endpoint: &Url, lang: &str) -> Result<Value> {
        let body: Value = client
            .get(endpoint.clone())
            .query(&[
                ("prev", self.prev.as_str()),
                ("cur", self.cur.as_str()),
                ("lang", lang),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("Некорректный JSON GachaBase")?;
        if body["type"] == "error" {
            bail!(
                "GachaBase: {}",
                body.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("ревизия недоступна")
            );
        }
        Ok(body)
    }

    /// Resolve the agent's canonical slug by ID, using the requested revision on both sides.
    pub async fn agent_snapshot(
        &self,
        client: &Client,
        id: u64,
        revision: &str,
        lang: &str,
    ) -> Result<Value> {
        let endpoint = self
            .endpoint
            .join(&format!("/diff/agents/{id}/__data.json"))?;
        let lookup = Self {
            endpoint,
            id,
            slug: String::new(),
            prev: revision.into(),
            cur: revision.into(),
        };
        let page = lookup.page(client, &lookup.endpoint, lang).await?;
        let page = if page["type"] == "redirect" {
            let location = page["location"]
                .as_str()
                .context("Отсутствует адрес страницы агента")?;
            let canonical = Self::parse(lookup.endpoint.join(location)?.as_str())?;
            ensure!(
                canonical.id == id && canonical.prev == revision && canonical.cur == revision,
                "GachaBase перенаправил на другого агента или ревизию"
            );
            lookup.page(client, &canonical.endpoint, lang).await?
        } else {
            page
        };
        let table = lookup.data_table(&page, lang)?;
        snapshot(table, "target")
    }

    fn extract(&self, page: &Value, lang: &str) -> Result<Snapshots> {
        let table = self.data_table(page, lang)?;
        Ok(Snapshots {
            prev: snapshot(table, "source")?,
            cur: snapshot(table, "target")?,
        })
    }

    fn data_table<'a>(&self, page: &'a Value, lang: &str) -> Result<&'a [Value]> {
        // SvelteKit's page data is a flat table. Object fields and array items
        // are references into that table, even when the actual value is numeric.
        let table = page["nodes"]
            .as_array()
            .context("Отсутствуют узлы страницы")?
            .iter()
            .filter_map(|node| node["data"].as_array())
            .find(|table| {
                table
                    .first()
                    .is_some_and(|root| root.get("devDiff").is_some())
            })
            .context("Отсутствуют данные сравнения")?;
        let root = &table[0];
        let meta = resolve(field(table, root, "devDiff")?, table, 0)?;
        ensure!(
            meta["lang"] == lang
                && meta["prev"] == self.prev
                && meta["cur"] == self.cur
                && meta["entity"] == "agents"
                && meta["id"].as_u64() == Some(self.id),
            "Язык, агент или ревизии в ответе отличаются от запроса"
        );
        Ok(table)
    }
}

fn snapshot(table: &[Value], side: &str) -> Result<Value> {
    let root = &table[0];
    let result = field(table, root, "data")?;
    let data = field(table, result, "data")?;
    let side = field(table, data, side)?;
    let dto = resolve(field(table, side, "dto")?, table, 0)?;
    let meta = field(table, root, "devDiff")?;
    let id = field(table, meta, "id")?;
    ensure!(&dto["id"] == id, "Отсутствует снимок агента {id}");
    let refs = field(table, side, "refs")?;
    let tooltips = if refs.get("tooltips").is_some() {
        resolve(field(table, refs, "tooltips")?, table, 0)?
    } else {
        Value::Null
    };
    Ok(crate::diff::snapshot(dto, tooltips))
}

pub(crate) async fn recent_revisions(client: &Client) -> Result<Vec<String>> {
    let page: Value = client
        .get("https://zzz.gachabase.net/timeline/__data.json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let table = page["nodes"]
        .as_array()
        .context("Отсутствует лента ревизий")?
        .iter()
        .filter_map(|node| node["data"].as_array())
        .find(|table| {
            table
                .first()
                .is_some_and(|root| root.get("timelineRevisions").is_some())
        })
        .context("Отсутствуют timelineRevisions")?;
    let rows = resolve(field(table, &table[0], "timelineRevisions")?, table, 0)?;
    beta_revision_names(&rows)
}

fn beta_revision_names(rows: &Value) -> Result<Vec<String>> {
    let mut revisions = Vec::new();
    for row in rows.as_array().context("Некорректная лента ревизий")? {
        if row["branch"] == "beta"
            && let (Some(version), Some(id), Some(time)) = (
                row["version"].as_str(),
                row["design_revision"].as_u64(),
                row["design_timestamp"].as_u64(),
            )
        {
            revisions.push((time, id, format!("beta_{version}_{id}")));
        }
    }
    revisions.sort_unstable_by(|a, b| b.cmp(a));
    let mut seen = std::collections::HashSet::new();
    Ok(revisions
        .into_iter()
        .map(|(_, _, name)| name)
        .filter(|name| seen.insert(name.clone()))
        .take(10)
        .collect())
}

pub(crate) fn referenced<'a>(table: &'a [Value], index: &Value) -> Result<&'a Value> {
    if index.as_i64() == Some(-1) {
        return Ok(&Value::Null); // devalue's undefined sentinel
    }
    index
        .as_u64()
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| table.get(i))
        .context("Некорректная ссылка в данных GachaBase")
}

pub(crate) fn field<'a>(table: &'a [Value], object: &Value, key: &str) -> Result<&'a Value> {
    referenced(
        table,
        object
            .get(key)
            .with_context(|| format!("Отсутствует поле {key}"))?,
    )
}

pub(crate) fn resolve(value: &Value, table: &[Value], depth: u8) -> Result<Value> {
    ensure!(
        depth < 64,
        "Слишком большая глубина вложенности данных GachaBase"
    );
    let child = |index: &Value| resolve(referenced(table, index)?, table, depth + 1);
    match value {
        Value::Array(items) => items
            .iter()
            .map(child)
            .collect::<Result<_>>()
            .map(Value::Array),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, i)| Ok((key.clone(), child(i)?)))
            .collect::<Result<_>>()
            .map(Value::Object),
        scalar => Ok(scalar.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUNBRINGER: &str = "https://zzz.gachabase.net/diff/agents/1601/sunbringer?prev=beta_3.4.5_20694646&cur=beta_3.4.5_20699967&lang=en";

    #[test]
    fn recent_beta_names_are_distinct_and_sorted_by_design_time() {
        let mut rows: Vec<_> = (1..=12)
            .map(|id| {
                json!({"branch":"beta","version":"3.4.5",
            "design_revision":id,"design_timestamp":20-id,"resource_revision":999})
            })
            .collect();
        rows.push(rows[0].clone());
        rows.push(json!({"branch":"release","version":"3.4.5","design_revision":13,"design_timestamp":100}));
        let names = beta_revision_names(&json!(rows)).unwrap();
        assert_eq!(
            names,
            (1..=10)
                .map(|id| format!("beta_3.4.5_{id}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reads_both_snapshots_and_rejects_locale_fallback() {
        let comparison = Comparison::parse(SUNBRINGER).unwrap();
        let page = json!({"nodes":[null, {"data":[
            {"data":1,"devDiff":2}, {"data":3},
            {"lang":4,"prev":5,"cur":6,"entity":7,"id":8},
            {"source":9,"target":10}, "en", comparison.prev, comparison.cur, "agents", 1601,
            {"dto":11,"refs":13}, {"dto":12,"refs":13},
            {"id":8,"damage":14}, {"id":8,"damage":15}, {}, 30, 45
        ]}]});
        let en = serde_json::to_value(comparison.extract(&page, "en").unwrap().changes()).unwrap();
        assert_eq!(en, json!([{"path":"agent.damage","before":30,"after":45}]));
        assert!(comparison.extract(&page, "chs").is_err());
        assert!(comparison.extract(&json!({"nodes":[]}), "en").is_err());
        assert!(
            Comparison::parse(&SUNBRINGER.replace("zzz.gachabase.net", "example.org")).is_err()
        );
        assert!(Comparison::parse(&SUNBRINGER.replace("prev=", "cur=")).is_err());
    }

    #[tokio::test]
    #[ignore = "fetches the public Sunbringer comparison from GachaBase"]
    async fn live_sunbringer_diff() {
        let page = Comparison::parse(SUNBRINGER).unwrap();
        let client = Client::new();
        let (en, cn) =
            tokio::try_join!(page.fetch(&client, "en"), page.fetch(&client, "chs")).unwrap();
        let m2 = |diff: &[Change]| {
            diff.iter()
                .find(|c| {
                    c.path.contains("mindscapes[")
                        && c.path.contains("level=2 ")
                        && c.path.ends_with(".description.text")
                })
                .unwrap()
                .after
                .clone()
                .unwrap()
        };
        let en = m2(&en.changes());
        let cn = m2(&cn.changes());
        assert!(en.as_str().unwrap().contains("Embermarks"));
        assert!(
            cn.as_str()
                .unwrap()
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        );
        assert_ne!(en, cn);
    }
}
