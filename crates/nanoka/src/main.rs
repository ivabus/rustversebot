//! A CLI client for Zenless Zone Zero data from nanoka.cc.

use clap::{Parser, Subcommand, ValueEnum};
use nanoka::{NanokaClient, types::EndgameType};
use std::collections::HashMap;

/// Query Shiyu Defense and Deadly Assault data from nanoka.cc.
#[derive(Parser)]
#[command(name = "nanoka", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Language for displayed names: `en`, `ko`, `zh`, or `ja`.
    #[arg(short, long, default_value = "en", global = true)]
    lang: String,
}

#[derive(Subcommand)]
enum Commands {
    /// List all seasons (Shiyu Defense or Deadly Assault).
    List {
        /// Filter by endgame type.
        #[arg(short, long)]
        r#type: Option<EndgameTypeArg>,

        /// Show only the most recent N seasons.
        #[arg(short, long)]
        recent: Option<usize>,

        /// Show season begin/end dates.
        #[arg(short, long)]
        dates: bool,
    },

    /// Show detailed info for a specific season.
    Show {
        /// Numeric season ID. For example, use `62053` or `69041`.
        id: u64,

        /// Print monster image URLs.
        #[arg(short, long)]
        images: bool,

        /// Output raw JSON (all fields, unfiltered).
        #[arg(long)]
        json: bool,

        /// Output JSON with image paths resolved to full URLs.
        #[arg(long, conflicts_with = "json")]
        json_resolved: bool,

        /// Show final Deadly Assault stats from `boss_adjust`.
        /// Set an optional player level from 1 through 29.
        /// The command uses the highest available level by default.
        #[arg(long, value_name = "LEVEL")]
        scaled: Option<Option<usize>>,
    },

    /// Display the currently resolved game data version.
    Version,
}

/// CLI-friendly endgame type argument.
#[derive(Clone, Copy, ValueEnum)]
enum EndgameTypeArg {
    /// Shiyu Defense, including Critical Node and Stable Node.
    Shiyu,
    /// Deadly Assault Trial boss rush.
    Deadly,
}

