use anyhow::Context;
use nanoka::{
    NanokaClient,
    types::{AnySeasonDetail, EndgameType},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let season_id = args
        .next()
        .context("usage: render_season <season-id> <output.png>")?
        .parse::<u64>()
        .context("season ID must be numeric")?;
    let output = args
        .next()
        .context("usage: render_season <season-id> <output.png>")?;
    anyhow::ensure!(args.next().is_none(), "too many arguments");

    let client = NanokaClient::new();
    let detail = client
        .get_detail_resolved(season_id)
        .await
        .with_context(|| format!("fetching resolved season {season_id}"))?;
    let png = match detail {
        AnySeasonDetail::Shiyu(detail) => {
            rustverse_svg::preload_shiyu_info_images(&detail).await?;
            rustverse_svg::shiyu_info(&detail)?
        }
        AnySeasonDetail::Boss(detail) => {
            let seasons = client
                .get_seasons_by_type(EndgameType::DeadlyAssault)
                .await
                .context("fetching Deadly Assault season index")?;
            let begin_time = seasons
                .get(&season_id.to_string())
                .and_then(|meta| meta.live_begin.as_deref().or(meta.begin.as_deref()));
            rustverse_svg::preload_deadly_info_images(&detail).await?;
            rustverse_svg::deadly_info_with_begin_time(&detail, begin_time)?
        }
    };
    std::fs::write(&output, png).with_context(|| format!("writing {output}"))?;
    println!("{output}");
    Ok(())
}
