use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(about = "Проанализировать изменения агента ZZZ по ссылке на дифф GachaBase")]
struct Args {
    /// Ссылка на дифф с параметрами prev и cur
    url: String,
    /// Модель OpenAI с поддержкой Responses API
    #[arg(long, env = "OPENAI_MODEL", default_value = rustverse_ai::DEFAULT_MODEL)]
    model: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _langfuse = rustverse_ai::telemetry::init("rustverse-ai")?;
    let key = rustverse_ai::api_key()?;
    println!(
        "{}",
        rustverse_ai::analyze(&args.url, &key, &args.model).await?
    );
    Ok(())
}