impl From<EndgameTypeArg> for EndgameType {
    fn from(arg: EndgameTypeArg) -> Self {
        match arg {
            EndgameTypeArg::Shiyu => EndgameType::ShiyuDefence,
            EndgameTypeArg::Deadly => EndgameType::DeadlyAssault,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let client = NanokaClient::new().with_lang(&cli.lang);

    match cli.command {
        Commands::List {
            r#type,
            recent,
            dates,
        } => cmd_list(&client, r#type, recent, dates).await?,
        Commands::Show {
            id,
            images,
            json,
            json_resolved,
            scaled,
        } => cmd_show(&client, id, images, json, json_resolved, scaled).await?,
        Commands::Version => {
            let v = client.version().await?;
            println!("{v}");
        }
    }

    Ok(())
}

async fn cmd_list(
    client: &NanokaClient,
    filter_type: Option<EndgameTypeArg>,
    recent: Option<usize>,
    dates: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let seasons = match filter_type.map(EndgameType::from) {
        Some(ty) => client.get_seasons_by_type(ty).await?,
        None => {
            // Fetch both and merge, prefixing with type marker
            let mut all = client.get_seasons().await?;
            let boss = client.get_boss_seasons().await?;
            all.extend(boss);
            all
        }
    };

    // Collect into a sorted vec (by numeric key descending so newest first)
    let mut entries: Vec<_> = seasons.iter().collect();
    entries.sort_by_key(|(k, _)| k.parse::<u64>().unwrap_or(0));
    entries.reverse();

    let limit = recent.unwrap_or(entries.len());

    for (id, meta) in entries.iter().take(limit) {
        let name = meta_local_name(meta, &client.lang);
        let type_label = meta
            .endgame_type()
            .map(|t| t.to_string())
            .unwrap_or_else(|| "?".into());
        if dates {
            let begin = meta
                .begin
                .as_deref()
                .or(meta.live_begin.as_deref())
                .unwrap_or("?");
            let end = meta
                .end
                .as_deref()
                .or(meta.live_end.as_deref())
                .unwrap_or("?");
            println!(
                "{id:>8}  {name:<24}  [{type_label:<15}]  sort={}  {begin} → {end}",
                meta.sort
            );
        } else {
            println!("{id:>8}  {name:<24}  [{type_label}]",);
        }
    }

    Ok(())
}

async fn cmd_show(
    client: &NanokaClient,
    id: u64,
    print_images: bool,
    raw_json: bool,
    json_resolved: bool,
    scaled: Option<Option<usize>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use nanoka::types::AnySeasonDetail;

    let mut detail = client.get_detail(id).await?;

    if raw_json || json_resolved {
        if json_resolved {
            client.resolve_images(&mut detail);
        }
        let j = serde_json::to_string_pretty(&detail)?;
        println!("{j}");
        return Ok(());
    }

    let type_label = detail.endgame_type().to_string();

    match &detail {
        AnySeasonDetail::Shiyu(d) => {
            print_season_header(
                d.id,
                &d.name,
                type_label,
                d.begin_time.as_deref(),
                d.end_time.as_deref(),
            );
            print_zones(client, &d.zone, print_images);
        }
        AnySeasonDetail::Boss(d) => {
            print_season_header(d.id, &d.name, type_label, None, None);
            if let Some(zone_map) = d.zones() {
                print_zones(client, zone_map, print_images);
            } else {
                println!("(no zones)");
            }

            // If --scaled flag is present, show computed final stats
            if let Some(level_opt) = scaled {
                print_scaled_stats(d, level_opt);
            }
        }
    }

    Ok(())
}

fn print_season_header(
    id: u64,
    name: &str,
    type_label: String,
    begin_time: Option<&str>,
    end_time: Option<&str>,
) {
    println!("=== [{type_label}] Season {id}: {name} ===");
    if let Some(bt) = begin_time {
        println!("Begin: {bt}");
    }
    if let Some(et) = end_time {
        println!("End:   {et}");
    }
    println!();
}

fn print_zones(
    client: &NanokaClient,
    zones: &HashMap<String, nanoka::types::Zone>,
    print_images: bool,
) {
    // Sort zones by key
    let mut zone_ids: Vec<&String> = zones.keys().collect();
    zone_ids.sort_by_key(|k| k.parse::<u64>().unwrap_or(0));

    for zone_id in zone_ids {
        let zone = &zones[zone_id];

        let stage_label = if zone.stage_num > 0 && !zone.name.is_empty() {
            format!("Stage {}: {}", zone.stage_num, zone.name)
        } else if !zone.name.is_empty() {
            zone.name.clone()
        } else {
            format!("Zone {}", zone_id)
        };

        println!(
            "┌─ {stage_label} (lvl {}, {} sub-zones)",
            zone.monster_level,
            zone.child.len()
        );

        // Goals
        if zone.ss_rank_goal > 0 {
            print!("│  Goals: SS ≥ {}, ", zone.ss_rank_goal);
        } else {
            print!("│  Goals: ");
        }
        if zone.goal_type == 2 {
            // Score-based (Deadly Assault)
            println!(
                "S ≥ {}, A ≥ {}, B ≥ {}",
                zone.s_rank_goal, zone.a_rank_goal, zone.b_rank_goal
            );
        } else if zone.goal_type == 3 {
            // Combined / parent node
            println!(
                "S={}, A={}, B={}",
                zone.s_rank_goal, zone.a_rank_goal, zone.b_rank_goal
            );
        } else {
            // Timer-based (standard Shiyu): goals are in seconds
            let fmt_sec = |v: u64| {
                if v == 0 {
                    "none".into()
                } else {
                    format!("{}:{:02}", v / 60, v % 60)
                }
            };
            println!(
                "S ≤ {}, A ≤ {}, B ≤ {}",
                fmt_sec(zone.s_rank_goal),
                fmt_sec(zone.a_rank_goal),
                fmt_sec(zone.b_rank_goal),
            );
        }

        // Buffs (layer_buff + selectable_buff)
        for (buff_id, buff) in &zone.layer_buff {
            if buff.title.is_empty() && buff.desc.is_empty() {
                continue;
            }
            let title = if buff.title.is_empty() {
                "(no name)"
            } else {
                &buff.title
            };
            let desc = strip_color_tags(&buff.desc);
            println!("│  Buff [{buff_id}] {title}: {desc}");
        }
        if !zone.selectable_buff.is_empty() {
            println!("│  Selectable buffs:");
            for (buff_id, buff) in &zone.selectable_buff {
                let title = if buff.title.is_empty() {
                    "(no name)"
                } else {
                    &buff.title
                };
                let desc = strip_color_tags(&buff.desc);
                println!("│    [{buff_id}] {title}: {desc}");
            }
        }

        // Rooms
        let mut room_ids: Vec<&String> = zone.layer_room.keys().collect();
        room_ids.sort_by_key(|k| k.parse::<u64>().unwrap_or(0));

        for room_id in room_ids {
            let room = &zone.layer_room[room_id];
            println!("│  ├─ Room {room_id} ({}-wave)", room.waves_num);

            // Weaknesses
            if !room.monster_weakness.is_empty() {
                let weaknesses: Vec<_> = room.monster_weakness.values().collect();
                println!(
                    "│  │  Weakness: {}",
                    weaknesses
                        .iter()
                        .map(|w| w.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            // Monsters
            let mut monster_ids: Vec<&String> = room.monster_list.keys().collect();
            monster_ids.sort();

            for (i, monster_id) in monster_ids.iter().enumerate() {
                let monster = &room.monster_list[*monster_id];
                let connector = if i == monster_ids.len() - 1 {
                    "└─"
                } else {
                    "├─"
                };

                let resist_str = format_resistances(&monster.element);
                println!(
                    "│  │  {connector} {name} (ID {id})",
                    name = monster.name,
                    id = monster.id,
                );
                println!(
                    "│  │     HP: {hp:.0}  ATK: {atk:.0}  DEF: {def:.0}  Stun: {stun:.0}",
                    hp = monster.stats.hp,
                    atk = monster.stats.attack,
                    def = monster.stats.defence,
                    stun = monster.stats.stun,
                );
                if !resist_str.is_empty() {
                    println!("│  │     Element: {resist_str}");
                }

                if print_images {
                    if let Some(url) = client.monster_image_url(&monster.image) {
                        println!("│  │     Image: {url}");
                    }
                    if !room.monster_icon.is_empty()
                        && let Some(url) = client.monster_image_url(&room.monster_icon)
                    {
                        println!("│  │     Boss icon: {url}");
                    }
                }
            }
        }

        // Child zones
        if !zone.child.is_empty() {
            println!("│  Child zones: {:?}", zone.child);
        }

        println!();
    }
}

/// Print computed final stats using boss_adjust scaling.
fn print_scaled_stats(detail: &nanoka::types::BossSeasonDetail, level_opt: Option<usize>) {
    let rates = detail.level_rates(0);
    if rates.is_empty() {
        println!("(no boss_adjust data available for scaling)");
        return;
    }

    let max_level = rates.len();
    let level = level_opt.unwrap_or(max_level);
    let level = level.clamp(1, max_level);

    println!("\n=== Scaled Boss Stats (level {level}/{max_level}) ===");
    println!("Level rates: {} entries loaded", rates.len());

    // Show rates summary at level
    let rate = &rates[level - 1];
    println!(
        "Rates at level {level}: hp_rate={}, atk_rate={}, points={}",
        rate.hp_rate, rate.atk_rate, rate.points
    );

    // Show per-zone monster stats
    if let Some(zones) = detail.zones() {
        let mut zone_ids: Vec<&String> = zones.keys().collect();
        zone_ids.sort_by_key(|k| k.parse::<u64>().unwrap_or(0));

        for zone_id in zone_ids {
            let zone = &zones[zone_id];
            for room in zone.layer_room.values() {
                for monster in room.monster_list.values() {
                    let api_hp = monster.stats.hp;
                    let api_atk = monster.stats.attack;

                    if let Some(scaled) = detail.scale_stats(api_hp, api_atk, level, 0) {
                        println!(
                            "  {name} (ID {id}):\n    API:  HP={api_hp:.0}  ATK={api_atk:.0}\n    Lv.{level}: HP={hp:.0}  ATK={atk:.0}  Points={pts}",
                            name = monster.name,
                            id = monster.id,
                            hp = scaled.hp,
                            atk = scaled.atk,
                            pts = scaled.points,
                        );
                    }

                    // Also show range
                    if let Some(range) = detail.scale_stats_range(api_hp, api_atk, 0) {
                        println!(
                            "    Range: HP {hp_min:.0} → {hp_max:.0}  ATK {atk_r:.0}  Points {pts_min} → {pts_max}",
                            hp_min = range.hp_min,
                            hp_max = range.hp_max,
                            atk_r = range.atk,
                            pts_min = range.points_min,
                            pts_max = range.points_max,
                        );
                    }
                }
            }
        }
    }
}

/// Pick the localized name or fall back to English.
fn meta_local_name<'a>(meta: &'a nanoka::types::SeasonMeta, lang: &str) -> &'a str {
    match lang {
        "ko" if !meta.ko.is_empty() => &meta.ko,
        "zh" if !meta.zh.is_empty() => &meta.zh,
        "ja" if !meta.ja.is_empty() => &meta.ja,
        _ => &meta.en,
    }
}

/// Format element weaknesses from the monster `element` field (`1` = weak).
///
/// Use `monster_weakness` from the room when it is available.
/// Otherwise, summarize the per-element flags.
/// A value of `-1` means resistant and does not appear in this summary.
fn format_resistances(elem: &nanoka::types::ElementResist) -> String {
    let map = [
        ("Phys", elem.physical),
        ("Fire", elem.fire),
        ("Ice", elem.ice),
        ("Elec", elem.electric),
        ("Wind", elem.wind),
        ("Ether", elem.ether),
    ];
    let parts: Vec<String> = map
        .iter()
        .filter(|(_, v)| *v == 1)
        .map(|(n, _)| n.to_string())
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(", ")
    }
}

/// Strip `<color=...>` / `</color>` HTML tags from buff descriptions.
fn strip_color_tags(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"</?color[^>]*>").expect("color tag regex"));
    re.replace_all(s, "").to_string()
}
