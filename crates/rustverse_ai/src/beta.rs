//! Read the agent changes listed by the GachaBase beta changelog.
use crate::gachabase::{Comparison, Snapshots, field, referenced, resolve};
use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug)]
pub struct AgentChange {
    pub id: u64,
    pub name: String,
    pub slug: String,
    pub status: String,
    pub assets_changed: bool,
}

#[derive(Clone, Debug)]
pub struct Revision {
    pub previous: String,
    pub current: String,
    pub agents: Vec<AgentChange>,
}

fn revision_name(value: &Value) -> Result<String> {
    Ok(format!(
        "{}_{}_{}",
        value["branch"].as_str().context("missing branch")?,
        value["version"].as_str().context("missing version")?,
        value["design_revision"]
            .as_u64()
            .context("missing design revision")?
    ))
}

fn revision_group(rev: &str) -> Result<String> {
    let fields: Vec<_> = rev.split('_').collect();
    let ["beta", version, id] = fields.as_slice() else {
        bail!("ожидается beta_X.Y.Z_N")
    };
    let parts: Vec<_> = version.split('.').collect();
    ensure!(
        parts.len() == 3
            && parts
                .iter()
                .chain(std::iter::once(id))
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
        "ожидается beta_X.Y.Z_N"
    );
    Ok(format!("{}.{}.X", parts[0], parts[1]))
}

fn decode_group(body: &str) -> Result<(Vec<Revision>, Option<String>)> {
    // The changelog resolves a SvelteKit streamed promise in a second NDJSON item.
    for item in serde_json::Deserializer::from_str(body).into_iter::<Value>() {
        let item = item?;
        let tables = item["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|n| &n["data"])
            .chain(std::iter::once(&item["data"]));
        for table in tables.filter_map(Value::as_array) {
            let Some(root) = table.first() else { continue };
            let Ok(data) = field(table, root, "data") else {
                continue;
            };
            if data.get("revisions").is_none() {
                continue;
            }
            let older = field(table, data, "previousGroup")?
                .as_str()
                .map(str::to_owned);
            let mut releases = Vec::new();
            for index in field(table, data, "revisions")?
                .as_array()
                .context("missing revisions")?
            {
                let row = referenced(table, index)?;
                let rev = resolve(field(table, row, "revision")?, table, 0)?;
                let prev = resolve(field(table, row, "previousRevision")?, table, 0)?;
                if rev["branch"] != "beta" || prev.is_null() {
                    continue;
                }
                let mut agents = BTreeMap::new();
                for entry in field(table, row, "agents")?
                    .as_array()
                    .context("missing agent list")?
                {
                    let entry = resolve(referenced(table, entry)?, table, 0)?;
                    let id = entry["id"].as_u64().context("missing agent ID")?;
                    // The slug is cosmetic. The ID route also works for incomplete agents.
                    let slug = entry["href"]
                        .as_str()
                        .and_then(|href| href.strip_prefix("/agents/"))
                        .and_then(|p| p.split('/').nth(1))
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| id.to_string());
                    agents.insert(
                        id,
                        AgentChange {
                            id,
                            name: entry
                                .pointer("/name/text")
                                .and_then(Value::as_str)
                                .unwrap_or(&slug)
                                .to_owned(),
                            slug,
                            status: entry["status"]
                                .as_str()
                                .context("missing agent change status")?
                                .into(),
                            assets_changed: entry["assets_changed"] == true,
                        },
                    );
                }
                releases.push(Revision {
                    previous: revision_name(&prev)?,
                    current: revision_name(&rev)?,
                    agents: agents.into_values().collect(),
                });
            }
            return Ok((releases, older));
        }
    }
    bail!("GachaBase не вернул журнал изменений беты")
}

