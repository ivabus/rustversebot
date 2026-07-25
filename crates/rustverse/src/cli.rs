use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rustverse::client::agent_cache;
use rustverse::models::zzz::ZZZAvatar;

/// Map ZZZ element type to display name.
fn element_name(t: i64) -> &'static str {
    match t {
        200 => "Physical",
        201 => "Fire",
        202 => "Ice",
        203 => "Electric",
        204 => "Ether",
        205 => "Wind",
        300 => "Lumen",
        _ => "?",
    }
}

/// Map ZZZ profession/class to display name.
fn profession_name(t: i64) -> &'static str {
    match t {
        1 => "Attack",
        2 => "Stun",
        3 => "Anomaly",
        4 => "Support",
        5 => "Defense",
        _ => "?",
    }
}

/// Format an agent for display: "name (rarity★, lvN)" or "name (rarity★, lvN, CN)".
/// Rank is shown for S-rank if ≠0, for A-rank if ≠6.
fn fmt_agent(av: &ZZZAvatar) -> String {
    let name = agent_cache::resolve_name(av.id);
    let show_rank = match av.rarity.as_str() {
        "S" => av.rank != 0,
        "A" => av.rank != 6,
        _ => true,
    };
    if show_rank {
        format!("{} ({}★, lv{}, M{})", name, av.rarity, av.level, av.rank)
    } else {
        format!("{} ({}★, lv{})", name, av.rarity, av.level)
    }
}

#[derive(Parser)]
#[command(name = "rustverse", about = "HoYoverse API CLI client (ZZZ PoC)")]
pub struct Cli {
    /// Cookie header string (not required for `login` command)
    #[arg(short = 'c', long, env = "HOYO_COOKIE")]
    pub cookie: Option<String>,

    /// Output raw JSON response
    #[arg(short = 'j', long, global = true)]
    pub json: bool,

    /// Resolve agent IDs to names (fetches avatar list for Shiyu/Deadly Assault)
    #[arg(short = 'n', long, global = true)]
    pub resolve_names: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List games linked to the HoYoLAB account
    Games,

    /// ZZZ Daily Note (battery status)
    DailyNote {
        /// UID of the ZZZ player
        #[arg(short, long)]
        uid: String,
        /// Server region code (auto-detected from UID if omitted)
        #[arg(short, long)]
        server: Option<String>,
    },

    /// ZZZ Shiyu Defense
    Shiyu {
        #[arg(short, long)]
        uid: String,
        /// Server region code (auto-detected from UID if omitted)
        #[arg(short, long)]
        server: Option<String>,
        /// Schedule type: 1 = current, 2 = previous
        #[arg(long, default_value = "1")]
        schedule: String,
    },

    /// ZZZ Deadly Assault
    DeadlyAssault {
        #[arg(short, long)]
        uid: String,
        /// Server region code (auto-detected from UID if omitted)
        #[arg(short, long)]
        server: Option<String>,
        /// Schedule type: 1 = current, 2 = previous
        #[arg(long, default_value = "1")]
        schedule: String,
    },

    /// ZZZ Gacha (banner) calendar
    Gacha {
        #[arg(short, long)]
        uid: String,
        /// Server region code (auto-detected from UID if omitted)
        #[arg(short, long)]
        server: Option<String>,
    },

    /// ZZZ Game Record Index (profile summary with stats)
    Index {
        #[arg(short, long)]
        uid: String,
        /// Server region code (auto-detected from UID if omitted)
        #[arg(short, long)]
        server: Option<String>,
    },

    /// ZZZ Agent (avatar) list
    Avatars {
        #[arg(short, long)]
        uid: String,
        /// Server region code (auto-detected from UID if omitted)
        #[arg(short, long)]
        server: Option<String>,
    },

