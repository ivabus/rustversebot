//! Temporary MiniJinja/resvg compatibility backend.

use std::sync::{Arc, LazyLock};

use anyhow::Context;
use nanoka::types::{BossSeasonDetail, SeasonDetail};
use regex::Regex;
use resvg::tiny_skia;
use resvg::usvg;
use resvg::usvg::{ImageHrefResolver, ImageKind};
use rustverse::models::zzz::{ZZZDeadlyAssault, ZZZShiyuDefense};
use serde::Serialize;

use crate::model::{
    TopDA, TopShiyu, prepare_deadly_info, prepare_deadly_info_with_begin_time, prepare_shiyu_info,
    wrap_game_text_lines,
};
use crate::renderer::RenderScale;

fn image_dir() -> String {
    std::env::var("IMAGE_CACHE_DIR").unwrap_or("image".into())
}

fn image_cache_path(href: &str) -> anyhow::Result<String> {
    let image_dir = image_dir();
    std::fs::create_dir_all(&image_dir)
        .with_context(|| format!("creating image cache directory {image_dir}"))?;
    let filename = href
        .rsplit('/')
        .next()
        .filter(|filename| !filename.is_empty())
        .context("image URL has no filename")?;
    Ok(format!("{image_dir}/{filename}"))
}

fn cached_image(href: &str) -> anyhow::Result<Arc<Vec<u8>>> {
    let cache_path = image_cache_path(href)?;
    Ok(Arc::new(std::fs::read(&cache_path).with_context(|| {
        format!("reading cached image {cache_path}")
    })?))
}

fn href_resolver(href: &str, _options: &usvg::Options) -> Option<ImageKind> {
    match href {
        "rustverse-bundled-da.webp" => {
            return Some(ImageKind::WEBP(Arc::new(
                include_bytes!("../../image/da.webp").to_vec(),
            )));
        }
        "rustverse-bundled-shiyu.webp" => {
            return Some(ImageKind::WEBP(Arc::new(
                include_bytes!("../../image/shiyu.webp").to_vec(),
            )));
        }
        "rustverse-bundled-hollows.png" => {
            return Some(ImageKind::PNG(Arc::new(
                include_bytes!("../../image/hollows.png").to_vec(),
            )));
        }
        _ => {}
    }

    let data = cached_image(href).ok()?;
    match href.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageKind::PNG(data)),
        "jpg" | "jpeg" => Some(ImageKind::JPEG(data)),
        "webp" => Some(ImageKind::WEBP(data)),
        "gif" => Some(ImageKind::GIF(data)),
        _ => None,
    }
}