/// Adjacent design revisions in chronological order, excluding the starting revision.
/// With no range, returns the newest revision and its immediate predecessor.
pub async fn revisions(
    client: &Client,
    prev: Option<&str>,
    cur: Option<&str>,
) -> Result<Vec<Revision>> {
    ensure!(prev.is_some() == cur.is_some(), "нужны обе ревизии");
    if let Some(start) = prev {
        revision_group(start)?;
    }
    ensure!(
        prev.is_none() || prev != cur,
        "начальная и конечная ревизии совпадают"
    );
    let mut group = cur.map(revision_group).transpose()?;
    let mut seen = HashSet::new();
    let mut wanted = cur.map(str::to_owned);
    let mut selected = Vec::new();
    loop {
        ensure!(seen.insert(group.clone()), "повтор группы GachaBase");
        let mut url = reqwest::Url::parse("https://zzz.gachabase.net/changelog/beta/")?;
        {
            let mut path = url.path_segments_mut().unwrap();
            path.pop_if_empty();
            if let Some(group) = &group {
                path.push(group);
            }
            path.push("__data.json");
        }
        let body = client
            .get(url)
            .query(&[("lang", "en")])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let (rows, older) = decode_group(&body)?;
        for row in rows {
            if wanted.as_ref().is_some_and(|rev| rev != &row.current) {
                continue;
            }
            let done = prev.is_none_or(|start| start == row.previous);
            wanted = Some(row.previous.clone());
            selected.push(row);
            if done {
                selected.reverse();
                return Ok(selected);
            }
        }
        ensure!(
            !selected.is_empty(),
            "конечная ревизия отсутствует в журнале GachaBase"
        );
        if let Some(start) = prev
            && group == Some(revision_group(start)?)
        {
            bail!("начальная ревизия не предшествует конечной в этой группе");
        }
        group = Some(older.context("начальная ревизия отсутствует в истории выбранной беты")?);
    }
}

pub async fn agent_report(
    client: &Client,
    revision: &Revision,
    agent: &AgentChange,
    key: &str,
    model: &str,
) -> Result<String> {
    let mut url = reqwest::Url::parse("https://zzz.gachabase.net/diff/agents/")?;
    url.path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .push(&agent.id.to_string())
        .push(&agent.slug);
    url.query_pairs_mut()
        .append_pair("prev", &revision.previous)
        .append_pair("cur", &revision.current);
    let page = Comparison::parse(url.as_str())?;
    let comparison = &page;
    let snapshots = |lang| async move {
        match agent.status.as_str() {
            "new" | "added" => Ok(Snapshots {
                prev: Value::Null,
                cur: comparison
                    .agent_snapshot(client, agent.id, &revision.current, lang)
                    .await?,
            }),
            "removed" | "deleted" => Ok(Snapshots {
                prev: comparison
                    .agent_snapshot(client, agent.id, &revision.previous, lang)
                    .await?,
                cur: Value::Null,
            }),
            "updated" => comparison.fetch(client, lang).await,
            other => bail!("неизвестный тип изменения агента: {other}"),
        }
    };
    let (en, cn) = tokio::try_join!(snapshots("en"), snapshots("chs"))?;
    let mut report = super::analyze_snapshots(client, &page, &en, &cn, key, model).await?;
    if agent.assets_changed {
        report.push_str(
            "\n\n### Ресурсы персонажа\n- GachaBase отметил обновление игровых ресурсов.",
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changelog_keeps_empty_hotfixes_and_new_agents_without_diff_links() {
        let chunk = json!({"type":"chunk","id":1,"data":[
            {"data":1},{"revisions":2,"previousGroup":3},[4,5],null,
            {"revision":6,"previousRevision":7,"agents":8},
            {"revision":7,"previousRevision":9,"agents":10},
            {"branch":11,"version":12,"design_revision":13},
            {"branch":11,"version":12,"design_revision":14},[],
            {"branch":11,"version":12,"design_revision":15},[16],
            "beta","3.4.5",3,2,1,
            {"id":17,"name":18,"href":19,"status":20},1601,{"text":21},
            "/agents/1601/sunbringer/beta","new","Sunbringer"
        ]});
        let (rows, older) =
            decode_group(&format!("{{\"type\":\"data\",\"nodes\":[]}}\n{chunk}")).unwrap();
        assert!(older.is_none());
        assert_eq!(rows[0].current, "beta_3.4.5_3");
        assert!(rows[0].agents.is_empty());
        assert_eq!(rows[0].previous, rows[1].current);
        assert_eq!(rows[1].agents[0].slug, "sunbringer");
        assert_eq!(rows[1].agents[0].status, "new");
        assert!(revision_group("beta_3.x.5_1").is_err());
    }

    #[tokio::test]
    #[ignore = "reads a historical multi-revision changelog from GachaBase"]
    async fn kafka_range_includes_every_intermediate_hotfix() {
        let rows = revisions(
            &Client::new(),
            Some("beta_3.4.4_20541895"),
            Some("beta_3.4.5_20699967"),
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 4);
        assert!(
            rows.windows(2)
                .all(|pair| pair[0].current == pair[1].previous)
        );
        assert!(rows.iter().flat_map(|r| &r.agents).any(|a| a.id == 1651));
        assert_eq!(rows.last().unwrap().agents[0].id, 1601);
    }
}
