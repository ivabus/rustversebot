//! Typst → SVG → PNG rendering. Each invocation owns its input files and processes.
mod beta;
const HIRES_WIDTH: &str = "2560";
pub use beta::beta_page;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, ensure};
use nanoka::types::{BossSeasonDetail, SeasonDetail};
use rustverse::models::zzz::{ZZZDeadlyAssault, ZZZShiyuDefense};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{fs, process::Command};

#[derive(Debug, Clone, Copy)]
pub enum Template {
    BetaAgent,
    TopDa,
    TopShiyu,
    Da,
    Shiyu,
    DeadlyInfo,
    ShiyuInfo,
}

impl Template {
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name {
            "beta_agent" | "beta_agent.typ" => Ok(Self::BetaAgent),
            "top_da" | "top_da.typ" => Ok(Self::TopDa),
            "top_sd" | "top_sd.typ" => Ok(Self::TopShiyu),
            "individual_da" | "individual_da.typ" => Ok(Self::Da),
            "individual_shiyu" | "individual_shiyu.typ" => Ok(Self::Shiyu),
            "endgame_da" | "endgame_da.typ" => Ok(Self::DeadlyInfo),
            "endgame_shiyu" | "endgame_shiyu.typ" => Ok(Self::ShiyuInfo),
            _ => anyhow::bail!("unknown image template: {name}"),
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::BetaAgent => include_str!("../beta_agent.typ"),
            Self::TopDa => include_str!("../top_da.typ"),
            Self::TopShiyu => include_str!("../top_sd.typ"),
            Self::Da => include_str!("../individual_da.typ"),
            Self::Shiyu => include_str!("../individual_shiyu.typ"),
            Self::DeadlyInfo => include_str!("../endgame_da.typ"),
            Self::ShiyuInfo => include_str!("../endgame_shiyu.typ"),
        }
    }
}

/// Equivalent to walking `jq '.. | strings'` and selecting remote PNG/WebP strings.
fn image_urls(value: &Value, urls: &mut BTreeSet<String>) {
    match value {
        Value::String(s)
            if (s.starts_with("https://") || s.starts_with("http://"))
                && (s.contains("webp") || s.contains("png")) =>
        {
            urls.insert(s.clone());
        }
        Value::Array(items) => items.iter().for_each(|v| image_urls(v, urls)),
        Value::Object(items) => items.values().for_each(|v| image_urls(v, urls)),
        _ => (),
    }
}

fn image_filename(url: &str) -> anyhow::Result<&str> {
    // Match the templates' split("/").last() exactly; never rewrite input URLs.
    let name = url
        .rsplit('/')
        .next()
        .context("image URL has no filename")?;
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '\0']),
        "invalid image filename"
    );
    Ok(name)
}

async fn download(
    client: &reqwest::Client,
    cache: &Path,
    url: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let path = cache.join(image_filename(url)?);
    if fs::metadata(&path)
        .await
        .is_ok_and(|m| m.is_file() && m.len() > 0)
    {
        return Ok(Some(path));
    }
    let response = client.get(url).send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // JSON includes art unused by some templates. Missing art is fatal only
        // when Typst actually reads it; never substitute an unrelated image.
        eprintln!("image not found (HTTP 404): {url}");
        return Ok(None);
    }
    let bytes = response.error_for_status()?.bytes().await?;
    ensure!(!bytes.is_empty(), "empty image response from {url}");
    // Publish only complete downloads, including when multiple renders share the cache.
    let temporary = tempfile::NamedTempFile::new_in(cache)?;
    fs::write(temporary.path(), &bytes).await?;
    temporary.persist(&path).map_err(|e| e.error)?;
    Ok(Some(path))
}

