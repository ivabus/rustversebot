use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, TimeZone, Utc};
use std::sync::Arc;
use std::time::Duration;
use teloxide::{prelude::*, types::InputFile};

use crate::BotState;
use crate::bot_templates::BotTemplateSender;
use crate::handlers;

const CHECKPOINTS: [(&str, ChronoDuration); 3] = [
    ("6h", ChronoDuration::hours(6)),
    ("24h", ChronoDuration::hours(24)),
    ("14d", ChronoDuration::days(14)),
];

#[derive(Debug, Clone)]
struct SchedulerSettings {
    tick_interval: Duration,
    checkpoint_window: Duration,
    request_spacing: Duration,
    retry_attempts: usize,
    retry_base_delay: Duration,
    retention_interval_ticks: u64,
    retention_days: i64,
    announcement_lead: ChronoDuration,
}

impl Default for SchedulerSettings {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5 * 60),
            checkpoint_window: Duration::from_secs(5 * 60),
            request_spacing: Duration::from_millis(500),
            retry_attempts: 3,
            retry_base_delay: Duration::from_secs(1),
            retention_interval_ticks: 12,
            retention_days: 90,
            announcement_lead: ChronoDuration::hours(24),
        }
    }
}

impl SchedulerSettings {
    fn from_env() -> Self {
        let mut settings = Self::default();
        if let Ok(value) = std::env::var("BOT_SCHEDULER_INTERVAL_SECS")
            && let Ok(seconds) = value.parse::<u64>()
        {
            settings.tick_interval = Duration::from_secs(seconds.max(1));
        }
        if let Ok(value) = std::env::var("BOT_CHECKPOINT_WINDOW_SECS")
            && let Ok(seconds) = value.parse::<u64>()
        {
            settings.checkpoint_window = Duration::from_secs(seconds.max(1));
        }
        if let Ok(value) = std::env::var("BOT_REQUEST_SPACING_MS")
            && let Ok(milliseconds) = value.parse()
        {
            settings.request_spacing = Duration::from_millis(milliseconds);
        }
        if let Ok(value) = std::env::var("BOT_RETRY_ATTEMPTS")
            && let Ok(attempts) = value.parse::<usize>()
        {
            settings.retry_attempts = attempts.max(1);
        }
        if let Ok(value) = std::env::var("BOT_RETENTION_DAYS")
            && let Ok(days) = value.parse::<i64>()
        {
            settings.retention_days = days.max(1);
        }
        if let Ok(value) = std::env::var("BOT_ANNOUNCEMENT_LEAD_HOURS")
            && let Ok(hours) = value.parse::<i64>()
        {
            settings.announcement_lead = ChronoDuration::hours(hours.max(1));
        }
        settings
    }
}

/// Main scheduler loop. Player snapshots are polled, while Nanoka work sleeps
/// until the active season ends before refreshing the following rotation.
pub async fn run(
    bot: Bot,
    state: Arc<BotState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    log::info!("Scheduler started");
    let settings = SchedulerSettings::from_env();
    let mut interval = tokio::time::interval(settings.tick_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    if let Err(error) = refresh_season_events(&state).await {
        // Keep player refreshes and checkpoint delivery alive when Nanoka is
        // temporarily unavailable; an existing DB index remains usable.
        log::error!("Initial Nanoka season refresh failed: {error:#}");
    }
    check_announcements(&bot, &state, &settings).await;
    let mut next_event_at =
        tokio::time::Instant::now() + next_season_transition_delay(&state, &settings).await?;
    let mut ticks = 0_u64;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                ticks += 1;
                tick(&bot, &state, &settings).await;
                if ticks.is_multiple_of(settings.retention_interval_ticks) {
                    match state.db.cleanup_old_results(settings.retention_days).await {
                        Ok(deleted) if deleted > 0 => {
                            log::info!("Removed {deleted} expired result snapshots");
                        }
                        Ok(_) => {}
                        Err(error) => log::error!("Retention cleanup failed: {error}"),
                    }
                }
            }
            _ = tokio::time::sleep_until(next_event_at) => {
                if let Err(error) = refresh_season_events(&state).await {
                    log::error!("Nanoka season refresh failed: {error:#}");
                }
                check_announcements(&bot, &state, &settings).await;
                next_event_at = tokio::time::Instant::now()
                    + next_season_transition_delay(&state, &settings).await?;
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    log::info!("Scheduler stopped");
                    return Ok(());
                }
            }
        }
    }
}