    /// Save cookies to file and verify them
    Login {
        /// HoYoLAB cookie string: "ltoken_v2=...; ltuid_v2=...; ltmid_v2=..."
        #[arg(short = 'c', long, env = "HOYO_COOKIE")]
        cookie: Option<String>,
        /// Open browser for manual login
        #[arg(short, long)]
        browser: bool,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        // Handle login command separately (no cookies needed yet)
        if let Command::Login { cookie, browser } = &self.command {
            let cookie_str = cookie.clone().or(self.cookie);
            return cmd_login(cookie_str, *browser).await;
        }

        let cookie_str = self
            .cookie
            .or_else(load_saved_cookie)
            .context("No cookies provided. Use --cookie, HOYO_COOKIE env, or `login` first.")?;

        let client = rustverse::client::zzz::ZZZClient::from_cookie_string(&cookie_str)
            .context("Failed to parse cookie string")?;

        match self.command {
            Command::Login { .. } => unreachable!(),
            Command::Games => {
                let cards = client.get_game_record_cards().await?;
                for card in &cards {
                    let game = card.game_name.as_deref().unwrap_or(match card.game_id {
                        2 => "Genshin Impact",
                        6 => "Honkai: Star Rail",
                        8 => "Zenless Zone Zero",
                        _ => "Unknown",
                    });
                    let public_label = if card.is_public.unwrap_or(false) {
                        "public"
                    } else {
                        "private"
                    };
                    let region_label = card.region_name.as_deref().unwrap_or(&card.region);
                    println!(
                        "[{game}] {nickname} | UID: {uid} | Lvl: {level} | {region_label} | {public_label}",
                        nickname = card.nickname,
                        uid = card.game_role_id,
                        level = card.level,
                    );
                }
                if cards.is_empty() {
                    println!("No games linked to this account.");
                }
            }

            Command::DailyNote { uid, server } => {
                let note = client.get_daily_note(&uid, server.as_deref()).await?;
                println!(
                    "Battery: {current}/{max} (full in {restore}s)",
                    current = note.energy.progress.current,
                    max = note.energy.progress.max,
                    restore = note.energy.restore,
                );
            }

            Command::Shiyu {
                uid,
                server,
                schedule,
            } => {
                // Resolve names: try target player's avatars, fall back to own
                if self.resolve_names {
                    match client.get_avatar_list(&uid, server.as_deref()).await {
                        Ok(list) => {
                            if list.avatar_list.is_empty() {
                                eprintln!(
                                    "Note: target player's avatar list is empty, names may be IDs"
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "Note: cannot fetch target avatars ({e}), trying own account..."
                            );
                            // Fall back to own avatars if not cached yet
                            if !agent_cache::is_cached() {
                                let cards = client.get_game_record_cards().await?;
                                if let Some(own) = cards.iter().find(|c| c.game_id == 8) {
                                    let _ = client.get_avatar_list(&own.game_role_id, None).await;
                                }
                            }
                        }
                    }
                }
                let data = client
                    .get_shiyu_defense(&uid, server.as_deref(), &schedule)
                    .await?;
                if self.json {
                    println!("{}", serde_json::to_string_pretty(&data)?);
                } else {
                    println!("Shiyu Defense:");
                    if let Some(ref brief) = data.brief {
                        let rp = brief.rank_percent.unwrap_or(0) as f64 / 100.0;
                        println!(
                            "  Score: {} / {}  Rating: {}  Top: {rp:.2}%",
                            brief.score.unwrap_or(0),
                            brief.max_score.unwrap_or(0),
                            brief.rating.as_deref().unwrap_or("?"),
                        );
                    }
                    for (layer_name, layer) in &data.layers {
                        let label = layer_name
                            .strip_suffix("_layer_detail")
                            .unwrap_or(layer_name);
                        let layer_buff = layer.buffer.as_ref().and_then(|b| b.title.as_deref());
                        println!(
                            "  [{}] Rating: {}",
                            label,
                            layer.rating.as_deref().unwrap_or("?"),
                        );
                        if let Some(b) = layer_buff {
                            println!("    Buff: {b}");
                        }
                        for ch in &layer.layer_challenge_info_list {
                            let avatars: Vec<String> =
                                ch.avatar_list.iter().map(fmt_agent).collect();
                            let buff = ch.buffer.as_ref().and_then(|b| b.title.as_deref());
                            println!(
                                "    Node {}: {} / {}  Rating: {}  Time: {}",
                                ch.layer_id.unwrap_or(0),
                                ch.score.unwrap_or(0),
                                ch.max_score.unwrap_or(0),
                                ch.rating.as_deref().unwrap_or("?"),
                                ch.challenge_time
                                    .as_ref()
                                    .map(|t| t.to_string())
                                    .unwrap_or_else(|| "?".into()),
                            );
                            println!("      Agents: {}", avatars.join(", "));
                            if let Some(b) = buff {
                                println!("      Buff: {b}");
                            }
                        }
                    }
                    if let Some(end) = &data.hadal_end_time {
                        println!(
                            "  Ends: {year}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} (UTC+8)",
                            year = end.year,
                            month = end.month,
                            day = end.day,
                            hour = end.hour,
                            minute = end.minute,
                            second = end.second,
                        );
                    }
                    println!("\nUse --json for full response data.");
                }
            }

            Command::DeadlyAssault {
                uid,
                server,
                schedule,
            } => {
                // Resolve names: try target player's avatars, fall back to own
                if self.resolve_names {
                    match client.get_avatar_list(&uid, server.as_deref()).await {
                        Ok(list) => {
                            if list.avatar_list.is_empty() {
                                eprintln!(
                                    "Note: target player's avatar list is empty, names may be IDs"
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "Note: cannot fetch target avatars ({e}), trying own account..."
                            );
                            if !agent_cache::is_cached() {
                                let cards = client.get_game_record_cards().await?;
                                if let Some(own) = cards.iter().find(|c| c.game_id == 8) {
                                    let _ = client.get_avatar_list(&own.game_role_id, None).await;
                                }
                            }
                        }
                    }
                }
                let data = client
                    .get_deadly_assault(&uid, server.as_deref(), &schedule)
                    .await?;
                if self.json {
                    println!("{}", serde_json::to_string_pretty(&data)?);
                } else {
                    println!("Deadly Assault:");
                    println!(
                        "  {}  |  Total: {} ★  |  Score: {}",
                        data.nick_name.as_deref().unwrap_or("?"),
                        data.total_star.unwrap_or(0),
                        data.total_score.unwrap_or(0),
                    );
                    for (i, room) in data.list.iter().enumerate() {
                        let boss = room.boss.first();
                        let buffs: Vec<&str> = room
                            .buffer
                            .iter()
                            .filter_map(|b| b.title.as_deref())
                            .collect();
                        println!(
                            "  Room {} — {}  |  {} ★  |  Score: {}",
                            i + 1,
                            boss.map(|b| b.name.as_str()).unwrap_or("?"),
                            room.star.unwrap_or(0),
                            room.score.unwrap_or(0),
                        );
                        if !buffs.is_empty() {
                            println!("    Buffs: {}", buffs.join(", "));
                        }
                        for av in &room.avatar_list {
                            println!("      {}", fmt_agent(av));
                        }
                    }
                    if let Some(rp) = data.rank_percent {
                        println!("  Top: {:.2}%", rp / 100.0);
                    }
                    println!("\nUse --json for full response data.");
                }
            }

            Command::Gacha { uid, server } => {
                let cal = client.get_gacha_calendar(&uid, server.as_deref()).await?;
                if self.json {
                    println!("{}", serde_json::to_string_pretty(&cal)?);
                    return Ok(());
                }
                println!("=== Character Banners ===");
                for banner in &cal.avatar_gacha_schedule_list {
                    println!(
                        "  [{}] Character Banner | Version: {} | {start} → {end}",
                        banner.gacha_state.as_deref().unwrap_or("?"),
                        banner.version.as_deref().unwrap_or("?"),
                        start = banner
                            .start_ts
                            .map(|t| chrono::DateTime::from_timestamp(t, 0)
                                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| t.to_string()))
                            .unwrap_or_else(|| "?".into()),
                        end = banner
                            .end_ts
                            .map(|t| chrono::DateTime::from_timestamp(t, 0)
                                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| t.to_string()))
                            .unwrap_or_else(|| "?".into()),
                    );
                    for ch in &banner.avatar_list {
                        let element = element_name(ch.avatar_element_type.unwrap_or(0));
                        let profession = profession_name(ch.avatar_profession.unwrap_or(0));
                        println!(
                            "    {}★ {} ({element}) [{profession}]",
                            ch.rarity.as_deref().unwrap_or("?"),
                            ch.avatar_name.as_deref().unwrap_or("?"),
                        );
                    }
                }

                println!("=== W-Engine Banners ===");
                for banner in &cal.weapon_gacha_schedule_list {
                    println!(
                        "  [{}] W-Engine Banner | Version: {} | {start} → {end}",
                        banner.gacha_state.as_deref().unwrap_or("?"),
                        banner.version.as_deref().unwrap_or("?"),
                        start = banner
                            .start_ts
                            .map(|t| chrono::DateTime::from_timestamp(t, 0)
                                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| t.to_string()))
                            .unwrap_or_else(|| "?".into()),
                        end = banner
                            .end_ts
                            .map(|t| chrono::DateTime::from_timestamp(t, 0)
                                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| t.to_string()))
                            .unwrap_or_else(|| "?".into()),
                    );
                    for w in &banner.weapon_list {
                        println!(
                            "    {}★ {}",
                            w.rarity.as_deref().unwrap_or("?"),
                            w.talent_title.as_deref().unwrap_or("(unknown)"),
                        );
                    }
                }
            }