async fn preload_info_images<'a>(hrefs: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
    let mut hrefs = hrefs
        .into_iter()
        .filter(|href| href.starts_with("https://") || href.starts_with("http://"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    hrefs.sort_unstable();
    hrefs.dedup();

    let client = reqwest::Client::new();
    let mut downloads = tokio::task::JoinSet::new();
    for href in hrefs {
        let cache_path = image_cache_path(&href)?;
        if tokio::fs::try_exists(&cache_path).await? {
            continue;
        }
        let client = client.clone();
        downloads.spawn(async move {
            let data = client
                .get(&href)
                .send()
                .await
                .with_context(|| format!("downloading image {href}"))?
                .error_for_status()
                .with_context(|| format!("downloading image {href}"))?
                .bytes()
                .await
                .with_context(|| format!("reading downloaded image {href}"))?;
            tokio::fs::write(&cache_path, &data)
                .await
                .with_context(|| format!("writing cached image {cache_path}"))
        });
    }

    let mut first_error = None;
    while let Some(result) = downloads.join_next().await {
        let result = result
            .context("image preloader task panicked")
            .and_then(|result| result);
        if let Err(error) = result
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

/// Download the boss art needed by a Deadly Assault card into the on-disk
/// image cache. The renderer will still read and decode it on demand.
pub async fn preload_deadly_info_images(data: &BossSeasonDetail) -> anyhow::Result<()> {
    let view = prepare_deadly_info(data)?;
    preload_info_images(view.rooms.iter().map(|room| room.monster.image.as_str())).await
}

/// Download the boss art needed by a Shiyu Defense card into the on-disk
/// image cache. The renderer will still read and decode it on demand.
pub async fn preload_shiyu_info_images(data: &SeasonDetail) -> anyhow::Result<()> {
    let view = prepare_shiyu_info(data)?;
    preload_info_images(
        view.rooms
            .iter()
            .map(|room| room.main_monster.image.as_str()),
    )
    .await
}

/// Download every remote image referenced by a player Deadly Assault card.
pub async fn preload_da_images(data: &ZZZDeadlyAssault) -> anyhow::Result<()> {
    let mut hrefs = Vec::new();
    for room in &data.list {
        if let Some(boss) = room.boss.first() {
            hrefs.push(boss.icon.as_str());
        }
        hrefs.extend(
            room.avatar_list
                .iter()
                .map(|avatar| avatar.role_square_url.as_str()),
        );
        if let Some(icon) = room
            .buffer
            .first()
            .and_then(|buffer| buffer.icon.as_deref())
        {
            hrefs.push(icon);
        }
    }
    preload_info_images(hrefs).await
}

/// Download every remote image that can be referenced by a player Shiyu card.
pub async fn preload_shiyu_images(data: &ZZZShiyuDefense) -> anyhow::Result<()> {
    let mut hrefs = Vec::new();
    for layer in data.layers.values() {
        for room in &layer.layer_challenge_info_list {
            if let Some(monster_pic) = room.monster_pic.as_deref() {
                hrefs.push(monster_pic);
            }
            hrefs.extend(
                room.avatar_list
                    .iter()
                    .map(|avatar| avatar.role_square_url.as_str()),
            );
        }
    }
    preload_info_images(hrefs).await
}

pub static USVG_OPTIONS: LazyLock<usvg::Options<'_>> = LazyLock::new(|| {
    let mut options = usvg::Options {
        text_rendering: usvg::TextRendering::OptimizeLegibility,
        shape_rendering: usvg::ShapeRendering::GeometricPrecision,
        image_rendering: usvg::ImageRendering::HighQuality,
        image_href_resolver: ImageHrefResolver {
            resolve_data: ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(href_resolver),
        },
        ..usvg::Options::default()
    };
    options
        .fontdb_mut()
        .load_font_data(include_bytes!("../../inpin.ttf").to_vec());
    options
});

pub static MJ_ENVIRONMENT: LazyLock<minijinja::Environment> = LazyLock::new(|| {
    let mut environment = minijinja::Environment::new();
    environment
        .add_template("defs.j2", include_str!("../../defs.j2"))
        .unwrap();
    environment
        .add_template("top_shiyu.j2", include_str!("../../top_shiyu.j2"))
        .unwrap();
    environment
        .add_template("top_da.j2", include_str!("../../top_da.j2"))
        .unwrap();
    environment
        .add_template("da.j2", include_str!("../../da.j2"))
        .unwrap();
    environment
        .add_template("shiyu.j2", include_str!("../../shiyu.j2"))
        .unwrap();
    environment
        .add_template("deadly_info.j2", include_str!("../../deadly_info.j2"))
        .unwrap();
    environment
        .add_template("shiyu_info.j2", include_str!("../../shiyu_info.j2"))
        .unwrap();
    environment.add_filter("game_text", format_game_text);
    environment.add_filter("wrap_game_text", wrap_game_text);
    environment.add_filter("strip_all_tags", strip_all_tags_filter);
    environment.add_filter("element_filter", element_filter);
    environment
});

static REMOTE_IMAGE_HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bhref="(https?://[^"]+)""#).expect("remote image href regex must compile")
});

pub fn render_from_serialize<T: Serialize>(template: &str, data: &T) -> Vec<u8> {
    try_render_from_serialize(template, data).expect("SVG rendering failed")
}

/// Render an arbitrary SVG template for the crate's legacy command-line tool.
#[doc(hidden)]
pub fn render_template_source<T: Serialize>(template_source: &str, data: &T) -> Vec<u8> {
    let mut environment = MJ_ENVIRONMENT.clone();
    environment.add_filter("split", split_filter);
    let template = environment.template_from_str(template_source).unwrap();
    let rendered = template.render(data).unwrap();

    std::fs::write("rendered.svg", &rendered).unwrap();
    rasterize_svg(&rendered, RenderScale::DEFAULT).expect("SVG rendering failed")
}