const EVENT_TYPES: [(&str, &str); 2] = [
    ("deadly_assault", "Deadly Assault"),
    ("shiyu_defense", "Shiyu Defense"),
];

/// Fetch only the small Nanoka indexes and upsert their production schedule.
/// We cache no season details: a card is fetched only when it will be delivered.
async fn refresh_season_events(state: &BotState) -> anyhow::Result<()> {
    let deadly = state.nanoka.get_boss_seasons().await?;
    let shiyu = state.nanoka.get_seasons().await?;
    let mut events = Vec::new();
    for (endgame_type, seasons, expected_sort, prefix) in [
        ("deadly_assault", deadly, 9, "69"),
        ("shiyu_defense", shiyu, 1, "62"),
    ] {
        for (season_id, meta) in seasons {
            if season_id.len() != 5 || !season_id.starts_with(prefix) || meta.sort != expected_sort
            {
                continue;
            }
            let Some(starts_at) = meta.live_begin.as_deref().and_then(parse_nanoka_datetime) else {
                continue;
            };
            let ends_at = meta
                .live_end
                .as_deref()
                .and_then(parse_nanoka_datetime)
                .map(|time| time.to_rfc3339());
            events.push(crate::db::SeasonEvent {
                endgame_type: endgame_type.to_owned(),
                season_id,
                starts_at: starts_at.to_rfc3339(),
                ends_at,
                name: meta.en,
            });
        }
    }
    state.db.cache_season_events(&events).await
}

/// Wake at the later of the current season's end and the next card's normal
/// lead-time. Thus the next rotation is fetched only after the current one
/// ends, but its announcement still follows the configured timing whenever
/// that window is available.
async fn next_season_transition_delay(
    state: &BotState,
    settings: &SchedulerSettings,
) -> anyhow::Result<Duration> {
    let now = Utc::now();
    let active = state.db.active_season_events(&now.to_rfc3339()).await?;
    let mut earliest = None;
    for (endgame_type, _) in EVENT_TYPES {
        let current_end = active
            .iter()
            .find(|event| event.endgame_type == endgame_type)
            .and_then(|event| event.ends_at.as_deref())
            .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
            .map(|time| time.with_timezone(&Utc))
            .filter(|time| *time > now);
        if let Some(event) = state
            .db
            .next_season_event(endgame_type, &now.to_rfc3339())
            .await?
            && let Ok(starts_at) = DateTime::parse_from_rfc3339(&event.starts_at)
        {
            let trigger = starts_at.with_timezone(&Utc) - settings.announcement_lead;
            let wake_at = current_end.map_or(trigger, |end| end.max(trigger));
            earliest =
                Some(earliest.map_or(wake_at, |current: DateTime<Utc>| current.min(wake_at)));
        } else if let Some(end) = current_end {
            earliest = Some(earliest.map_or(end, |current: DateTime<Utc>| current.min(end)));
        }
    }
    // A daily fallback only covers an incomplete index (for example a missing
    // live_end); normal operation waits for an actual season transition.
    Ok(earliest
        .and_then(|time| (time - now).to_std().ok())
        .filter(|delay| !delay.is_zero())
        .unwrap_or(Duration::from_secs(24 * 60 * 60)))
}

async fn check_announcements(bot: &Bot, state: &Arc<BotState>, settings: &SchedulerSettings) {
    if let Err(error) = check_deadly_assault_announcement(bot, state, settings).await {
        log::error!("Deadly Assault announcement error: {error:#}");
    }
    if let Err(error) = check_shiyu_defense_announcement(bot, state, settings).await {
        log::error!("Shiyu Defense announcement error: {error:#}");
    }
}

async fn tick(bot: &Bot, state: &Arc<BotState>, settings: &SchedulerSettings) {
    // 1. Fetch data for all tracked users
    if let Err(e) = fetch_all_bg(state).await {
        log::error!("Fetch error: {e}");
    }

    // 2. Check for checkpoint posts
    if let Err(e) = check_checkpoints(bot, state, settings).await {
        log::error!("Checkpoint error: {e}");
    }
}