            Command::Index { uid, server } => {
                // When -n: fetch full avatar list (for complete agent roster)
                // and Deadly Assault (for nickname). Fall back to own account if needed.
                let mut full_avatars: Option<Vec<rustverse::models::zzz::ZZZAvatarInfo>> = None;
                let mut nickname: Option<String> = None;

                // 0. Always try to get nickname from game record card (lightweight).
                //    This only works for the authenticated user's own games.
                if let Ok(cards) = client.get_game_record_cards().await
                    && let Some(card) = cards.iter().find(|c| c.game_role_id == uid)
                {
                    nickname = Some(card.nickname.clone());
                }

                if self.resolve_names {
                    // 1. Try to get the full agent list (more complete than index's avatar_list)
                    match client.get_avatar_list(&uid, server.as_deref()).await {
                        Ok(list) => {
                            if list.avatar_list.is_empty() {
                                eprintln!("Note: target player's avatar list is empty");
                            } else {
                                full_avatars = Some(list.avatar_list);
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "Note: cannot fetch target avatars ({e}), trying own account..."
                            );
                            if !agent_cache::is_cached() {
                                let cards = client.get_game_record_cards().await?;
                                if let Some(own) = cards.iter().find(|c| c.game_id == 8)
                                    && let Ok(list) =
                                        client.get_avatar_list(&own.game_role_id, None).await
                                    && !list.avatar_list.is_empty()
                                {
                                    full_avatars = Some(list.avatar_list);
                                }
                            }
                        }
                    }

                    // 2. Try Deadly Assault as stronger fallback for nickname (works for third-party UIDs)
                    if nickname.is_none() {
                        match client
                            .get_deadly_assault(&uid, server.as_deref(), "1")
                            .await
                        {
                            Ok(da) => {
                                nickname = da.nick_name;
                            }
                            Err(e) => eprintln!("Note: cannot fetch deadly assault ({e})"),
                        }
                    }
                }

                let data = client.get_index(&uid, server.as_deref()).await?;
                if self.json {
                    println!("{}", serde_json::to_string_pretty(&data)?);
                } else {
                    let nick = nickname.as_deref().unwrap_or("?");
                    println!("=== {nick} (UID: {uid}) ===");

                    if let Some(ref stats) = data.stats {
                        println!();
                        if let Some(ref wl) = stats.world_level_name {
                            println!("  Interknot Level:     {wl}");
                        }
                        println!("  Active Days:         {}", stats.active_days.unwrap_or(0));
                        println!("  Agents:              {}", stats.avatar_num.unwrap_or(0));
                        println!("  Bangboo:             {}", stats.buddy_num.unwrap_or(0));
                        println!(
                            "  Achievements:        {}",
                            stats.achievement_count.unwrap_or(0)
                        );
                        println!(
                            "  Shiyu Full S:        {}",
                            stats.challenge_full_s_times.unwrap_or(0)
                        );
                        println!(
                            "  DA Full Stars:       {}",
                            stats.memory_battlefield_full_stars_times.unwrap_or(0)
                        );

                        // Shiyu summary
                        if let Some(ref hadal) = stats.hadal_brief
                            && let Some(ref v2) = hadal.v2
                        {
                            let rp = v2.rank_percent.unwrap_or(0) as f64 / 100.0;
                            println!(
                                "  Shiyu Defense:       {} / {}  Rating: {}  Top: {rp:.2}%",
                                v2.score.unwrap_or(0),
                                v2.max_score.unwrap_or(0),
                                v2.rating.as_deref().unwrap_or("?"),
                            );
                        }

                        // Deadly Assault summary
                        if let Some(ref mb) = stats.memory_battlefield {
                            let rp = mb.rank_percent.unwrap_or(0) as f64 / 100.0;
                            println!(
                                "  Deadly Assault:      {} ★  Score: {}  Top: {rp:.2}%",
                                mb.total_star.unwrap_or(0),
                                mb.total_score.unwrap_or(0),
                            );
                        }

                        // Tower
                        if let Some(t) = stats.climbing_tower_layer {
                            println!("  Tower (S1):          Floor {t}");
                        }
                    }

                    // Use full avatar list when -n was given, otherwise fall back to index's list
                    let display_avatars = full_avatars.as_ref().unwrap_or(&data.avatar_list);
                    if !display_avatars.is_empty() {
                        let s_agents: Vec<_> =
                            display_avatars.iter().filter(|a| a.rarity == "S").collect();
                        let a_agents: Vec<_> =
                            display_avatars.iter().filter(|a| a.rarity == "A").collect();
                        println!();
                        println!("=== S-Rank Agents ({}) ===", s_agents.len());
                        for av in &s_agents {
                            println!("  Lv{:<3} C{:<2} | {}", av.level, av.rank, av.name_mi18n,);
                        }
                        println!("=== A-Rank Agents ({}) ===", a_agents.len());
                        for av in &a_agents {
                            println!("  Lv{:<3} C{:<2} | {}", av.level, av.rank, av.name_mi18n,);
                        }
                    }
                    println!("\nUse --json for full response data.");
                }
            }