pub fn try_render_from_serialize<T: Serialize>(
    template: &str,
    data: &T,
) -> anyhow::Result<Vec<u8>> {
    try_render_from_serialize_with_scale(template, data, RenderScale::DEFAULT)
}

/// Render a compatibility SVG template at an explicit output scale.
pub fn try_render_from_serialize_with_scale<T: Serialize>(
    template: &str,
    data: &T,
    scale: RenderScale,
) -> anyhow::Result<Vec<u8>> {
    let template = MJ_ENVIRONMENT.get_template(template)?;
    let rendered = template.render(data)?;

    for captures in REMOTE_IMAGE_HREF.captures_iter(&rendered) {
        let href = captures
            .get(1)
            .context("remote image href capture is missing")?
            .as_str();
        let cache_path = image_cache_path(href)?;
        anyhow::ensure!(
            std::fs::exists(&cache_path)?,
            "remote image was not preloaded: {href}"
        );
    }

    rasterize_svg(&rendered, scale)
}

pub(crate) fn rasterize_svg(rendered: &str, scale: RenderScale) -> anyhow::Result<Vec<u8>> {
    let tree = usvg::Tree::from_data(rendered.as_bytes(), &USVG_OPTIONS)?;
    let pixmap_size = tree
        .size()
        .to_int_size()
        .scale_by(scale.factor())
        .ok_or_else(|| anyhow::anyhow!("rendered SVG size is invalid"))?;
    let mut pixmap = tiny_skia::Pixmap::new(pixmap_size.width(), pixmap_size.height())
        .ok_or_else(|| anyhow::anyhow!("could not allocate SVG render target"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale.factor(), scale.factor()),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

pub fn top_shiyu(data: &TopShiyu) -> Vec<u8> {
    render_from_serialize("top_shiyu.j2", data)
}

pub fn top_da(data: &TopDA) -> Vec<u8> {
    render_from_serialize("top_da.j2", data)
}

pub fn da(data: &ZZZDeadlyAssault) -> Vec<u8> {
    render_from_serialize("da.j2", data)
}

pub fn shiyu(data: &ZZZShiyuDefense) -> Vec<u8> {
    render_from_serialize("shiyu.j2", data)
}

pub fn deadly_info(data: &BossSeasonDetail) -> anyhow::Result<Vec<u8>> {
    let view = prepare_deadly_info(data)?;
    try_render_from_serialize("deadly_info.j2", &view)
}

pub fn deadly_info_with_begin_time(
    data: &BossSeasonDetail,
    begin_time: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    let view = prepare_deadly_info_with_begin_time(data, begin_time)?;
    try_render_from_serialize("deadly_info.j2", &view)
}

pub fn shiyu_info(data: &SeasonDetail) -> anyhow::Result<Vec<u8>> {
    let view = prepare_shiyu_info(data)?;
    try_render_from_serialize("shiyu_info.j2", &view)
}

fn element_filter(_state: &minijinja::State, value: minijinja::Value, target: i64) -> Vec<String> {
    let Ok(value) = serde_json::to_value(value) else {
        return Vec::new();
    };
    let Some(elements) = value.as_object() else {
        return Vec::new();
    };

    let mut result = elements
        .iter()
        .filter(|(_, value)| value.as_i64() == Some(target))
        .map(|(name, _)| {
            let mut chars = name.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>();
    result.sort();
    result
}

fn wrap_game_text(_state: &minijinja::State, text: String, max_width: usize) -> Vec<String> {
    wrap_game_text_lines(&text, max_width)
}

fn split_filter(_state: &minijinja::State, value: String, delimiter: String) -> Vec<String> {
    value.split(&delimiter).map(str::to_owned).collect()
}

fn format_game_text(value: String) -> String {
    value
        .replace("</color>", "</tspan>")
        .replace("<color=", r#"<tspan fill=""#)
        .replace('>', r#"">"#)
        .replace(r#"</tspan">"#, "</tspan>")
}

fn strip_all_tags_filter(value: String) -> String {
    static TERM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<Term[^>]*>").expect("valid term regex"));
    static ICON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<IconMap[^>]*>").expect("valid icon regex"));

    ICON.replace_all(TERM.replace_all(&value, "").as_ref(), "")
        .replace("</Term>", "")
}