/// Fetch endgame data for all registered users.
/// Returns (success_count, error_count).
pub async fn fetch_all_users(cookie: &str, state: &BotState) -> anyhow::Result<(usize, usize)> {
    let client = rustverse::client::zzz::ZZZClient::from_cookie_string(cookie)?;
    let users = state.db.get_all_users().await?;

    let mut ok_count = 0;
    let mut err_count = 0;

    for user in &users {
        match fetch_one_inner(&client, &user.uid, user, state).await {
            Ok(true) => ok_count += 1,
            Ok(false) => { /* data not public, counted as skipped */ }
            Err(_) => err_count += 1,
        }
    }

    log::info!(
        "Fetched {} users ({} ok, {} errors)",
        users.len(),
        ok_count,
        err_count
    );
    Ok((ok_count, err_count))
}

/// Fetch endgame data for a single UID (doesn't need to be registered).
/// Returns true if data was stored successfully, false if data not public.
pub async fn fetch_single_user(cookie: &str, uid: &str, state: &BotState) -> anyhow::Result<bool> {
    let client = rustverse::client::zzz::ZZZClient::from_cookie_string(cookie)?;
    // Use a placeholder UserRow — we just need uid for this fetch
    let placeholder = crate::db::UserRow {
        chat_id: 0,
        telegram_user_id: 0,
        uid: uid.to_string(),
        nickname: None,
    };
    fetch_one_inner(&client, uid, &placeholder, state).await
}

/// Core fetch logic for a single user. Shared by fetch_all_users and fetch_single_user.
async fn fetch_one_inner(
    client: &rustverse::client::zzz::ZZZClient,
    uid: &str,
    user: &crate::db::UserRow,
    state: &BotState,
) -> anyhow::Result<bool> {
    let mut any_stored = false;

    // Fetch Deadly Assault
    match retry_hoyolab(|| client.get_deadly_assault(uid, None, "1")).await {
        Ok(data) => {
            let season_start = data
                .start_time
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_default();
            let season_end = data
                .end_time
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_default();
            let total_score = data.total_score.unwrap_or(0);
            let json = serde_json::to_string(&data).context("failed to serialize DA result")?;

            if data.has_data.unwrap_or(false) || total_score > 0 {
                state
                    .db
                    .insert_result(
                        uid,
                        "deadly_assault",
                        &season_start,
                        &season_end,
                        total_score,
                        &json,
                    )
                    .await?;
                any_stored = true;

                if let Some(ref nick) = data.nick_name
                    && !nick.is_empty()
                    && user.nickname.is_none()
                    && user.chat_id != 0
                {
                    state
                        .db
                        .add_user(user.chat_id, user.telegram_user_id, uid, Some(nick))
                        .await?;
                }
            }
        }
        Err(rustverse::error::HoyoverseError::DataNotPublic) => {
            log::debug!("DA data not public for {uid}");
        }
        Err(e) => {
            log::debug!("DA fetch failed for {uid}: {e}");
        }
    }

    tokio::time::sleep(SchedulerSettings::from_env().request_spacing).await;

    // Fetch Shiyu Defense
    match retry_hoyolab(|| client.get_shiyu_defense(uid, None, "1")).await {
        Ok(data) => {
            let season_start = data
                .hadal_begin_time
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_default();
            let season_end = data
                .hadal_end_time
                .as_ref()
                .map(|t| t.to_string())
                .unwrap_or_default();
            let total_score = data.brief.as_ref().and_then(|b| b.score).unwrap_or(0);
            let json = serde_json::to_string(&data).context("failed to serialize SD result")?;

            if !season_start.is_empty() {
                state
                    .db
                    .insert_result(
                        uid,
                        "shiyu_defense",
                        &season_start,
                        &season_end,
                        total_score,
                        &json,
                    )
                    .await?;
                any_stored = true;
            }
        }
        Err(rustverse::error::HoyoverseError::DataNotPublic) => {
            log::debug!("SD data not public for {uid}");
        }
        Err(e) => {
            log::debug!("SD fetch failed for {uid}: {e}");
        }
    }

    // Cache avatar names for this UID
    if any_stored {
        let _ = cache_avatar_names(client, uid, state).await;
    }

    Ok(any_stored)
}

