use anyhow::Context;
use rustverse_images::{Template, render};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: rustverse_images <template.typ> <input.json> <output.png> [season.json]\n       rustverse_images beta <report.md> <output.png>\n       rustverse_images infographic <layout.typ> <output.png>";
    let command = args.next().context(usage)?;
    let input = args.next().context(usage)?;
    let output = args.next().context(usage)?;
    if command == "infographic" {
        anyhow::ensure!(args.next().is_none(), "{usage}");
        let source = tokio::fs::read_to_string(input).await?;
        tokio::fs::write(output, rustverse_images::render_infographic(source).await?).await?;
        return Ok(());
    }
    if command == "beta" {
        anyhow::ensure!(args.next().is_none(), "{usage}");
        let report = tokio::fs::read_to_string(input).await?;
        tokio::fs::write(output, rustverse_images::beta_page(&report).await?).await?;
        return Ok(());
    }
    let template = Template::parse(&command)?;
    let input = serde_json::from_slice(&tokio::fs::read(input).await?)?;
    let season = match args.next() {
        Some(path) => Some(serde_json::from_slice(&tokio::fs::read(path).await?)?),
        None => None,
    };
    anyhow::ensure!(args.next().is_none(), "{usage}");
    let png = render(template, &input, season.as_ref()).await?;
    tokio::fs::write(output, png).await?;
    Ok(())
}
