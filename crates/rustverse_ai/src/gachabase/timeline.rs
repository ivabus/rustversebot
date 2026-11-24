use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{field, resolve};
use crate::beta::Branch;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TimelineQuery {
    branch: String,
    version: Option<String>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct Build {
    branch: String,
    version: String,
    design_revision: u64,
    design_timestamp: i64,
}

impl Build {
    fn name(&self) -> String {
        format!("{}_{}_{}", self.branch, self.version, self.design_revision)
    }

    fn point(&self) -> Result<Value> {
        let time = DateTime::<Utc>::from_timestamp(self.design_timestamp, 0)
            .context("Invalid GachaBase design timestamp")?;
        Ok(
            json!({"revision":self.name(), "design_at":time.to_rfc3339_opts(SecondsFormat::Secs, true)}),
        )
    }
}

pub(crate) struct Timeline(Vec<Build>);

impl Timeline {
    #[tracing::instrument(target = "rustverse::bot", name = "gachabase.timeline", skip_all, err)]
    pub async fn load(client: &Client) -> Result<Self> {
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
            .find(|t| {
                t.first()
                    .is_some_and(|root| root.get("timelineRevisions").is_some())
            })
            .context("Отсутствуют timelineRevisions")?;
        Self::from_rows(&resolve(
            field(table, &table[0], "timelineRevisions")?,
            table,
            0,
        )?)
    }

    pub(crate) fn from_rows(rows: &Value) -> Result<Self> {
        let mut builds: Vec<Build> = rows
            .as_array()
            .context("Некорректная лента ревизий")?
            .iter()
            .filter(|r| matches!(r["branch"].as_str(), Some("beta" | "release")))
            .filter_map(|r| Build::deserialize(r).ok())
            .collect();
        builds.sort_unstable_by(|a, b| {
            (b.design_timestamp, b.design_revision, &b.branch, &b.version).cmp(&(
                a.design_timestamp,
                a.design_revision,
                &a.branch,
                &a.version,
            ))
        });
        let mut seen = HashSet::new();
        builds.retain(|b| seen.insert(b.name()));
        Ok(Self(builds))
    }

    pub fn recent(&self) -> Vec<String> {
        let mut counts = [0; 2];
        self.0
            .iter()
            .filter(|b| {
                let count = &mut counts[usize::from(b.branch == "release")];
                *count += 1;
                *count <= 10
            })
            .map(Build::name)
            .collect()
    }

    pub fn latest(&self, branch: Option<&str>) -> Option<String> {
        self.0
            .iter()
            .find(|b| branch.is_none_or(|s| b.branch == s))
            .map(Build::name)
    }

    pub fn page(&self, query: TimelineQuery) -> Result<Value> {
        const PAGE_SIZE: usize = 24;
        let branch = Branch::parse(&query.branch)?;
        let mut parts: Vec<_> = query
            .version
            .as_deref()
            .map(|s| s.split('.').collect())
            .unwrap_or_default();
        if parts.last().is_some_and(|p| p.eq_ignore_ascii_case("x")) {
            ensure!(
                (2..=3).contains(&parts.len()),
                "Expected a version such as 1.x or 1.4.x"
            );
            parts.pop();
        }
        ensure!(
            parts.len() <= 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "Expected a version such as 1.x, 1.4 or 1.4.0"
        );
        let matching = self.0.iter().rev().filter(|b| {
            b.branch == branch.name()
                && b.version
                    .split('.')
                    .take(parts.len())
                    .eq(parts.iter().copied())
        });
        let (kind, rows) = if parts.len() == 3 {
            (
                "revisions",
                matching.map(Build::point).collect::<Result<Vec<_>>>()?,
            )
        } else {
            let mut groups = Vec::<(&Build, &Build, usize)>::new();
            let mut indices = HashMap::new();
            for build in matching {
                let index = *indices.entry(&build.version).or_insert_with(|| {
                    groups.push((build, build, 0));
                    groups.len() - 1
                });
                groups[index].1 = build;
                groups[index].2 += 1;
            }
            let versions = groups
                .into_iter()
                .map(|(first, last, count)| {
                    Ok(json!({
                        "version":first.version, "first":first.point()?, "last":last.point()?,
                        "revision_count":count
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            ("versions", versions)
        };
        let offset = query.offset.unwrap_or(0);
        ensure!(
            offset <= rows.len(),
            "Offset exceeds timeline length {}",
            rows.len()
        );
        let end = (offset + PAGE_SIZE).min(rows.len());
        Ok(
            json!({"branch":branch.name(), "kind":kind, "entries":&rows[offset..end],
            "total": rows.len(), "next_offset":(end < rows.len()).then_some(end)}),
        )
    }
}