            Command::Avatars { uid, server } => {
                let data = client.get_avatar_list(&uid, server.as_deref()).await?;
                if self.json {
                    println!("{}", serde_json::to_string_pretty(&data)?);
                } else {
                    // Group by rarity
                    let s_agents: Vec<_> = data
                        .avatar_list
                        .iter()
                        .filter(|a| a.rarity == "S")
                        .collect();
                    let a_agents: Vec<_> = data
                        .avatar_list
                        .iter()
                        .filter(|a| a.rarity == "A")
                        .collect();

                    println!("=== S-Rank ({}) ===", s_agents.len());
                    for av in &s_agents {
                        println!("  Lv{:<3} C{:<2} | {}", av.level, av.rank, av.name_mi18n,);
                    }
                    println!("=== A-Rank ({}) ===", a_agents.len());
                    for av in &a_agents {
                        println!("  Lv{:<3} C{:<2} | {}", av.level, av.rank, av.name_mi18n,);
                    }
                    println!("\nUse --json for full response data.");
                }
            }
        }

        Ok(())
    }
}

// ── Cookie file & login helpers ──

fn cookie_path() -> std::path::PathBuf {
    let base = dirs_next().unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("cookies.txt")
}

fn dirs_next() -> Option<std::path::PathBuf> {
    std::env::var("RUSTVERSE_CONFIG")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("rustverse")))
}