async fn retry_hoyolab<T, F, Fut>(mut request: F) -> Result<T, rustverse::error::HoyoverseError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, rustverse::error::HoyoverseError>>,
{
    let settings = SchedulerSettings::from_env();
    let mut attempt = 0;
    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error @ rustverse::error::HoyoverseError::DataNotPublic) => return Err(error),
            Err(error) if attempt + 1 >= settings.retry_attempts => return Err(error),
            Err(_) => {
                let factor = 1_u32 << attempt.min(8);
                tokio::time::sleep(settings.retry_base_delay * factor).await;
                attempt += 1;
            }
        }
    }
}

async fn cache_avatar_names(
    client: &rustverse::client::zzz::ZZZClient,
    uid: &str,
    state: &BotState,
) {
    match client.get_avatar_list(uid, None).await {
        Ok(list) => {
            let names: Vec<(i64, &str)> = list
                .avatar_list
                .iter()
                .map(|a| (a.id, a.name_mi18n.as_str()))
                .collect();
            if let Err(e) = state.db.cache_avatars(uid, &names).await {
                log::debug!("Failed to cache avatars for {uid}: {e}");
            }
        }
        Err(e) => {
            log::debug!("Avatar list fetch failed for {uid}: {e}");
        }
    }
}

/// Internal tick helper used by the background scheduler.
async fn fetch_all_bg(state: &Arc<BotState>) -> anyhow::Result<()> {
    let cookie = match state.db.get_cookie().await? {
        Some(c) => c,
        None => {
            log::warn!("No cookie set, skipping fetch");
            return Ok(());
        }
    };
    let _ = fetch_all_users(&cookie, state).await?;
    Ok(())
}

/// Nanoka season indexes use the Europe game-server time (UTC+1).
fn parse_nanoka_datetime(value: &str) -> Option<DateTime<Utc>> {
    let local = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?;
    let nanoka_timezone = FixedOffset::east_opt(60 * 60)?;
    nanoka_timezone
        .from_local_datetime(&local)
        .single()
        .map(|date| date.with_timezone(&Utc))
}

/// HoYoLAB player-result timestamps retain their API UTC+8 interpretation.
fn parse_hoyolab_datetime(value: &str) -> Option<DateTime<Utc>> {
    let local = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?;
    let hoyolab_timezone = FixedOffset::east_opt(8 * 60 * 60)?;
    hoyolab_timezone
        .from_local_datetime(&local)
        .single()
        .map(|date| date.with_timezone(&Utc))
}

async fn upcoming_season(
    state: &BotState,
    endgame_type: &str,
    now: DateTime<Utc>,
    lead: ChronoDuration,
) -> anyhow::Result<Option<(u64, DateTime<Utc>)>> {
    let Some(event) = state
        .db
        .next_season_event(endgame_type, &now.to_rfc3339())
        .await?
    else {
        return Ok(None);
    };
    let season_id = event.season_id.parse()?;
    let starts_at = DateTime::parse_from_rfc3339(&event.starts_at)?.with_timezone(&Utc);
    Ok((starts_at <= now + lead).then_some((season_id, starts_at)))
}

#[cfg(test)]
fn upcoming_from_index(
    seasons: &std::collections::HashMap<String, nanoka::types::SeasonMeta>,
    now: DateTime<Utc>,
    lead: ChronoDuration,
    endgame_type: nanoka::types::EndgameType,
    sort: u32,
) -> Option<(u64, DateTime<Utc>)> {
    seasons
        .iter()
        .filter_map(|(id, meta)| {
            (id.len() == 5 && meta.sort == sort)
                .then(|| id.parse::<u64>().ok())
                .flatten()
                .filter(|id| nanoka::types::EndgameType::from_id(*id) == Some(endgame_type))
                .zip(parse_nanoka_datetime(meta.live_begin.as_deref()?))
        })
        .filter(|(_, starts_at)| *starts_at > now && *starts_at <= now + lead)
        .min_by_key(|(_, starts_at)| *starts_at)
}