async fn stage_images(
    input: &Value,
    season: Option<&Value>,
    directory: &Path,
) -> anyhow::Result<()> {
    let mut urls = BTreeSet::new();
    image_urls(input, &mut urls);
    if let Some(season) = season {
        image_urls(season, &mut urls);
    }
    let cache =
        PathBuf::from(std::env::var_os("IMAGE_CACHE_DIR").unwrap_or_else(|| "images".into()));
    fs::create_dir_all(&cache).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut jobs = tokio::task::JoinSet::new();
    for url in urls {
        let (client, cache, directory) = (client.clone(), cache.clone(), directory.to_owned());
        jobs.spawn(async move {
            let path = download(&client, &cache, &url)
                .await
                .with_context(|| format!("downloading {url}"))?;
            if let Some(path) = path {
                fs::copy(&path, directory.join(image_filename(&url)?)).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        if jobs.len() >= 8 {
            jobs.join_next()
                .await
                .context("missing download task")???;
        }
    }
    while let Some(result) = jobs.join_next().await {
        result??;
    }
    Ok(())
}

async fn pipeline(
    directory: &Path,
    typst: &std::ffi::OsStr,
    resvg: &std::ffi::OsStr,
    sizing: [&str; 2],
) -> anyhow::Result<Vec<u8>> {
    let mut compiler = Command::new(typst)
        .current_dir(directory)
        .args([
            "compile",
            "--root",
            ".",
            "--font-path",
            ".",
            "--format",
            "svg",
            "template.typ",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting typst CLI")?;
    let pipe: Stdio = compiler
        .stdout
        .take()
        .context("missing typst stdout")?
        .try_into()?;
    let rasterizer = Command::new(resvg)
        .current_dir(directory)
        .args(sizing)
        .args(["-", "output.png"])
        .stdin(pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting resvg CLI")?;
    let (compiled, rendered) = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::try_join!(compiler.wait_with_output(), rasterizer.wait_with_output())
    })
    .await
    .context("image rendering exceeded 120 seconds")??;
    ensure!(
        compiled.status.success() && rendered.status.success(),
        "image rendering failed: typst {}: {}; resvg {}: {}",
        compiled.status,
        String::from_utf8_lossy(&compiled.stderr),
        rendered.status,
        String::from_utf8_lossy(&rendered.stderr)
    );
    let png = fs::read(directory.join("output.png"))
        .await
        .context("reading resvg PNG")?;
    ensure!(
        png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "resvg output is not a PNG"
    );
    Ok(png)
}

/// Download all referenced images before either renderer is started.
pub async fn render(
    template: Template,
    input: &Value,
    season: Option<&Value>,
) -> anyhow::Result<Vec<u8>> {
    ensure!(
        !matches!(template, Template::Shiyu) || season.is_some(),
        "individual Shiyu requires season JSON"
    );
    let with_images = !matches!(template, Template::BetaAgent);
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::create_dir(root.join("images")).await?;
    fs::write(root.join("template.typ"), template.source()).await?;
    fs::write(root.join("prelude.typ"), include_bytes!("../prelude.typ")).await?;
    fs::write(root.join("inpin.ttf"), include_bytes!("../inpin.ttf")).await?;
    let data = match template {
        Template::BetaAgent => serde_json::to_vec(&beta::template_data(input)?)?,
        _ => serde_json::to_vec(input)?,
    };
    fs::write(root.join("input.json"), data).await?;
    if let Some(season) = season {
        fs::write(root.join("season.json"), serde_json::to_vec(season)?).await?;
    }
    if with_images {
        stage_images(input, season, &root.join("images")).await?;
        for (name, bytes) in [
            ("da.webp", include_bytes!("../image/da.webp").as_slice()),
            (
                "shiyu.webp",
                include_bytes!("../image/shiyu.webp").as_slice(),
            ),
            (
                "star-icon.png",
                include_bytes!("../image/star-icon.png").as_slice(),
            ),
        ] {
            fs::write(root.join("images").join(name), bytes).await?;
        }
    }
    let typst = std::env::var_os("TYPST_BIN").unwrap_or_else(|| "typst".into());
    let resvg = std::env::var_os("RESVG_BIN").unwrap_or_else(|| "resvg".into());
    let sizing = match template {
        // The 20 cm season pages are already wider than 2560 px at this zoom.
        Template::DeadlyInfo | Template::ShiyuInfo => ["-z", "4"],
        _ => ["--width", HIRES_WIDTH],
    };
    pipeline(root, &typst, &resvg, sizing).await
}

#[derive(Serialize, Deserialize)]
pub struct TopShiyu {
    pub top: Vec<TopShiyuItem>,
}
#[derive(Serialize, Deserialize)]
pub struct TopShiyuItem {
    pub nickname: String,
    pub rating: String,
    pub score: u32,
}
#[derive(Serialize, Deserialize)]
pub struct TopDA {
    pub top: Vec<TopDAItem>,
}
#[derive(Serialize, Deserialize)]
pub struct TopDAItem {
    pub nickname: String,
    pub stars: u8,
    pub hard_stars: u8,
    pub total_score: u32,
    pub normal_score: u32,
    pub hard_score: u32,
}

pub async fn top_da(data: &TopDA) -> anyhow::Result<Vec<u8>> {
    render(Template::TopDa, &serde_json::to_value(data)?, None).await
}
pub async fn top_shiyu(data: &TopShiyu) -> anyhow::Result<Vec<u8>> {
    render(Template::TopShiyu, &serde_json::to_value(data)?, None).await
}
pub async fn da(data: &ZZZDeadlyAssault) -> anyhow::Result<Vec<u8>> {
    render(Template::Da, &serde_json::to_value(data)?, None).await
}
pub async fn shiyu(
    data: &ZZZShiyuDefense,
    season: &SeasonDetail,
    nickname: &str,
) -> anyhow::Result<Vec<u8>> {
    ensure!(
        data.zone_id == Some(i64::try_from(season.id)?),
        "Shiyu player and season IDs differ"
    );
    let mut input = serde_json::to_value(data)?;
    input["nick_name"] = nickname.into();
    render(
        Template::Shiyu,
        &input,
        Some(&serde_json::to_value(season)?),
    )
    .await
}
pub async fn deadly_info(
    data: &BossSeasonDetail,
    begin: &str,
    end: &str,
) -> anyhow::Result<Vec<u8>> {
    ensure!(!data.modes.is_empty(), "Deadly Assault season has no modes");
    let mut input = serde_json::to_value(data)?;
    input["begin_time"] = begin.into();
    input["end_time"] = end.into();
    input["modes"]
        .as_array_mut()
        .context("missing modes")?
        .sort_by_key(|mode| {
            let id = mode["id"].as_u64().unwrap_or(0);
            (id != data.id, id)
        });
    render(Template::DeadlyInfo, &input, None).await
}
pub async fn shiyu_info(data: &SeasonDetail) -> anyhow::Result<Vec<u8>> {
    let stage = data
        .zone
        .values()
        .find(|zone| zone.stage_num == 5 && !zone.child.is_empty())
        .context("Shiyu season has no Critical Node stage 5")?;
    let mut input = serde_json::to_value(data)?;
    input["zone"]
        .as_object_mut()
        .context("missing zones")?
        .retain(|id, _| id.parse::<u64>().is_ok_and(|id| stage.child.contains(&id)));
    render(Template::ShiyuInfo, &input, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recursively_finds_images_including_hard_mode_and_deduplicates() {
        let input = json!({"list": [{"icon": "https://example.org/a.webp"}],
            "hard_list": [{"nested": ["https://example.org/b.png", "https://example.org/a.webp"]}],
            "text": "png", "local": "images/local.png", "other": "https://example.org/file.json"});
        let mut urls = BTreeSet::new();
        image_urls(&input, &mut urls);
        assert_eq!(
            urls.into_iter().collect::<Vec<_>>(),
            ["https://example.org/a.webp", "https://example.org/b.png"]
        );
    }

    #[tokio::test]
    async fn all_six_templates_render_with_real_clis() {
        let avatar = json!({"role_square_url": "images/star-icon.png", "rank": 1, "rarity": "S"});
        let room = json!({"boss": [{"name": "Boss"}], "score": 12345, "avatar_list": [avatar.clone(), avatar.clone(), avatar],
            "buffer": [{"title": "Buff", "description": "A useful buff."}]});
        let monster = json!({"name": "Boss", "element": {"ice": 1, "fire": -1},
            "stats": {"hp": 1234567, "attack": 1000, "defence": 900}});
        let zone = json!({"name": "Room", "layer_room": {"62053051": {"monster_list": {"1": monster}}},
            "layer_buff": {"1": {"desc": "A mechanic."}},
            "selectable_buff": {"1": {"title": "Buff", "desc": "A useful buff."}}});
        let season = json!({"name": "Critical Node", "id": 62053, "begin_time": "2026-07-24 04:00:00", "end_time": "2026-08-07 03:59:59", "zone": {"62053051": zone.clone()}});
        let fixtures = [
            (
                Template::TopDa,
                json!({"top": [{"nickname": "Player", "total_score": 100000, "stars": 9, "hard_stars": 2, "hard_score": 10000, "normal_score": 90000}]}),
            ),
            (
                Template::TopShiyu,
                json!({"top": [{"nickname": "Player", "score": 50000, "rating": "S+"}]}),
            ),
            (
                Template::Da,
                json!({"nick_name": "Player", "total_score": 12345, "rank_percent": 100, "has_hard": true, "hard_rank_percent": 200, "list": [room.clone()], "hard_list": [room]}),
            ),
            (
                Template::Shiyu,
                json!({"nick_name": "Игрок #1", "brief": {"score": 50000, "rating": "S+", "rank_percent": 100}, "fitfh_layer_detail": {"layer_challenge_info_list": [{"layer_id": 62053051, "score": 50000, "avatar_list": []}]}}),
            ),
            (
                Template::DeadlyInfo,
                json!({"name": "Deadly Assault", "id": 69041, "begin_time": "2026-07-24 04:00:00", "end_time": "2026-08-07 03:59:59", "modes": [{"zone_type": 1001, "zone": {"1": zone.clone()}}, {"zone_type": 1002, "zone": {"2": zone}}]}),
            ),
            (Template::ShiyuInfo, season.clone()),
        ];
        for (template, input) in fixtures {
            let png = render(template, &input, Some(&season))
                .await
                .unwrap_or_else(|e| panic!("{template:?}: {e:#}"));
            assert!(png.len() > 1000);
            if let Some(directory) = std::env::var_os("RENDER_TEST_OUTPUT") {
                fs::create_dir_all(&directory).await.unwrap();
                fs::write(Path::new(&directory).join(format!("{template:?}.png")), png)
                    .await
                    .unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipeline_reports_both_failures_and_drains_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let compiler = dir.path().join("fake typst");
        let rasterizer = dir.path().join("fake resvg");
        fs::write(&compiler, "#!/bin/sh\ni=0; while [ $i -lt 10000 ]; do echo compiler-diagnostic >&2; i=$((i+1)); done\nexit 7\n").await.unwrap();
        fs::write(
            &rasterizer,
            "#!/bin/sh\ncat >/dev/null\necho rasterizer-diagnostic >&2\nexit 8\n",
        )
        .await
        .unwrap();
        for path in [&compiler, &rasterizer] {
            fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .await
                .unwrap();
        }
        let error = pipeline(
            dir.path(),
            compiler.as_os_str(),
            rasterizer.as_os_str(),
            ["-z", "4"],
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("compiler-diagnostic") && error.contains("rasterizer-diagnostic"));
    }
}