fn load_saved_cookie() -> Option<String> {
    let path = cookie_path();
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn save_cookie(cookie: &str) -> Result<()> {
    let path = cookie_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, cookie)?;
    eprintln!("Cookies saved to {}", path.display());
    Ok(())
}

async fn cmd_login(cookie: Option<String>, open_browser: bool) -> Result<()> {
    if open_browser {
        eprintln!("Opening https://act.hoyolab.com ...");
        eprintln!(
            "Log in, then copy cookies from DevTools → Application → Cookies → act.hoyolab.com"
        );
        eprintln!("You need: ltoken_v2, ltuid_v2, ltmid_v2");
        let _ = open::that("https://act.hoyolab.com");
        eprintln!("\nThen run: rustverse login -c 'ltoken_v2=...; ltuid_v2=...; ltmid_v2=...'");
        return Ok(());
    }

    let cookie_str = cookie
        .context("No cookie provided. Use -c 'ltoken_v2=...; ltuid_v2=...; ltmid_v2=...' or -b to open browser.")?;

    eprint!("Verifying cookies... ");
    match rustverse::client::zzz::ZZZClient::from_cookie_string(&cookie_str) {
        Ok(client) => match client.get_game_record_cards().await {
            Ok(cards) => {
                eprintln!("OK ({} game(s) linked)", cards.len());
                for c in &cards {
                    let game = match c.game_id {
                        2 => "Genshin",
                        6 => "HSR",
                        8 => "ZZZ",
                        _ => "?",
                    };
                    eprintln!(
                        "  [{game}] {nick} — UID {uid}",
                        nick = c.nickname,
                        uid = c.game_role_id
                    );
                }
            }
            Err(e) => {
                eprintln!("FAILED: {e}");
                anyhow::bail!("Cookies are invalid or expired. Please re-login.");
            }
        },
        Err(e) => {
            eprintln!("FAILED: {e}");
            anyhow::bail!("Could not parse cookies.");
        }
    }

    save_cookie(&cookie_str)?;
    eprintln!("\nCookies saved. Run: rustverse games");
    Ok(())
}
