//! Character pages for beta changelogs. Each report is a separate, auto-height PNG.
use anyhow::{Context, Result};
use serde_json::json;

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
        &json!({"agent":name,"revisions":revisions.replace('`',""),"lines":lines}),
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(dimension(&short, 16), 1280);
        assert_eq!(dimension(&long, 16), 1280);
        assert!(dimension(&long, 20) > dimension(&short, 20) * 3);
    }
}
