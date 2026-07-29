use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::Context;
use nanoka::types::{BossSeasonDetail, Buff, ElementResist, Monster, SeasonDetail, Zone};
use regex::Regex;
use resvg::tiny_skia;
use resvg::usvg;
use resvg::usvg::ImageHrefResolver;
use resvg::usvg::ImageKind;
use rustverse::models::zzz::ZZZDeadlyAssault;
use rustverse::models::zzz::ZZZShiyuDefense;
use serde::Deserialize;
use serde::Serialize;

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
                include_bytes!("../image/da.webp").to_vec(),
            )));
        }
        "rustverse-bundled-shiyu.webp" => {
            return Some(ImageKind::WEBP(Arc::new(
                include_bytes!("../image/shiyu.webp").to_vec(),
            )));
        }
        "rustverse-bundled-hollows.png" => {
            return Some(ImageKind::PNG(Arc::new(
                include_bytes!("../image/hollows.png").to_vec(),
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
        // NO ImageKind::SVG
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
    let mut opt = usvg::Options {
        text_rendering: usvg::TextRendering::OptimizeLegibility,
        shape_rendering: usvg::ShapeRendering::GeometricPrecision,
        image_rendering: usvg::ImageRendering::HighQuality,
        image_href_resolver: ImageHrefResolver {
            resolve_data: ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(href_resolver),
        },
        ..usvg::Options::default()
    };
    opt.fontdb_mut()
        .load_font_data(include_bytes!("../inpin.ttf").to_vec());
    opt
});

pub static MJ_ENVIRONMENT: LazyLock<minijinja::Environment> = LazyLock::new(|| {
    let mut env = minijinja::Environment::new();
    env.add_template("defs.j2", include_str!("../defs.j2"))
        .unwrap();
    env.add_template("top_shiyu.j2", include_str!("../top_shiyu.j2"))
        .unwrap();
    env.add_template("top_da.j2", include_str!("../top_da.j2"))
        .unwrap();
    env.add_template("da.j2", include_str!("../da.j2")).unwrap();
    env.add_template("shiyu.j2", include_str!("../shiyu.j2"))
        .unwrap();
    env.add_template("deadly_info.j2", include_str!("../deadly_info.j2"))
        .unwrap();
    env.add_template("shiyu_info.j2", include_str!("../shiyu_info.j2"))
        .unwrap();
    env.add_filter("game_text", format_game_text);
    env.add_filter("wrap_game_text", wrap_game_text);
    env.add_filter("strip_all_tags", strip_all_tags_filter);
    env.add_filter("element_filter", element_filter);
    env
});

static REMOTE_IMAGE_HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\bhref="(https?://[^"]+)""#).expect("remote image href regex must compile")
});

pub const ZOOM_FACTOR: f32 = 5.0;

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

pub fn render_from_serialize<T: Serialize>(template: &str, data: &T) -> Vec<u8> {
    try_render_from_serialize(template, data).expect("SVG rendering failed")
}

pub fn try_render_from_serialize<T: Serialize>(
    template: &str,
    data: &T,
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

    let tree = usvg::Tree::from_data(rendered.as_bytes(), &USVG_OPTIONS)?;
    let pixmap_size = tree
        .size()
        .to_int_size()
        .scale_by(ZOOM_FACTOR)
        .ok_or_else(|| anyhow::anyhow!("rendered SVG size is invalid"))?;
    let mut pixmap = tiny_skia::Pixmap::new(pixmap_size.width(), pixmap_size.height())
        .ok_or_else(|| anyhow::anyhow!("could not allocate SVG render target"))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(ZOOM_FACTOR, ZOOM_FACTOR),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}
pub fn top_shiyu(data: &TopShiyu) -> Vec<u8> {
    render_from_serialize("top_shiyu.j2", data)
}

#[derive(Serialize, Deserialize)]
pub struct TopDA {
    pub top: Vec<TopDAItem>,
}

