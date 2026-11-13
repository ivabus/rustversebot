use anyhow::Context;
use rustverse_images::{Template, render};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: rustverse_images <template.typ> <input.json> <output.png> [season.json]";
    let template = Template::parse(&args.next().context(usage)?)?;
    let input = serde_json::from_slice(&tokio::fs::read(args.next().context(usage)?).await?)?;
    let output = args.next().context(usage)?;
    let season = match args.next() {
        Some(path) => Some(serde_json::from_slice(&tokio::fs::read(path).await?)?),
        None => None,
    };
    anyhow::ensure!(args.next().is_none(), "{usage}");
    let png = render(template, &input, season.as_ref()).await?;
    tokio::fs::write(output, png).await?;
    Ok(())
}
