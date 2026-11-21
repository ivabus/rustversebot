use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::Arc;
use std::time::Duration;
use teloxide::{prelude::*, types::InputFile};

use crate::BotState;
use crate::bot_templates::BotTemplateSender;
use crate::handlers;
use crate::season_time::{EUROPE_SERVER, WALL_TIME, moscow_date, moscow_time, parse_europe_time};

const CHECKPOINTS: [(&str, ChronoDuration); 3] = [
    ("6h", ChronoDuration::hours(6)),
    ("24h", ChronoDuration::hours(24)),
    ("14d", ChronoDuration::days(14)),
];

#[derive(Debug, Clone)]
pub(crate) struct SchedulerSettings {
    tick_interval: Duration,
    checkpoint_window: Duration,
    pub(crate) request_spacing: Duration,
    pub(crate) retry_attempts: usize,
    pub(crate) retry_base_delay: Duration,
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
    pub(crate) fn from_env() -> Self {
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

/// Poll the calendar, player snapshots and delivery windows independently.
pub async fn run(
    bot: Bot,
    state: Arc<BotState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let settings = SchedulerSettings::from_env();
    let interval = |period| {
        let mut ticks = tokio::time::interval(period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticks
    };
    let players = async {
        let mut ticks = interval(settings.tick_interval);
        let mut count = 0_u64;
        loop {
            ticks.tick().await;
            if let Err(error) = state.db.expire_prompt_sessions().await {
                log::warn!("Prompt session cleanup failed: {error}");
            }
            if let Err(error) = fetch_all_bg(&state).await {
                log::error!("Fetch error: {error}");
            }
            count += 1;
            if count.is_multiple_of(settings.retention_interval_ticks)
                && let Err(error) = state.db.cleanup_old_results(settings.retention_days).await
            {
                log::error!("Retention cleanup failed: {error}");
            }
        }
    };
    let checkpoints = async {
        let mut ticks = interval(settings.tick_interval.min(settings.checkpoint_window / 2));
        loop {
            ticks.tick().await;
            if let Err(error) = check_checkpoints(&bot, &state, &settings).await {
                log::error!("Checkpoint error: {error}");
            }
        }
    };
    let announcements = async {
        let mut ticks = interval(settings.tick_interval);
        let mut next_refresh = tokio::time::Instant::now();
        loop {
            ticks.tick().await;
            if tokio::time::Instant::now() >= next_refresh {
                match refresh_season_events(&state).await {
                    Ok(()) => {
                        next_refresh = tokio::time::Instant::now() + Duration::from_secs(3600)
                    }
                    Err(error) => log::error!("Nanoka season refresh failed: {error:#}"),
                }
            }
            check_announcements(&bot, &state, &settings).await;
        }
    };
    let promo_announcements = async {
        let mut ticks = interval(settings.tick_interval);
        loop {
            ticks.tick().await;
            if let Err(error) = announce_promocodes(&bot, &state, &settings).await {
                log::warn!("Promo code poll failed: {error:#}");
            }
        }
    };
    let beta_announcements = async {
        let mut ticks = interval(settings.tick_interval);
        loop {
            ticks.tick().await;
            if let Err(error) = crate::beta::poll(&bot, &state, &settings).await {
                log::warn!("Beta changelog poll failed: {error:#}");
            }
        }
    };
    tokio::select! {
        _ = beta_announcements => {},
        _ = players => {},
        _ = checkpoints => {},
        _ = announcements => {},
        _ = promo_announcements => {},
        _ = shutdown.wait_for(|stop| *stop) => {},
    }
    Ok(())
}

const EVENT_TYPES: [(&str, &str); 2] = [
    ("deadly_assault", "Deadly Assault"),
    ("shiyu_defense", "Shiyu Defense"),
];

async fn announce_promocodes(
    bot: &Bot,
    state: &BotState,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    if state.db.get_distinct_chats().await?.is_empty() {
        return Ok(());
    }
    let listing = crate::promocodes::fetch(&state.http).await?;
    deliver_promo_listing(bot, state, settings, &listing).await
}

async fn deliver_promo_listing(
    bot: &Bot,
    state: &BotState,
    settings: &SchedulerSettings,
    listing: &crate::promocodes::Listing,
) -> anyhow::Result<()> {
    let codes: Vec<_> = listing
        .codes
        .iter()
        .map(|entry| entry.code.as_str())
        .collect();
    let mut pending = state.db.pending_promo_announcements(&codes).await?;
    let sender = BotTemplateSender::new(bot, &state.templates);
    for entry in &listing.codes {
        let Some(chats) = pending.remove(&entry.code) else {
            continue;
        };
        let message = state.templates.render(
            "promocodes",
            &serde_json::json!({
                "announcement": true, "codes": [entry], "source": listing.source,
            }),
        )?;
        for chat in chats {
            for attempt in 0..settings.retry_attempts {
                match sender
                    .send_rendered_message(ChatId(chat), message.clone())
                    .await
                {
                    Ok(_) => {
                        if let Err(error) = state.db.mark_promo_announced(chat, &entry.code).await {
                            log::error!(
                                "Could not record promo code {} for chat {chat}: {error:#}",
                                entry.code
                            );
                        }
                        break;
                    }
                    Err(error) => {
                        log::warn!(
                            "Could not send promo code {} to chat {chat}: {error:#}",
                            entry.code
                        );
                        if attempt + 1 < settings.retry_attempts {
                            tokio::time::sleep(
                                settings.retry_base_delay * (1_u32 << attempt.min(8)),
                            )
                            .await;
                        }
                    }
                }
            }
            tokio::time::sleep(settings.request_spacing).await;
        }
    }
    Ok(())
}

/// Fetch the Nanoka indexes and store their production schedule.
///
/// Do not cache season details. Fetch a card only when the bot can deliver it.
async fn refresh_season_events(state: &BotState) -> anyhow::Result<()> {
    let (deadly, shiyu) =
        tokio::try_join!(state.nanoka.get_boss_seasons(), state.nanoka.get_seasons())?;
    let mut events = Vec::new();
    for (kind, seasons) in [("deadly_assault", deadly), ("shiyu_defense", shiyu)] {
        events.extend(
            seasons
                .into_iter()
                .filter_map(|(id, meta)| live_event(kind, id, meta)),
        );
    }
    state.db.cache_season_events(&events).await
}

fn live_event(
    kind: &str,
    id: String,
    meta: nanoka::types::SeasonMeta,
) -> Option<crate::db::SeasonEvent> {
    let valid = match kind {
        "deadly_assault" => matches!(id.len(), 5 | 6) && id.starts_with("69") && meta.sort == 9,
        "shiyu_defense" => id.len() == 5 && id.starts_with("62") && meta.sort == 1,
        _ => false,
    };
    if !valid || !id.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(crate::db::SeasonEvent {
        endgame_type: kind.to_owned(),
        season_id: id,
        starts_at: parse_europe_time(meta.live_begin.as_deref()?)?.to_rfc3339(),
        ends_at: meta
            .live_end
            .as_deref()
            .and_then(parse_europe_time)
            .map(|end| end.to_rfc3339()),
        name: meta.en,
    })
}

async fn check_announcements(bot: &Bot, state: &BotState, settings: &SchedulerSettings) {
    for (kind, _) in EVENT_TYPES {
        if let Err(error) = announce_next_season(bot, state, kind, settings).await {
            log::error!("{kind} announcement error: {error:#}");
        }
    }
}

/// Fetch endgame data for all registered users.
/// Return the success count and the error count.
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

/// Fetch endgame data for one UID.
///
/// The UID does not need to be registered.
/// Return `true` after storage. Return `false` when the data is not public.
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

async fn announce_next_season(
    bot: &Bot,
    state: &BotState,
    kind: &str,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    let Some((event, starts_at)) =
        next_announcement(&state.db, kind, Utc::now(), settings.announcement_lead).await?
    else {
        return Ok(());
    };
    let mut pending_chats = Vec::new();
    for chat in state.db.get_distinct_chats().await? {
        match state
            .db
            .is_season_announcement_posted(chat, kind, &event.season_id)
            .await
        {
            Ok(false) => pending_chats.push(chat),
            Ok(true) => {}
            Err(error) => {
                log::error!("Could not inspect {kind} announcement for chat {chat}: {error}")
            }
        }
    }
    if pending_chats.is_empty() {
        return Ok(());
    }

    let id = event.season_id.parse()?;
    let begin = moscow_date(starts_at);
    let end = event
        .ends_at
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()?
        .map(|t| moscow_date(t.to_utc()))
        .unwrap_or_else(|| "—".into());
    let (png, template) = match kind {
        "deadly_assault" => {
            let detail = state.nanoka.get_boss_detail_resolved(id, None).await?;
            (
                rustverse_images::deadly_info(&detail, &begin, &end).await?,
                "deadly_info_announcement",
            )
        }
        "shiyu_defense" => {
            let nanoka::types::AnySeasonDetail::Shiyu(mut detail) =
                state.nanoka.get_detail_resolved(id).await?
            else {
                anyhow::bail!("Nanoka returned a non-Shiyu detail for season {id}");
            };
            detail.begin_time = Some(begin);
            detail.end_time = Some(end);
            (
                rustverse_images::shiyu_info(&detail).await?,
                "shiyu_info_announcement",
            )
        }
        _ => anyhow::bail!("unknown season type {kind}"),
    };
    let caption = serde_json::json!({"season_id": id, "starts_at": moscow_time(starts_at)});
    let sender = BotTemplateSender::new(bot, &state.templates);
    for chat in pending_chats {
        for attempt in 0..settings.retry_attempts {
            if Utc::now() >= starts_at {
                return Ok(());
            }
            match sender
                .send_photo(
                    ChatId(chat),
                    InputFile::memory(png.clone()),
                    template,
                    &caption,
                )
                .await
            {
                Ok(_) => {
                    if let Err(error) = state
                        .db
                        .mark_season_announcement_posted(
                            chat,
                            kind,
                            &event.season_id,
                            &event.starts_at,
                        )
                        .await
                    {
                        log::error!(
                            "Could not persist {kind} announcement for chat {chat}: {error}"
                        );
                    }
                    break;
                }
                Err(error) => {
                    log::warn!("{kind} announcement {id} for chat {chat}: {error}");
                    if attempt + 1 < settings.retry_attempts {
                        tokio::time::sleep(settings.retry_base_delay * (1_u32 << attempt.min(8)))
                            .await;
                    }
                }
            }
        }
        tokio::time::sleep(settings.request_spacing).await;
    }
    Ok(())
}

async fn next_announcement(
    db: &crate::db::Db,
    kind: &str,
    now: DateTime<Utc>,
    lead: ChronoDuration,
) -> anyhow::Result<Option<(crate::db::SeasonEvent, DateTime<Utc>)>> {
    let Some(event) = db.next_season_event(kind, &now.to_rfc3339()).await? else {
        return Ok(None);
    };
    let start = DateTime::parse_from_rfc3339(&event.starts_at)?.to_utc();
    Ok(((start - lead..start).contains(&now)).then_some((event, start)))
}

#[derive(Clone, Copy)]
struct Checkpoint<'a> {
    kind: &'a str,
    label: &'a str,
    season: &'a str,
    due: DateTime<Utc>,
    expires: DateTime<Utc>,
}

impl Checkpoint<'_> {
    fn is_open(&self, now: DateTime<Utc>) -> bool {
        (self.due..self.expires).contains(&now)
    }
}

async fn send_checkpoint(
    bot: &Bot,
    state: &BotState,
    chat_id: i64,
    checkpoint: Checkpoint<'_>,
    settings: &SchedulerSettings,
) -> anyhow::Result<bool> {
    if !checkpoint.is_open(Utc::now()) {
        return Ok(false);
    }
    let (png, caption) = handlers::build_top_image_and_caption(
        state,
        chat_id,
        checkpoint.kind,
        checkpoint.label,
        Some(checkpoint.season),
    )
    .await?;
    if png.is_empty() {
        return Ok(false);
    }

    let sender = BotTemplateSender::new(bot, &state.templates);
    for attempt in 0..settings.retry_attempts {
        if !checkpoint.is_open(Utc::now()) {
            return Ok(false);
        }
        match sender
            .send_photo_with_rendered(
                ChatId(chat_id),
                InputFile::memory(png.clone()),
                caption.clone(),
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(error) if attempt + 1 == settings.retry_attempts => return Err(error),
            Err(error) => {
                log::warn!(
                    "Checkpoint {} for chat {chat_id}: {error}",
                    checkpoint.label
                );
                tokio::time::sleep(settings.retry_base_delay * (1_u32 << attempt.min(8))).await;
            }
        }
    }
    Ok(false)
}

/// Post only checkpoints whose short delivery window is currently open.
async fn check_checkpoints(
    bot: &Bot,
    state: &Arc<BotState>,
    settings: &SchedulerSettings,
) -> anyhow::Result<()> {
    let now = Utc::now();
    let window = ChronoDuration::from_std(settings.checkpoint_window)?;
    for (kind, _) in EVENT_TYPES {
        for (label, offset) in CHECKPOINTS {
            // The final checkpoint can coincide with the following season's first snapshot.
            let latest = now - offset;
            let db_time = |t: DateTime<Utc>| {
                t.with_timezone(&EUROPE_SERVER)
                    .format(WALL_TIME)
                    .to_string()
            };
            let candidates = state
                .db
                .checkpoint_candidates(kind, &db_time(latest - window), &db_time(latest), label)
                .await?;
            for (chat_id, season) in candidates {
                let Some(start) = parse_europe_time(&season) else {
                    continue;
                };
                let due = start + offset;
                let checkpoint = Checkpoint {
                    kind,
                    label,
                    season: &season,
                    due,
                    expires: due + window,
                };
                match send_checkpoint(bot, state, chat_id, checkpoint, settings).await {
                    Ok(true) => {
                        if let Err(error) = state
                            .db
                            .mark_checkpoint_posted(chat_id, kind, &season, label)
                            .await
                        {
                            log::error!(
                                "Could not persist checkpoint {label} for {kind}, chat {chat_id}: {error}"
                            );
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::error!("Checkpoint {label} for {kind}, chat {chat_id}: {error}")
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
    use crate::db::Db;

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value).unwrap().to_utc()
    }

    #[test]
    fn checkpoints_use_the_europe_release_in_summer_and_winter() {
        for day in ["2026-07-10", "2026-11-06"] {
            let season = format!("{day} 04:00:00");
            let start = parse_europe_time(&season).unwrap();
            assert_eq!(start, utc(&format!("{day}T03:00:00Z")));
            for (label, offset) in CHECKPOINTS {
                let due = start + offset;
                let c = Checkpoint {
                    kind: "deadly_assault",
                    label,
                    season: &season,
                    due,
                    expires: due + ChronoDuration::minutes(5),
                };
                assert!(!c.is_open(due - ChronoDuration::seconds(1)));
                assert!(c.is_open(due));
                assert!(c.is_open(c.expires - ChronoDuration::seconds(1)));
                assert!(!c.is_open(c.expires));
                if label == "24h" {
                    assert!(!c.is_open(start + ChronoDuration::hours(14)));
                    assert!(!c.is_open(start + ChronoDuration::hours(17)));
                    assert!(c.is_open(start + ChronoDuration::hours(24)));
                }
            }
        }
        assert_eq!(
            moscow_time(utc("2026-11-07T03:00:00Z")),
            "07.11.2026 06:00:00 МСК"
        );
        assert_eq!(moscow_date(utc("2026-11-06T22:30:00Z")), "2026-11-07");
        assert!(parse_europe_time("not a date").is_none());
    }

    #[tokio::test]
    async fn live_announcements_open_a_day_before_release_even_during_an_active_season() {
        let db = Db::new_test().await.unwrap();
        let meta = |sort, live: Option<&str>| {
            serde_json::from_value(serde_json::json!({
                "sort": sort, "en": "Trial", "begin": "2026-10-21 04:00:00", "live_begin": live
            }))
            .unwrap()
        };
        let kind = "deadly_assault";
        let active =
            live_event(kind, "690491".into(), meta(9, Some("2026-11-06 04:00:00"))).unwrap();
        let next = live_event(kind, "690501".into(), meta(9, Some("2026-11-20 04:00:00"))).unwrap();
        let shiyu = live_event(
            "shiyu_defense",
            "62062".into(),
            meta(1, Some("2026-11-27 04:00:00")),
        )
        .unwrap();
        assert!(live_event(kind, "690511".into(), meta(9, None)).is_none());
        assert!(
            live_event(
                "shiyu_defense",
                "61001".into(),
                meta(2, Some("2026-11-20 04:00:00"))
            )
            .is_none()
        );
        db.cache_season_events(&[active, next, shiyu])
            .await
            .unwrap();
        let lead = ChronoDuration::hours(24);
        assert!(
            next_announcement(&db, kind, utc("2026-11-19T02:59:59Z"), lead)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            next_announcement(&db, kind, utc("2026-11-19T03:00:00Z"), lead)
                .await
                .unwrap()
                .unwrap()
                .0
                .season_id,
            "690501"
        );
        assert!(
            next_announcement(&db, kind, utc("2026-11-20T03:00:00Z"), lead)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn final_checkpoint_uses_the_ending_season_and_existing_chat_markers() {
        let db = Db::new_test().await.unwrap();
        let kind = "shiyu_defense";
        let season = "2026-10-30 04:00:00";
        for chat in [1, 2] {
            db.add_user(chat, chat, "150000001", None).await.unwrap();
        }
        for date in [season, "2026-11-13 04:00:00"] {
            db.insert_result("150000001", kind, date, "", 20, "{}")
                .await
                .unwrap();
        }
        db.mark_checkpoint_posted(1, kind, season, "14d")
            .await
            .unwrap();
        assert_eq!(
            db.checkpoint_candidates(kind, "2026-10-30 03:56:00", "2026-10-30 04:01:00", "14d")
                .await
                .unwrap(),
            [(2, season.to_owned())]
        );
    }
}