#[derive(Serialize, Deserialize)]
pub struct TopDAItem {
    pub nickname: String,
    pub stars: u8,
    pub total_score: u32,
    pub normal_score: u32,
    pub hard_score: u32,
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EndgameInfoBuff {
    pub id: String,
    pub title: String,
    pub desc: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EndgameInfoMonster {
    pub id: u64,
    pub name: String,
    pub image: String,
    pub hp: f64,
    pub attack: f64,
    pub defence: f64,
    pub weaknesses: Vec<String>,
    pub resistances: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeadlyInfoRoom {
    pub id: String,
    pub is_complex: bool,
    pub name: String,
    pub monster: EndgameInfoMonster,
    pub mechanics: Vec<String>,
    pub layout_y: u32,
    pub layout_height: u32,
    pub mechanics_y: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeadlyInfoView {
    pub id: u64,
    pub begin_date: Option<String>,
    pub buffs: Vec<EndgameInfoBuff>,
    pub canvas_height: u32,
    pub buff_cards_height: u32,
    pub rooms_start: u32,
    pub rooms: Vec<DeadlyInfoRoom>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShiyuInfoRoom {
    pub id: String,
    pub name: String,
    pub waves_num: u32,
    pub weaknesses: Vec<String>,
    pub buffs: Vec<EndgameInfoBuff>,
    pub mechanics: Vec<String>,
    pub main_monster: EndgameInfoMonster,
    pub other_monsters: Vec<EndgameInfoMonster>,
    pub layout_y: u32,
    pub layout_height: u32,
    pub mechanics_y: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ShiyuInfoView {
    pub id: u64,
    pub name: String,
    pub stage_num: u32,
    pub begin_time: Option<String>,
    pub begin_date: Option<String>,
    pub end_time: Option<String>,
    pub s_rank_goal: u64,
    pub a_rank_goal: u64,
    pub b_rank_goal: u64,
    pub score_cap: Option<u64>,
    pub mechanics_wrap_width: usize,
    pub title_top_gap: u32,
    pub canvas_height: u32,
    pub buff_cards_height: u32,
    pub rooms_start: u32,
    pub rooms: Vec<ShiyuInfoRoom>,
}

const SHIYU_MECHANICS_WRAP_WIDTH: usize = 90;
const SHIYU_TITLE_TOP_GAP: u32 = 4;
const DEADLY_BUFF_WRAP_WIDTH: usize = 38;
const DEADLY_MECHANICS_Y: u32 = 59;
const DEADLY_MECHANICS_WITHOUT_ELEMENTS_Y: u32 = 43;

fn season_date(value: Option<&str>) -> Option<String> {
    let date = value?.split_whitespace().next()?;
    (date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .chars()
            .enumerate()
            .all(|(index, character)| matches!(index, 4 | 7) || character.is_ascii_digit()))
    .then(|| date.to_owned())
}

fn info_buff(id: &str, buff: &Buff) -> EndgameInfoBuff {
    EndgameInfoBuff {
        id: id.to_owned(),
        title: buff.title.clone(),
        desc: buff.desc.clone(),
    }
}

fn element_names(elements: &ElementResist, target: i32) -> Vec<String> {
    [
        ("Physical", elements.physical),
        ("Fire", elements.fire),
        ("Ice", elements.ice),
        ("Electric", elements.electric),
        ("Ether", elements.ether),
        ("Wind", elements.wind),
    ]
    .into_iter()
    .filter(|(_, value)| *value == target)
    .map(|(name, _)| name.to_owned())
    .collect()
}

fn info_monster(monster: &Monster) -> EndgameInfoMonster {
    EndgameInfoMonster {
        id: monster.id,
        name: monster.name.clone(),
        image: monster.image.clone(),
        hp: monster.stats.hp,
        attack: monster.stats.attack,
        defence: monster.stats.defence,
        weaknesses: element_names(&monster.element, 1),
        resistances: element_names(&monster.element, -1),
    }
}

fn sorted_monsters(zone: &Zone) -> Vec<&Monster> {
    let mut monsters = zone.layer_room.iter().collect::<Vec<_>>();
    monsters.sort_by_key(|(key, _)| *key);

    let mut monsters = monsters
        .into_iter()
        .flat_map(|(_, room)| room.monster_list.iter())
        .collect::<Vec<_>>();
    monsters.sort_by(|(left_key, left), (right_key, right)| {
        right
            .stats
            .hp
            .total_cmp(&left.stats.hp)
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left_key.cmp(right_key))
    });
    monsters.into_iter().map(|(_, monster)| monster).collect()
}

fn normalise_deadly_name(zone_name: &str, monster_name: &str) -> String {
    match zone_name {
        "异变能量体" => "Phaethon of the Scorched Horizon".to_owned(),
        "" => monster_name.to_owned(),
        value => value.to_owned(),
    }
}

pub fn prepare_deadly_info_with_begin_time(
    data: &BossSeasonDetail,
    begin_time: Option<&str>,
) -> anyhow::Result<DeadlyInfoView> {
    anyhow::ensure!(!data.modes.is_empty(), "Deadly Assault season has no modes");

    // The primary mode has the season ID.  Beta seasons may prepend a second
    // mode for the complex boss; render it as an additional room after the
    // regular three rooms rather than silently dropping either mode.
    let mut zones = data
        .modes
        .iter()
        .flat_map(|mode| {
            mode.zone
                .iter()
                .map(move |(zone_id, zone)| (mode.id != data.id, mode.id, zone_id, zone))
        })
        .collect::<Vec<_>>();
    zones.sort_by(
        |(left_is_extra, left_mode, left_key, left),
         (right_is_extra, right_mode, right_key, right)| {
            left_is_extra
                .cmp(right_is_extra)
                .then_with(|| left.stage_num.cmp(&right.stage_num))
                .then_with(|| left_mode.cmp(right_mode))
                .then_with(|| left_key.cmp(right_key))
        },
    );

    let mut buffs = BTreeMap::new();
    for (_, _, _, zone) in &zones {
        for (id, buff) in &zone.selectable_buff {
            if !buff.title.is_empty() || !buff.desc.is_empty() {
                buffs
                    .entry(id.clone())
                    .or_insert_with(|| info_buff(id, buff));
            }
        }
    }

    let mut rooms = Vec::new();
    for (is_complex, _, zone_id, zone) in zones {
        let monster = sorted_monsters(zone)
            .into_iter()
            .next()
            .with_context(|| format!("Deadly Assault zone {zone_id} has no monsters"))?;

        let mut mechanics = zone.layer_buff.iter().collect::<Vec<_>>();
        mechanics.sort_by_key(|(key, _)| *key);
        let mechanics = mechanics
            .into_iter()
            .filter(|(_, buff)| !buff.desc.is_empty())
            .map(|(_, buff)| buff.desc.clone())
            .collect();

        let monster = info_monster(monster);
        let mechanics_y = if monster.weaknesses.is_empty() && monster.resistances.is_empty() {
            DEADLY_MECHANICS_WITHOUT_ELEMENTS_Y
        } else {
            DEADLY_MECHANICS_Y
        };

        rooms.push(DeadlyInfoRoom {
            id: zone_id.clone(),
            is_complex,
            name: normalise_deadly_name(&zone.name, &monster.name),
            monster,
            mechanics,
            layout_y: 0,
            layout_height: 0,
            mechanics_y,
        });
    }

    anyhow::ensure!(!rooms.is_empty(), "Deadly Assault season has no zones");
    let buffs = buffs.into_values().collect::<Vec<_>>();
    let buff_cards_height = buffs
        .iter()
        .map(|buff| 36 + wrap_game_text_lines(&buff.desc, DEADLY_BUFF_WRAP_WIDTH).len() as u32 * 9)
        .max()
        .unwrap_or(90)
        .max(110);
    let rooms_start = 100 + buff_cards_height + 10;
    let mut next_y = 0;
    for room in &mut rooms {
        let mechanic_lines = room
            .mechanics
            .iter()
            .map(|mechanic| wrap_game_text_lines(mechanic, 82).len() as u32)
            .sum::<u32>();
        room.layout_height = (room.mechanics_y + 10 + mechanic_lines * 10).max(112);
        room.layout_y = next_y;
        next_y += room.layout_height + 8;
    }
    let canvas_height = rooms_start + next_y + 6;

    Ok(DeadlyInfoView {
        id: data.id,
        begin_date: season_date(begin_time),
        buffs,
        canvas_height,
        buff_cards_height,
        rooms_start,
        rooms,
    })
}

pub fn prepare_deadly_info(data: &BossSeasonDetail) -> anyhow::Result<DeadlyInfoView> {
    prepare_deadly_info_with_begin_time(data, None)
}

fn parse_score_cap(zones: &[&Zone]) -> Option<u64> {
    static SCORE_CAP_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)score cap[^0-9]*([0-9][0-9, ]*)").expect("valid score cap regex")
    });

    zones
        .iter()
        .flat_map(|zone| zone.layer_buff.values())
        .filter_map(|buff| SCORE_CAP_RE.captures(&buff.desc))
        .filter_map(|captures| captures.get(1))
        .filter_map(|value| {
            let digits = value
                .as_str()
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>();
            digits.parse().ok()
        })
        .max()
}

pub fn prepare_shiyu_info(data: &SeasonDetail) -> anyhow::Result<ShiyuInfoView> {
    let mut parents = data
        .zone
        .iter()
        .filter(|(_, zone)| zone.stage_num == 5 && !zone.child.is_empty())
        .collect::<Vec<_>>();
    parents.sort_by_key(|(key, _)| *key);
    let (parent_id, parent) = parents
        .into_iter()
        .next()
        .context("Shiyu season has no stage 5 parent zone")?;

    let mut child_ids = parent.child.clone();
    child_ids.sort_unstable();
    let mut child_zones = Vec::with_capacity(child_ids.len());
    for child_id in child_ids {
        let key = child_id.to_string();
        let zone = data.zone.get(&key).with_context(|| {
            format!("Shiyu stage 5 parent {parent_id} references missing child {key}")
        })?;
        child_zones.push((key, zone));
    }
    anyhow::ensure!(
        !child_zones.is_empty(),
        "Shiyu stage 5 parent has no child zones"
    );

    let score_cap = parse_score_cap(
        &child_zones
            .iter()
            .map(|(_, zone)| *zone)
            .collect::<Vec<_>>(),
    );
    let rank_zone = child_zones[0].1;
    let mut rooms = Vec::with_capacity(child_zones.len());

    for (index, (zone_id, zone)) in child_zones.into_iter().enumerate() {
        let mut layer_rooms = zone.layer_room.iter().collect::<Vec<_>>();
        layer_rooms.sort_by_key(|(key, _)| *key);
        let (_, layer_room) = layer_rooms
            .into_iter()
            .next()
            .with_context(|| format!("Shiyu child zone {zone_id} has no combat room"))?;

        let monsters = sorted_monsters(zone);
        let (main_monster, other_monsters) = monsters
            .split_first()
            .with_context(|| format!("Shiyu child zone {zone_id} has no monsters"))?;

        let mut weaknesses = layer_room.monster_weakness.iter().collect::<Vec<_>>();
        weaknesses.sort_by(|(left, _), (right, _)| {
            left.parse::<u32>()
                .unwrap_or_default()
                .cmp(&right.parse::<u32>().unwrap_or_default())
                .then_with(|| left.cmp(right))
        });

        let mut layer_buffs = zone.layer_buff.iter().collect::<Vec<_>>();
        layer_buffs.sort_by_key(|(key, _)| *key);
        let buffs = layer_buffs
            .iter()
            .filter(|(_, buff)| !buff.title.is_empty() && !buff.desc.is_empty())
            .map(|(id, buff)| info_buff(id, buff))
            .collect();
        let mechanics = layer_buffs
            .iter()
            .filter(|(_, buff)| !buff.desc.is_empty())
            .map(|(_, buff)| buff.desc.clone())
            .collect();

        rooms.push(ShiyuInfoRoom {
            id: zone_id,
            name: if zone.name.is_empty() {
                format!("Room {}", index + 1)
            } else {
                zone.name.clone()
            },
            waves_num: layer_room.waves_num,
            weaknesses: weaknesses
                .into_iter()
                .map(|(_, name)| name.clone())
                .collect(),
            buffs,
            mechanics,
            main_monster: info_monster(main_monster),
            other_monsters: other_monsters
                .iter()
                .map(|monster| info_monster(monster))
                .collect(),
            layout_y: 0,
            layout_height: 0,
            mechanics_y: 0,
        });
    }

    // Shiyu buffs are rendered in full inside each room. A separate row of
    // title-only cards duplicated that information and made the image taller.
    let buff_cards_height = 0;
    let rooms_start = 118 + SHIYU_TITLE_TOP_GAP;
    let mut next_y = 0;
    for room in &mut rooms {
        room.mechanics_y = 99 + room.other_monsters.len() as u32 * 10;
        let mechanic_lines = room
            .mechanics
            .iter()
            .map(|mechanic| wrap_game_text_lines(mechanic, SHIYU_MECHANICS_WRAP_WIDTH).len() as u32)
            .sum::<u32>();
        room.layout_height = (room.mechanics_y + 24 + mechanic_lines * 8).max(134);
        room.layout_y = next_y;
        next_y += room.layout_height + 8;
    }
    let canvas_height = rooms_start + next_y + 6;

    Ok(ShiyuInfoView {
        id: data.id,
        name: data.name.clone(),
        stage_num: parent.stage_num,
        begin_time: data.begin_time.clone(),
        begin_date: season_date(data.begin_time.as_deref()),
        end_time: data.end_time.clone(),
        s_rank_goal: rank_zone.s_rank_goal,
        a_rank_goal: rank_zone.a_rank_goal,
        b_rank_goal: rank_zone.b_rank_goal,
        score_cap,
        mechanics_wrap_width: SHIYU_MECHANICS_WRAP_WIDTH,
        title_top_gap: SHIYU_TITLE_TOP_GAP,
        canvas_height,
        buff_cards_height,
        rooms_start,
        rooms,
    })
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

fn is_prohibited_start(c: char) -> bool {
    matches!(
        c,
        '.' | ','
            | '!'
            | '?'
            | ':'
            | ';'
            | ')'
            | ']'
            | '}'
            | '”'
            | '’'
            | '。'
            | '、'
            | '！'
            | '？'
            | '：'
            | '；'
            | '）'
            | '】'
            | '』'
            | '」'
    )
}

struct CharItem {
    c: char,
    is_visible: bool,
}

struct Token {
    text: String,
    visible_len: usize,
    visible_len_no_trailing_space: usize,
}

fn wrap_game_text(_state: &minijinja::State, text: String, max_width: usize) -> Vec<String> {
    wrap_game_text_lines(&text, max_width)
}

fn wrap_game_text_lines(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut items = Vec::new();
        let mut in_tag = false;
        for c in paragraph.chars() {
            if c == '<' {
                in_tag = true;
                items.push(CharItem {
                    c,
                    is_visible: false,
                });
            } else if c == '>' && in_tag {
                in_tag = false;
                items.push(CharItem {
                    c,
                    is_visible: false,
                });
            } else {
                items.push(CharItem {
                    c,
                    is_visible: !in_tag,
                });
            }
        }

        let mut tokens = Vec::new();
        let mut current_text = String::new();
        let mut current_visible_len = 0;
        let mut current_visible_len_no_space = 0;
        for (index, item) in items.iter().enumerate() {
            current_text.push(item.c);
            if item.is_visible {
                current_visible_len += 1;
                if !item.c.is_whitespace() {
                    current_visible_len_no_space = current_visible_len;
                }
            }
            if item.is_visible && item.c.is_whitespace() {
                let next = items[index + 1..].iter().find(|item| item.is_visible);
                if next.is_some_and(|next| !next.c.is_whitespace() && !is_prohibited_start(next.c))
                {
                    tokens.push(Token {
                        text: std::mem::take(&mut current_text),
                        visible_len: current_visible_len,
                        visible_len_no_trailing_space: current_visible_len_no_space,
                    });
                    current_visible_len = 0;
                    current_visible_len_no_space = 0;
                }
            }
        }
        if !current_text.is_empty() {
            tokens.push(Token {
                text: current_text,
                visible_len: current_visible_len,
                visible_len_no_trailing_space: current_visible_len_no_space,
            });
        }

        let mut current_line = String::new();
        let mut current_line_visible_len = 0;
        for token in tokens {
            if current_line_visible_len + token.visible_len_no_trailing_space > max_width
                && !current_line.is_empty()
            {
                lines.push(current_line.trim_end().to_owned());
                current_line.clear();
                current_line_visible_len = 0;
            }
            current_line.push_str(&token.text);
            current_line_visible_len += token.visible_len;
        }
        if !current_line.is_empty() {
            lines.push(current_line.trim_end().to_owned());
        }
    }
    close_and_reopen_colors_at_line_breaks(lines)
}

/// A color span may cover several words and therefore several wrapped lines.
/// SVG `<tspan>` elements cannot cross the sibling tspans used for line
/// positioning, so close it on the current line and reopen it on the next.
fn close_and_reopen_colors_at_line_breaks(lines: Vec<String>) -> Vec<String> {
    let mut active_colors = Vec::new();

    lines
        .into_iter()
        .map(|line| {
            let mut rendered: String = active_colors.concat();
            rendered.push_str(&line);

            let mut remainder = line.as_str();
            while let Some(tag_start) = remainder.find('<') {
                let Some(tag_end) = remainder[tag_start..].find('>') else {
                    break;
                };
                let tag_end = tag_start + tag_end + 1;
                let tag = &remainder[tag_start..tag_end];
                if tag.starts_with("<color=") {
                    active_colors.push(tag.to_owned());
                } else if tag == "</color>" {
                    active_colors.pop();
                }
                remainder = &remainder[tag_end..];
            }

            for _ in 0..active_colors.len() {
                rendered.push_str("</color>");
            }
            rendered
        })
        .collect()
}

fn format_game_text(value: String) -> String {
    value
        .replace("</color>", "</tspan>")
        .replace("<color=", r#"<tspan fill=""#)
        .replace('>', r#"">"#)
        .replace(r#"</tspan">"#, "</tspan>")
}

fn strip_all_tags_filter(value: String) -> String {
    let term = Regex::new(r"<Term[^>]*>").expect("valid term regex");
    let icon = Regex::new(r"<IconMap[^>]*>").expect("valid icon regex");
    icon.replace_all(term.replace_all(&value, "").as_ref(), "")
        .replace("</Term>", "")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use nanoka::types::{
        BossMode, BossSeasonDetail, Buff, ElementResist, Monster, MonsterStats, Room, SeasonDetail,
        Zone,
    };

    use super::wrap_game_text_lines;

    #[test]
    fn wrapped_colored_text_keeps_its_color_on_every_line() {
        let lines = wrap_game_text_lines("<color=#2BAD00>increases by 30% faster</color>", 12);

        assert_eq!(
            lines,
            vec![
                "<color=#2BAD00>increases by</color>",
                "<color=#2BAD00>30% faster</color>",
            ]
        );
    }

    fn monster(id: u64, name: &str, hp: f64) -> Monster {
        Monster {
            id,
            name: name.to_owned(),
            image: "image/shiyu.webp".to_owned(),
            element: ElementResist {
                ice: 0,
                fire: 1,
                electric: 0,
                ether: -1,
                physical: 0,
                wind: 0,
            },
            stats: MonsterStats {
                hp,
                attack: 1_000.0,
                defence: 500.0,
                stun: 1_000.0,
                attribute_infliction: 0.0,
            },
        }
    }

    fn buff(title: &str, desc: &str) -> Buff {
        Buff {
            title: title.to_owned(),
            desc: desc.to_owned(),
        }
    }

    fn child_zone(id: u64, room_number: usize) -> (String, Zone) {
        let room_id = id.to_string();
        let room = Room {
            monster_icon: String::new(),
            monster_list: HashMap::from([
                ("secondary".to_owned(), monster(2, "Secondary", 1_000.0)),
                ("main".to_owned(), monster(1, "Main", 2_000.0)),
            ]),
            monster_weakness: HashMap::from([
                ("201".to_owned(), "Fire".to_owned()),
                ("200".to_owned(), "Physical".to_owned()),
            ]),
            waves_num: 2,
        };
        (
            room_id.clone(),
            Zone {
                name: format!("Room {room_number}"),
                stage_num: 5,
                monster_level: 70,
                layer_buff: HashMap::from([
                    (
                        "common".to_owned(),
                        buff("", "The total score cap (including bonus) is 50,000."),
                    ),
                    (
                        format!("thematic-{room_number}"),
                        buff(&format!("Buff {room_number}"), "A useful room buff."),
                    ),
                ]),
                selectable_buff: HashMap::new(),
                child: Vec::new(),
                layer_room: HashMap::from([(room_id, room)]),
                goal_type: 2,
                ss_rank_goal: 0,
                s_rank_goal: 25_000,
                a_rank_goal: 16_000,
                b_rank_goal: 8_000,
            },
        )
    }

    fn shiyu_fixture() -> SeasonDetail {
        let children = [62053053, 62053051, 62053052];
        let mut zones = HashMap::from([(
            "6205305".to_owned(),
            Zone {
                name: String::new(),
                stage_num: 5,
                monster_level: 0,
                layer_buff: HashMap::new(),
                selectable_buff: HashMap::new(),
                child: children.to_vec(),
                layer_room: HashMap::new(),
                goal_type: 3,
                ss_rank_goal: 0,
                s_rank_goal: 0,
                a_rank_goal: 0,
                b_rank_goal: 0,
            },
        )]);
        for (index, id) in [62053051, 62053052, 62053053].into_iter().enumerate() {
            let (key, zone) = child_zone(id, index + 1);
            zones.insert(key, zone);
        }
        SeasonDetail {
            id: 62053,
            name: "Critical Node".to_owned(),
            priority: 1,
            zone: zones,
            begin_time: Some("2026-07-24 04:00:00".to_owned()),
            end_time: Some("2026-08-07 03:59:59".to_owned()),
        }
    }

    #[test]
    fn shiyu_view_is_sorted_and_selects_the_highest_hp_monster() {
        let view = super::prepare_shiyu_info(&shiyu_fixture()).unwrap();
        assert_eq!(
            view.rooms
                .iter()
                .map(|room| room.id.as_str())
                .collect::<Vec<_>>(),
            ["62053051", "62053052", "62053053"]
        );
        assert!(
            view.rooms
                .iter()
                .all(|room| room.main_monster.name == "Main")
        );
        assert_eq!(view.score_cap, Some(50_000));
        assert_eq!(view.rooms[0].weaknesses, ["Physical", "Fire"]);
        assert_eq!(view.rooms[0].main_monster.resistances, ["Ether"]);
    }

    #[test]
    fn shiyu_info_fixture_renders_as_png() {
        let png = super::shiyu_info(&shiyu_fixture()).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn renderer_rejects_remote_images_that_were_not_preloaded() {
        let data = serde_json::json!({
            "list": [{
                "boss": [{ "icon": "https://example.invalid/not-cached.webp" }],
                "avatar_list": [],
                "buffer": [{ "icon": "image/star-icon.png" }],
                "score": 0,
            }],
            "total_score": 0,
            "rank_percent": 0,
        });
        let error = super::try_render_from_serialize("da.j2", &data).unwrap_err();
        assert!(error.to_string().contains("remote image was not preloaded"));
    }

    #[test]
    fn deadly_view_includes_the_complex_boss_mode() {
        let mut regular_zones = HashMap::new();
        for (index, id) in [69043101, 69043102, 69043103].into_iter().enumerate() {
            let (key, mut zone) = child_zone(id, index + 1);
            zone.stage_num = index as u32 + 1;
            regular_zones.insert(key, zone);
        }
        let (complex_key, mut complex_zone) = child_zone(69043201, 4);
        for room in complex_zone.layer_room.values_mut() {
            for monster in room.monster_list.values_mut() {
                monster.element = ElementResist {
                    ice: 0,
                    fire: 0,
                    electric: 0,
                    ether: 0,
                    physical: 0,
                    wind: 0,
                };
            }
        }
        let detail = BossSeasonDetail {
            id: 690431,
            name: "Trial".to_owned(),
            priority: 9,
            boss_adjust: HashMap::new(),
            zone_type: 1001,
            modes: vec![
                BossMode {
                    id: 690432,
                    zone_type: 1002,
                    zone: HashMap::from([(complex_key, complex_zone)]),
                },
                BossMode {
                    id: 690431,
                    zone_type: 1001,
                    zone: regular_zones,
                },
            ],
        };

        let view = super::prepare_deadly_info(&detail).unwrap();
        assert_eq!(view.rooms.len(), 4);
        assert_eq!(view.rooms.last().unwrap().id, "69043201");
        assert!(view.rooms.last().unwrap().is_complex);
        assert!(view.rooms[..3].iter().all(|room| !room.is_complex));
        assert_eq!(view.rooms.last().unwrap().mechanics_y, 43);
    }
}
