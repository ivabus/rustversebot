//! Character pages for beta changelogs. Each report is a separate, auto-height PNG.
use anyhow::{Context, Result};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Serialize;
use serde_json::{Value, json};

pub async fn beta_page(markdown: &str) -> Result<Vec<u8>> {
    let mut lines = markdown.lines().filter(|line| !line.trim().is_empty());
    let name = lines
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .context("missing agent heading")?;
    let revisions = lines
        .next()
        .and_then(|line| line.strip_prefix("## "))
        .context("missing revision subtitle")?;
    let lines: Vec<_> = lines.map(str::trim).collect();
    anyhow::ensure!(!lines.is_empty(), "empty beta report");
    crate::render(
        crate::Template::BetaAgent,
        &json!({"agent":name,"revisions":revisions,"lines":lines}),
        None,
    )
    .await
}

#[derive(Serialize)]
struct Span {
    text: String,
    strong: bool,
    code: bool,
}

fn inline(line: &str) -> Vec<Span> {
    // Only consume markup that the card actually renders. In particular, keep
    // numbered steps and literal game notation from older cached reports.
    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut strong_depth = 0usize;
    for (event, range) in Parser::new(line).into_offset_iter() {
        let (range, next_depth, code) = match event {
            Event::Start(Tag::Strong) => (range.start..range.start + 2, strong_depth + 1, None),
            Event::End(TagEnd::Strong) => (range.end - 2..range.end, strong_depth - 1, None),
            Event::Code(text) => (range, strong_depth, Some(text)),
            _ => continue,
        };
        if cursor < range.start {
            spans.push(Span {
                text: line[cursor..range.start].to_owned(),
                strong: strong_depth > 0,
                code: false,
            });
        }
        if let Some(text) = code {
            spans.push(Span {
                text: text.into_string(),
                strong: strong_depth > 0,
                code: true,
            });
        }
        cursor = range.end;
        strong_depth = next_depth;
    }
    if cursor < line.len() {
        spans.push(Span {
            text: line[cursor..].to_owned(),
            strong: false,
            code: false,
        });
    }
    spans
}

pub(super) fn template_data(input: &Value) -> Result<Value> {
    let mut lines = Vec::new();
    for line in input["lines"]
        .as_array()
        .context("missing beta report lines")?
    {
        let line = line.as_str().context("expected beta report line")?;
        let (kind, body) = if line.starts_with('#') {
            ("heading", line.trim_start_matches('#').trim_start())
        } else if let Some(body) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            ("bullet", body)
        } else {
            ("paragraph", line)
        };
        lines.push(json!({"kind":kind, "spans":inline(body)}));
    }
    Ok(json!({
        "agent":input["agent"],
        "revisions":inline(input["revisions"].as_str().context("missing beta report revisions")?),
        "lines":lines,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_code_preserves_literals_and_surrounding_emphasis() {
        let spans = inline("1. **До `ExQTE`** → `NormalQTE`, ``tag`**``.");
        let code: Vec<_> = spans
            .iter()
            .filter(|s| s.code)
            .map(|s| (s.text.as_str(), s.strong))
            .collect();
        assert_eq!(
            code,
            [("ExQTE", true), ("NormalQTE", false), ("tag`**", false)]
        );
        assert_eq!(
            spans.iter().map(|s| s.text.as_str()).collect::<String>(),
            "1. До ExQTE → NormalQTE, tag`**."
        );
    }

    #[tokio::test]
    async fn a_complete_agent_report_stays_on_one_auto_height_page() {
        let header = "# Кафка\n\n## `beta_3.4.4_20541895` → `beta_3.4.5_20691792`\n\n";
        let section = "### Базовая атака\n- Урон при попадании вырос с **25%** до **30%**.\n";
        let short = beta_page(&format!("{header}{section}")).await.unwrap();
        let long = beta_page(&format!("{header}{}", section.repeat(12)))
            .await
            .unwrap();
        let dimension =
            |png: &[u8], index| u32::from_be_bytes(png[index..index + 4].try_into().unwrap());
        assert_eq!(dimension(&short, 16), 2560);
        assert_eq!(dimension(&long, 16), 2560);
        assert!(dimension(&long, 20) > dimension(&short, 20) * 3);
    }
}