#[cfg(test)]
fn upcoming_deadly_assault(
    seasons: &std::collections::HashMap<String, nanoka::types::SeasonMeta>,
    now: DateTime<Utc>,
    lead: ChronoDuration,
) -> Option<(u64, DateTime<Utc>)> {
    upcoming_from_index(
        seasons,
        now,
        lead,
        nanoka::types::EndgameType::DeadlyAssault,
        9,
    )
}

#[cfg(test)]
fn upcoming_shiyu_defense(
    seasons: &std::collections::HashMap<String, nanoka::types::SeasonMeta>,
    now: DateTime<Utc>,
    lead: ChronoDuration,
) -> Option<(u64, DateTime<Utc>)> {
    upcoming_from_index(
        seasons,
        now,
        lead,
        nanoka::types::EndgameType::ShiyuDefence,
        1,
    )
}

async fn check_deadly_assault_announcement(
    bot: &Bot,
    state: &Arc<BotState>,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    let chats = state.db.get_distinct_chats().await?;
    if chats.is_empty() {
        return Ok(());
    }

    let Some((season_id, starts_at)) = upcoming_season(
        state,
        "deadly_assault",
        Utc::now(),
        settings.announcement_lead,
    )
    .await?
    else {
        return Ok(());
    };
    let season_id_string = season_id.to_string();

    let mut pending_chats = Vec::new();
    for chat_id in chats {
        match state
            .db
            .is_season_announcement_posted(chat_id, "deadly_assault", &season_id_string)
            .await
        {
            Ok(false) => pending_chats.push(chat_id),
            Ok(true) => {}
            Err(error) => {
                log::error!(
                    "Could not inspect DA announcement {season_id} for chat {chat_id}: {error}"
                );
            }
        }
    }
    if pending_chats.is_empty() {
        return Ok(());
    }

    let detail = state
        .nanoka
        .get_boss_detail_resolved(season_id, None)
        .await
        .with_context(|| format!("fetching resolved Deadly Assault season {season_id}"))?;
    let nanoka_timezone = FixedOffset::east_opt(60 * 60).context("invalid Nanoka offset")?;
    let begin_time = starts_at
        .with_timezone(&nanoka_timezone)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let png = tokio::task::spawn_blocking(move || {
        rustverse_svg::deadly_info_with_begin_time(&detail, Some(&begin_time))
    })
    .await
    .context("Deadly Assault renderer task panicked")??;

    let msk = FixedOffset::east_opt(3 * 60 * 60).context("invalid MSK offset")?;
    let caption_data = serde_json::json!({
        "season_id": season_id,
        "starts_at": starts_at.with_timezone(&msk).format("%d.%m.%Y %H:%M МСК").to_string(),
    });
    let starts_at_storage = starts_at.to_rfc3339();

    for chat_id in pending_chats {
        let mut delivered = false;
        for attempt in 0..settings.retry_attempts {
            let sender = BotTemplateSender::new(bot, &state.templates);
            match sender
                .send_photo(
                    ChatId(chat_id),
                    InputFile::memory(png.clone()),
                    "deadly_info_announcement",
                    &caption_data,
                )
                .await
            {
                Ok(_) => {
                    delivered = true;
                    break;
                }
                Err(error) if attempt + 1 < settings.retry_attempts => {
                    log::warn!(
                        "DA announcement {season_id} attempt {} failed for chat {chat_id}: {error}",
                        attempt + 1
                    );
                    let factor = 1_u32 << attempt.min(8);
                    tokio::time::sleep(settings.retry_base_delay * factor).await;
                }
                Err(error) => {
                    log::error!(
                        "DA announcement {season_id} could not be sent to chat {chat_id}: {error}"
                    );
                }
            }
        }

        if delivered
            && let Err(error) = state
                .db
                .mark_season_announcement_posted(
                    chat_id,
                    "deadly_assault",
                    &season_id_string,
                    &starts_at_storage,
                )
                .await
        {
            log::error!(
                "Could not persist DA announcement {season_id} for chat {chat_id}: {error}"
            );
        }
        tokio::time::sleep(settings.request_spacing).await;
    }
    Ok(())
}

async fn check_shiyu_defense_announcement(
    bot: &Bot,
    state: &Arc<BotState>,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    let chats = state.db.get_distinct_chats().await?;
    if chats.is_empty() {
        return Ok(());
    }

    let Some((season_id, starts_at)) = upcoming_season(
        state,
        "shiyu_defense",
        Utc::now(),
        settings.announcement_lead,
    )
    .await?
    else {
        return Ok(());
    };
    let season_id_string = season_id.to_string();

    let mut pending_chats = Vec::new();
    for chat_id in chats {
        match state
            .db
            .is_season_announcement_posted(chat_id, "shiyu_defense", &season_id_string)
            .await
        {
            Ok(false) => pending_chats.push(chat_id),
            Ok(true) => {}
            Err(error) => {
                log::error!(
                    "Could not inspect Shiyu announcement {season_id} for chat {chat_id}: {error}"
                );
            }
        }
    }
    if pending_chats.is_empty() {
        return Ok(());
    }

    let detail = state
        .nanoka
        .get_detail_resolved(season_id)
        .await
        .with_context(|| format!("fetching resolved Shiyu Defense season {season_id}"))?;
    let nanoka::types::AnySeasonDetail::Shiyu(detail) = detail else {
        anyhow::bail!("Nanoka returned a non-Shiyu detail for season {season_id}");
    };
    let png = tokio::task::spawn_blocking(move || rustverse_svg::shiyu_info(&detail))
        .await
        .context("Shiyu Defense renderer task panicked")??;

    let msk = FixedOffset::east_opt(3 * 60 * 60).context("invalid MSK offset")?;
    let caption_data = serde_json::json!({
        "season_id": season_id,
        "starts_at": starts_at.with_timezone(&msk).format("%d.%m.%Y %H:%M МСК").to_string(),
    });
    let starts_at_storage = starts_at.to_rfc3339();

    for chat_id in pending_chats {
        let mut delivered = false;
        for attempt in 0..settings.retry_attempts {
            let sender = BotTemplateSender::new(bot, &state.templates);
            match sender
                .send_photo(
                    ChatId(chat_id),
                    InputFile::memory(png.clone()),
                    "shiyu_info_announcement",
                    &caption_data,
                )
                .await
            {
                Ok(_) => {
                    delivered = true;
                    break;
                }
                Err(error) if attempt + 1 < settings.retry_attempts => {
                    log::warn!(
                        "Shiyu announcement {season_id} attempt {} failed for chat {chat_id}: {error}",
                        attempt + 1
                    );
                    let factor = 1_u32 << attempt.min(8);
                    tokio::time::sleep(settings.retry_base_delay * factor).await;
                }
                Err(error) => {
                    log::error!(
                        "Shiyu announcement {season_id} could not be sent to chat {chat_id}: {error}"
                    );
                }
            }
        }

        if delivered
            && let Err(error) = state
                .db
                .mark_season_announcement_posted(
                    chat_id,
                    "shiyu_defense",
                    &season_id_string,
                    &starts_at_storage,
                )
                .await
        {
            log::error!(
                "Could not persist Shiyu announcement {season_id} for chat {chat_id}: {error}"
            );
        }
        tokio::time::sleep(settings.request_spacing).await;
    }
    Ok(())
}

fn current_checkpoints(
    season_start: DateTime<Utc>,
    now: DateTime<Utc>,
    window: ChronoDuration,
) -> Vec<&'static str> {
    CHECKPOINTS
        .iter()
        .filter_map(|(label, offset)| {
            let checkpoint = season_start + *offset;
            (now >= checkpoint && now < checkpoint + window).then_some(*label)
        })
        .collect()
}

async fn send_checkpoint(
    bot: &Bot,
    state: &BotState,
    chat_id: i64,
    endgame_type: &str,
    label: &str,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    let (png, caption) =
        handlers::build_top_image_and_caption(state, chat_id, endgame_type, label).await?;

    let mut last_error = None;
    for attempt in 0..settings.retry_attempts {
        let sender = BotTemplateSender::new(bot, &state.templates);
        let result = if png.is_empty() {
            sender
                .send_rendered_message(ChatId(chat_id), caption.clone())
                .await
        } else {
            sender
                .send_photo_with_rendered(
                    ChatId(chat_id),
                    InputFile::memory(png.clone()),
                    caption.clone(),
                )
                .await
        };
        match result {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < settings.retry_attempts {
                    let factor = 1_u32 << attempt.min(8);
                    tokio::time::sleep(settings.retry_base_delay * factor).await;
                }
            }
        }
    }
    last_error.map_or_else(
        || {
            Err(anyhow::anyhow!(
                "Telegram retry attempts must be greater than zero"
            ))
        },
        Err,
    )
}

/// Post only checkpoints whose short delivery window is currently open.
async fn check_checkpoints(
    bot: &Bot,
    state: &Arc<BotState>,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    let chats = state.db.get_distinct_chats().await?;
    if chats.is_empty() {
        return Ok(());
    }

    for (et, _) in EVENT_TYPES {
        // Get the latest season start from results
        let season_start = match state.db.get_latest_season_start(et).await? {
            Some(s) => s,
            None => continue,
        };

        let Some(season_start_dt) = parse_hoyolab_datetime(&season_start) else {
            log::warn!("Invalid season start returned by API: {season_start}");
            continue;
        };

        let checkpoint_window = ChronoDuration::from_std(settings.checkpoint_window)
            .context("checkpoint window is outside chrono's supported range")?;
        for label in current_checkpoints(season_start_dt, Utc::now(), checkpoint_window) {
            for chat_id in &chats {
                let entries = match state
                    .db
                    .get_latest_results(*chat_id, et, &season_start)
                    .await
                {
                    Ok(entries) => entries,
                    Err(error) => {
                        log::error!("Could not read results for chat {chat_id}: {error}");
                        continue;
                    }
                };
                if entries.is_empty() {
                    continue;
                }
                let posted = state
                    .db
                    .is_checkpoint_posted(*chat_id, et, &season_start, label)
                    .await
                    .with_context(|| {
                        format!("failed to inspect checkpoint {label} for {et}, chat {chat_id}")
                    });
                let posted = match posted {
                    Ok(posted) => posted,
                    Err(error) => {
                        log::error!("{error:#}");
                        continue;
                    }
                };
                if posted {
                    continue;
                }

                match send_checkpoint(bot, state, *chat_id, et, label, settings).await {
                    Ok(()) => {
                        if let Err(error) = state
                            .db
                            .mark_checkpoint_posted(*chat_id, et, &season_start, label)
                            .await
                        {
                            // Delivery succeeded, but leaving it unmarked is safer than
                            // hiding the persistence failure; the next tick may retry.
                            log::error!(
                                "Could not persist checkpoint {label} for chat {chat_id}: {error}"
                            );
                        }
                    }
                    Err(error) => {
                        log::error!(
                            "Checkpoint {label} for {et} could not be sent to chat {chat_id}: {error}"
                        );
                    }
                }
                tokio::time::sleep(settings.request_spacing).await;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanoka::types::SeasonMeta;
    use std::collections::HashMap;

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn nanoka_live_begin_is_interpreted_as_europe_server_time() {
        let msk = FixedOffset::east_opt(3 * 60 * 60).unwrap();
        // Production index examples 69041 and 62053 currently use 04:00:00.
        let start = parse_nanoka_datetime("2026-07-24 04:00:00").unwrap();
        assert_eq!(
            start,
            utc("2026-07-24T03:00:00Z"),
            "04:00 Europe-server time must be stored as 03:00 UTC"
        );
        assert_eq!(
            start.with_timezone(&msk).format("%H:%M").to_string(),
            "06:00"
        );
    }

    #[test]
    fn hoyolab_timestamp_remains_utc_plus_eight() {
        assert_eq!(
            parse_hoyolab_datetime("2026-07-03 04:00:00"),
            Some(utc("2026-07-02T20:00:00Z"))
        );
    }

    #[test]
    fn checkpoint_is_due_at_the_exact_boundary() {
        let start = utc("2026-07-02T20:00:00Z");
        let window = ChronoDuration::minutes(5);
        assert!(
            current_checkpoints(start, start + ChronoDuration::hours(6), window).contains(&"6h")
        );
        assert!(
            !current_checkpoints(
                start,
                start + ChronoDuration::hours(6) - ChronoDuration::seconds(1),
                window,
            )
            .contains(&"6h")
        );
    }

    #[test]
    fn expired_checkpoints_are_never_sent_after_restart() {
        let start = utc("2026-07-02T20:00:00Z");
        assert_eq!(
            current_checkpoints(
                start,
                start + ChronoDuration::days(15),
                ChronoDuration::minutes(5)
            ),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn checkpoint_expires_at_the_end_of_its_delivery_window() {
        let start = utc("2026-07-02T20:00:00Z");
        let checkpoint = start + ChronoDuration::hours(24);
        let window = ChronoDuration::minutes(5);

        assert_eq!(
            current_checkpoints(
                start,
                checkpoint + window - ChronoDuration::seconds(1),
                window
            ),
            vec!["24h"]
        );
        assert!(current_checkpoints(start, checkpoint + window, window).is_empty());
    }

    #[test]
    fn invalid_api_timestamp_is_rejected() {
        assert_eq!(parse_nanoka_datetime("not a timestamp"), None);
        assert_eq!(parse_hoyolab_datetime("not a timestamp"), None);
    }

    fn season(begin: Option<&str>, live_begin: Option<&str>) -> SeasonMeta {
        SeasonMeta {
            sort: 9,
            en: "Trial".to_owned(),
            ko: String::new(),
            zh: String::new(),
            ja: String::new(),
            begin: begin.map(str::to_owned),
            end: None,
            live_begin: live_begin.map(str::to_owned),
            live_end: None,
        }
    }

    #[test]
    fn selects_nearest_da_only_during_the_day_before_start() {
        let now = utc("2026-07-24T06:00:00Z");
        let mut seasons = HashMap::new();
        seasons.insert(
            "69040".to_owned(),
            season(None, Some("2026-07-23 08:00:00")),
        );
        seasons.insert(
            "69041".to_owned(),
            season(None, Some("2026-07-25 07:00:00")),
        );
        seasons.insert(
            "69042".to_owned(),
            season(None, Some("2026-07-26 07:00:00")),
        );

        assert_eq!(
            upcoming_deadly_assault(&seasons, now, ChronoDuration::hours(24)),
            Some((69041, utc("2026-07-25T06:00:00Z")))
        );
        let started = HashMap::from([(
            "69041".to_owned(),
            season(None, Some("2026-07-25 07:00:00")),
        )]);
        assert_eq!(
            upcoming_deadly_assault(
                &started,
                utc("2026-07-25T06:00:00Z"),
                ChronoDuration::hours(24)
            ),
            None,
            "a season is never announced after it has started"
        );
    }

    #[test]
    fn live_begin_can_select_a_rerun_of_an_old_season() {
        let now = utc("2026-07-24T05:00:00Z");
        let seasons = HashMap::from([(
            "69041".to_owned(),
            season(Some("2026-01-01 06:00:00"), Some("2026-07-25 06:00:00")),
        )]);
        assert_eq!(
            upcoming_deadly_assault(&seasons, now, ChronoDuration::hours(24)),
            Some((69041, utc("2026-07-25T05:00:00Z")))
        );
    }

    #[test]
    fn selects_nearest_critical_node_for_shiyu() {
        let now = utc("2026-07-23T03:00:00Z");
        let mut critical = season(None, Some("2026-07-24 04:00:00"));
        critical.sort = 1;
        let mut stable = season(None, Some("2026-07-23 04:00:00"));
        stable.sort = 2;
        let seasons = HashMap::from([("62053".to_owned(), critical), ("61010".to_owned(), stable)]);

        assert_eq!(
            upcoming_shiyu_defense(&seasons, now, ChronoDuration::hours(24)),
            Some((62053, utc("2026-07-24T03:00:00Z")))
        );
    }

    #[test]
    fn ignores_beta_ids_and_non_live_dates() {
        let now = utc("2026-07-24T00:00:00Z");
        let seasons = HashMap::from([
            (
                "690421".to_owned(),
                season(Some("2026-07-25 06:00:00"), Some("2026-07-25 06:00:00")),
            ),
            (
                "69042".to_owned(),
                season(Some("2026-07-25 06:00:00"), None),
            ),
        ]);
        assert_eq!(
            upcoming_deadly_assault(&seasons, now, ChronoDuration::hours(24)),
            None
        );
    }
}
