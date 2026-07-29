use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use nanoka::types::{EndgameType, SeasonMeta};
use std::{collections::HashMap, sync::Arc};
use teloxide::{
    prelude::*,
    types::{
        InlineKeyboardButton, InlineKeyboardMarkup, InputFile, InputMedia, InputMediaPhoto,
        ParseMode,
    },
};

use crate::BotState;
use crate::Command;
use crate::bot_templates::{BotTemplateSender, RenderedTemplate};
use crate::scheduler;

fn endgame_name(endgame_type: &str) -> &str {
    match endgame_type {
        "deadly_assault" => "Deadly Assault",
        "shiyu_defense" => "Shiyu Defense",
        other => other,
    }
}

/// Main command dispatcher.
pub async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<BotState>,
) -> anyhow::Result<()> {
    let chat_id = msg.chat.id;
    let message_id = msg.id;

    match cmd {
        Command::Start => cmd_start(&bot, chat_id, &state).await,
        Command::Register(uid) => cmd_register(&bot, msg, &uid, &state).await,
        Command::Unregister(uid) => cmd_unregister(&bot, chat_id, &uid, &state).await,
        Command::Status => cmd_status(&bot, chat_id, &state).await,
        Command::Next => cmd_endgame_pair(&bot, chat_id, SeasonPosition::Next, &state).await,
        Command::Current => cmd_endgame_pair(&bot, chat_id, SeasonPosition::Current, &state).await,
        Command::Previous => {
            cmd_endgame_pair(&bot, chat_id, SeasonPosition::Previous, &state).await
        }
        Command::TopDeadly => cmd_top(&bot, chat_id, "deadly_assault", &state).await,
        Command::TopShiyu => cmd_top(&bot, chat_id, "shiyu_defense", &state).await,
        Command::Cookie(cookie) => cmd_cookie(&bot, msg, &cookie, &state).await,
        Command::RefetchAll => cmd_refetch_all(&bot, msg, &state).await,
        Command::RefetchUid(uid) => cmd_refetch_uid(&bot, msg, &uid, &state).await,
        Command::Da(uid) => cmd_detail_command(&bot, chat_id, "da", Some(&uid), &state).await,
        Command::Shiyu(uid) => cmd_detail_command(&bot, chat_id, "sd", Some(&uid), &state).await,
        Command::Uids => cmd_uids(&bot, chat_id, &state).await,
    }?;

    // Delete the command message to keep chats clean
    let _ = bot.delete_message(chat_id, message_id).await;

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeasonPosition {
    Previous,
    Current,
    Next,
}

impl SeasonPosition {
    const fn command_name(self) -> &'static str {
        match self {
            Self::Previous => "previous",
            Self::Current => "current",
            Self::Next => "next",
        }
    }

    const fn caption(self) -> &'static str {
        match self {
            Self::Previous => "Предыдущий сезон",
            Self::Current => "Текущий сезон",
            Self::Next => "Следующий сезон",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedSeason {
    id: u64,
    sequence_id: u64,
    is_test: bool,
    begin_time: String,
    starts_at: DateTime<Utc>,
    ends_at: Option<DateTime<Utc>>,
}

fn parse_nanoka_datetime(value: &str) -> Option<DateTime<Utc>> {
    let local = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?;
    let nanoka_timezone = FixedOffset::east_opt(60 * 60)?;
    nanoka_timezone
        .from_local_datetime(&local)
        .single()
        .map(|date| date.with_timezone(&Utc))
}

fn indexed_season(
    id: &str,
    meta: &SeasonMeta,
    endgame_type: EndgameType,
    sort: u32,
) -> Option<IndexedSeason> {
    if !(id.len() == 5 || id.len() == 6) || meta.sort != sort {
        return None;
    }
    let id = id.parse::<u64>().ok()?;
    if EndgameType::from_id(id) != Some(endgame_type) {
        return None;
    }
    let begin_time = meta
        .live_begin
        .as_deref()
        .or(meta.begin.as_deref())?
        .to_owned();
    let starts_at = parse_nanoka_datetime(&begin_time)?;
    let ends_at = meta
        .live_end
        .as_deref()
        .or(meta.end.as_deref())
        .and_then(parse_nanoka_datetime);
    let is_test = id >= 100_000;
    // Test-server IDs are the nominal season ID followed by one extra digit:
    // 690421 follows 69041, and 620541 follows 62053.
    let sequence_id = if is_test { id / 10 } else { id };
    Some(IndexedSeason {
        id,
        sequence_id,
        is_test,
        begin_time,
        starts_at,
        ends_at,
    })
}

fn select_indexed_season(
    seasons: &HashMap<String, SeasonMeta>,
    endgame_type: EndgameType,
    sort: u32,
    position: SeasonPosition,
    now: DateTime<Utc>,
) -> Option<IndexedSeason> {
    let mut production_entries = seasons
        .iter()
        .filter_map(|(id, meta)| indexed_season(id, meta, endgame_type, sort))
        .filter(|season| !season.is_test)
        .collect::<Vec<_>>();
    production_entries.sort_by_key(|season| season.starts_at);

    let current_production_index = production_entries
        .iter()
        .rposition(|season| season.starts_at <= now && season.ends_at.is_none_or(|end| now < end));
    let latest_started_production = production_entries
        .iter()
        .rposition(|season| season.starts_at <= now);

    // A six-digit season can graduate from preview to the live rotation while
    // retaining its preview ID. Treat it as current only after the preceding
    // five-digit production season has ended. An active five-digit season
    // always wins, so broad preview dates cannot make a beta season current.
    let promoted_current = if current_production_index.is_none() {
        latest_started_production.and_then(|previous_index| {
            let previous = &production_entries[previous_index];
            seasons
                .iter()
                .filter_map(|(id, meta)| indexed_season(id, meta, endgame_type, sort))
                .filter(|season| {
                    season.is_test
                        && season.sequence_id == previous.sequence_id + 1
                        && previous.ends_at.is_some_and(|end| season.starts_at >= end)
                        && season.starts_at <= now
                        && season.ends_at.is_none_or(|end| now < end)
                })
                .max_by_key(|season| season.starts_at)
        })
    } else {
        None
    };

    let current = current_production_index
        .map(|index| production_entries[index].clone())
        .or(promoted_current);

    let current = current?;
    match position {
        SeasonPosition::Previous if current.is_test => production_entries
            .iter()
            .filter(|season| season.starts_at < current.starts_at)
            .max_by_key(|season| season.starts_at)
            .cloned(),
        SeasonPosition::Previous => production_entries
            .iter()
            .position(|season| season.id == current.id)
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| production_entries.get(index))
            .cloned(),
        SeasonPosition::Current => Some(current.clone()),
        SeasonPosition::Next => {
            let mut test_entries = seasons
                .iter()
                .filter_map(|(id, meta)| indexed_season(id, meta, endgame_type, sort))
                .filter(|season| season.is_test && season.sequence_id > current.sequence_id)
                .collect::<Vec<_>>();
            test_entries.sort_by_key(|season| season.sequence_id);

            if let Some(test) = test_entries.first() {
                // Beta IDs encode their production counterpart with one
                // trailing digit. Do not jump from 69041 to 690431 when the
                // immediate 690421 preview is unavailable.
                return (test.sequence_id == current.sequence_id + 1).then(|| test.clone());
            }

            // Outside a beta window, retain the established production index
            // behaviour: its direct chronological neighbour is /next.
            production_entries
                .iter()
                .filter(|season| season.starts_at > current.starts_at)
                .min_by_key(|season| season.starts_at)
                .cloned()
        }
    }
}

fn indexed_season_date(season: &IndexedSeason) -> &str {
    season
        .begin_time
        .split_whitespace()
        .next()
        .unwrap_or(&season.begin_time)
}

async fn cmd_endgame_pair(
    bot: &Bot,
    chat_id: ChatId,
    position: SeasonPosition,
    state: &BotState,
) -> anyhow::Result<()> {
    let (deadly_index, shiyu_index) =
        tokio::try_join!(state.nanoka.get_boss_seasons(), state.nanoka.get_seasons())?;
    let now = Utc::now();
    let deadly = select_indexed_season(&deadly_index, EndgameType::DeadlyAssault, 9, position, now);
    let shiyu = select_indexed_season(&shiyu_index, EndgameType::ShiyuDefence, 1, position, now);
    let (Some(deadly), Some(shiyu)) = (deadly, shiyu) else {
        bot.send_message(
            chat_id,
            format!(
                "Не удалось найти полную пару сезонов для /{}.",
                position.command_name()
            ),
        )
        .await?;
        return Ok(());
    };

    let (deadly_detail, shiyu_detail) = tokio::try_join!(
        state.nanoka.get_boss_detail_resolved(deadly.id, None),
        state.nanoka.get_detail_resolved(shiyu.id)
    )?;
    let nanoka::types::AnySeasonDetail::Shiyu(shiyu_detail) = shiyu_detail else {
        anyhow::bail!("Nanoka returned a non-Shiyu detail for season {}", shiyu.id);
    };

    let deadly_begin_time = deadly.begin_time.clone();
    let (deadly_preload, shiyu_preload) = tokio::join!(
        rustverse_svg::preload_deadly_info_images(&deadly_detail),
        rustverse_svg::preload_shiyu_info_images(&shiyu_detail)
    );
    deadly_preload?;
    shiyu_preload?;

    let deadly_render = tokio::task::spawn_blocking(move || {
        rustverse_svg::deadly_info_with_begin_time(&deadly_detail, Some(&deadly_begin_time))
    });
    let shiyu_render =
        tokio::task::spawn_blocking(move || rustverse_svg::shiyu_info(&shiyu_detail));
    let (deadly_png, shiyu_png) = tokio::join!(deadly_render, shiyu_render);
    let deadly_png = deadly_png
        .map_err(|error| anyhow::anyhow!("Deadly Assault renderer task panicked: {error}"))??;
    let shiyu_png = shiyu_png
        .map_err(|error| anyhow::anyhow!("Shiyu Defense renderer task panicked: {error}"))??;

    let album_caption = format!(
        "<b>Deadly Assault</b> · {} · <b>{}</b>\n<b>Shiyu Defense</b> · {} · <b>{}</b>",
        position.caption(),
        indexed_season_date(&deadly),
        position.caption(),
        indexed_season_date(&shiyu)
    );
    let media = vec![
        InputMedia::Photo(
            InputMediaPhoto::new(InputFile::memory(deadly_png))
                .caption(album_caption)
                .parse_mode(ParseMode::Html),
        ),
        InputMedia::Photo(InputMediaPhoto::new(InputFile::memory(shiyu_png))),
    ];
    bot.send_media_group(chat_id, media).await?;
    Ok(())
}

pub fn is_missing_detail_command(msg: Message) -> bool {
    missing_detail_kind(msg.text()).is_some()
}

fn missing_detail_kind(text: Option<&str>) -> Option<&'static str> {
    let command = text?.split_whitespace().next()?;
    if text?.split_whitespace().count() != 1 {
        return None;
    }
    let command = command.split('@').next().unwrap_or(command);
    match command.to_ascii_lowercase().as_str() {
        "/da" => Some("da"),
        "/shiyu" => Some("sd"),
        _ => None,
    }
}

pub async fn missing_detail_command_handler(
    bot: Bot,
    msg: Message,
    state: Arc<BotState>,
) -> anyhow::Result<()> {
    let Some(kind) = missing_detail_kind(msg.text()) else {
        return Ok(());
    };
    send_uid_choice(&bot, msg.chat.id, kind, &state).await?;
    let _ = bot.delete_message(msg.chat.id, msg.id).await;
    Ok(())
}

pub async fn callback_handler(
    bot: Bot,
    query: CallbackQuery,
    state: Arc<BotState>,
) -> anyhow::Result<()> {
    let Some(data) = query.data.as_deref() else {
        bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    let Some((kind, uid)) = parse_detail_callback(data) else {
        bot.answer_callback_query(query.id)
            .text("Некорректная кнопка")
            .show_alert(true)
            .await?;
        return Ok(());
    };
    let Some(message) = query.message.as_ref() else {
        bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    let chat_id = message.chat().id;
    if state.db.get_user(chat_id.0, uid).await?.is_none() {
        bot.answer_callback_query(query.id)
            .text("Этот UID больше не зарегистрирован в чате")
            .show_alert(true)
            .await?;
        return Ok(());
    }
    bot.answer_callback_query(query.id).await?;
    send_detail(&bot, chat_id, kind, uid, &state).await
}

// ── /start ──

async fn cmd_start(bot: &Bot, chat_id: ChatId, state: &BotState) -> anyhow::Result<()> {
    BotTemplateSender::new(bot, &state.templates)
        .send_message(chat_id, "welcome", &())
        .await?;
    Ok(())
}

// ── /register <uid> ──

async fn cmd_register(bot: &Bot, msg: Message, uid: &str, state: &BotState) -> anyhow::Result<()> {
    let chat_id = msg.chat.id;
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let uid = uid.trim();

    // Validate UID format (should be digits, preferably starting with 15 for EU)
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(
                chat_id,
                "register_invalid",
                &serde_json::json!({ "uid": uid }),
            )
            .await?;
        return Ok(());
    }

    // Check if already registered in this chat
    if state.db.get_user(chat_id.0, uid).await?.is_some() {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(
                chat_id,
                "register_already",
                &serde_json::json!({ "uid": uid }),
            )
            .await?;
        return Ok(());
    }

    // Try to verify the UID by fetching data from HoYoLAB
    let verification = verify_uid(state, uid).await;

    match verification {
        Ok(nickname) => {
            state
                .db
                .add_user(chat_id.0, user_id, uid, nickname.as_deref())
                .await?;

            let display_name = nickname.as_deref().unwrap_or(uid);
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "register_success",
                    &serde_json::json!({ "uid": uid, "display_name": display_name }),
                )
                .await?;
        }
        Err(VerificationError::DataNotPublic) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "register_not_public",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
        Err(VerificationError::Other(e)) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "register_error",
                    &serde_json::json!({ "uid": uid, "error": e.to_string() }),
                )
                .await?;
        }
    }

    Ok(())
}

// ── /unregister <uid> ──

async fn cmd_unregister(
    bot: &Bot,
    chat_id: ChatId,
    uid: &str,
    state: &BotState,
) -> anyhow::Result<()> {
    let uid = uid.trim();
    match state.db.remove_user(chat_id.0, uid).await? {
        true => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "unregister_success",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
        false => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "unregister_not_found",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
    }
    Ok(())
}

// ── /status ──

async fn cmd_status(bot: &Bot, chat_id: ChatId, state: &BotState) -> anyhow::Result<()> {
    let tracked = state.db.get_all_users().await?.len();
    let (deadly_index, shiyu_index) =
        tokio::try_join!(state.nanoka.get_boss_seasons(), state.nanoka.get_seasons())?;
    let now = Utc::now();
    let active_seasons = [
        (
            "Deadly Assault",
            select_indexed_season(
                &deadly_index,
                EndgameType::DeadlyAssault,
                9,
                SeasonPosition::Current,
                now,
            ),
        ),
        (
            "Shiyu Defense",
            select_indexed_season(
                &shiyu_index,
                EndgameType::ShiyuDefence,
                1,
                SeasonPosition::Current,
                now,
            ),
        ),
    ];
    let seasons = active_seasons
        .iter()
        .map(|(name, season)| {
            serde_json::json!({
            "name": name,
            "active": season.is_some(),
            "start": season.as_ref().map(|season| season.starts_at.to_rfc3339()).unwrap_or_else(|| "—".to_owned()),
            "end": season.as_ref().and_then(|season| season.ends_at.map(|end| end.to_rfc3339())).unwrap_or_else(|| "—".to_owned()),
            "tracked": tracked,
            })
        })
        .collect::<Vec<_>>();

    let rendered = state
        .templates
        .render("status", &serde_json::json!({ "seasons": seasons }))?;
    BotTemplateSender::new(bot, &state.templates)
        .send_rendered_message(chat_id, rendered)
        .await?;

    Ok(())
}

// ── /top_da, /top_sd ──

async fn cmd_top(
    bot: &Bot,
    chat_id: ChatId,
    endgame_type: &str,
    state: &BotState,
) -> anyhow::Result<()> {
    let (png, caption) =
        build_top_image_and_caption(state, chat_id.0, endgame_type, "manual").await?;
    if png.is_empty() {
        BotTemplateSender::new(bot, &state.templates)
            .send_rendered_message(chat_id, caption)
            .await?;
    } else {
        BotTemplateSender::new(bot, &state.templates)
            .send_photo_with_rendered(chat_id, InputFile::memory(png), caption)
            .await?;
    }
    Ok(())
}

// ── /cookie <new_cookie> (admin only) ──

async fn cmd_cookie(bot: &Bot, msg: Message, cookie: &str, state: &BotState) -> anyhow::Result<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = msg.chat.id;

    if user_id != state.admin_id {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "not_admin", &())
            .await?;
        return Ok(());
    }

    // Validate cookie by trying to create a client
    match rustverse::client::zzz::ZZZClient::from_cookie_string(cookie.trim()) {
        Ok(client) => match client.get_game_record_cards().await {
            Ok(cards) => {
                state.db.set_cookie(cookie.trim()).await?;

                let game_list: Vec<String> = cards
                    .iter()
                    .map(|c| {
                        let game = match c.game_id {
                            2 => "Genshin",
                            6 => "HSR",
                            8 => "ZZZ",
                            _ => "?",
                        };
                        format!("[{game}] {} (UID {})", c.nickname, c.game_role_id)
                    })
                    .collect();

                let status = if game_list.is_empty() {
                    "OK (нет привязанных игр)".to_string()
                } else {
                    format!("OK\nПривязанные игры:\n{}", game_list.join("\n"))
                };

                BotTemplateSender::new(bot, &state.templates)
                    .send_message(
                        chat_id,
                        "cookie_updated",
                        &serde_json::json!({ "status": status }),
                    )
                    .await?;
            }
            Err(e) => {
                BotTemplateSender::new(bot, &state.templates)
                    .send_message(
                        chat_id,
                        "cookie_invalid",
                        &serde_json::json!({ "error": e.to_string() }),
                    )
                    .await?;
            }
        },
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "cookie_invalid",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await?;
        }
    }

    Ok(())
}

// ── /refetch_all, /refetch <uid> (admin only) ──

async fn cmd_refetch_all(bot: &Bot, msg: Message, state: &BotState) -> anyhow::Result<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = msg.chat.id;

    if user_id != state.admin_id {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "not_admin", &())
            .await?;
        return Ok(());
    }

    BotTemplateSender::new(bot, &state.templates)
        .send_message(chat_id, "refetch_all_start", &())
        .await?;

    let users = state.db.get_all_users().await?;
    if users.is_empty() {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "refetch_all_empty", &())
            .await?;
        return Ok(());
    }

    let cookie = match state.db.get_cookie().await? {
        Some(c) => c,
        None => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(chat_id, "no_cookie", &())
                .await?;
            return Ok(());
        }
    };

    match scheduler::fetch_all_users(&cookie, state).await {
        Ok((ok, err)) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "refetch_all_result",
                    &serde_json::json!({ "ok": ok, "err": err }),
                )
                .await?;
        }
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "refetch_all_error",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await?;
        }
    }

    Ok(())
}

async fn cmd_refetch_uid(
    bot: &Bot,
    msg: Message,
    uid: &str,
    state: &BotState,
) -> anyhow::Result<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let chat_id = msg.chat.id;

    if user_id != state.admin_id {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "not_admin", &())
            .await?;
        return Ok(());
    }

    let uid = uid.trim();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "invalid_uid", &serde_json::json!({ "uid": uid }))
            .await?;
        return Ok(());
    }

    BotTemplateSender::new(bot, &state.templates)
        .send_message(
            chat_id,
            "refetch_uid_start",
            &serde_json::json!({ "uid": uid }),
        )
        .await?;

    let cookie = match state.db.get_cookie().await? {
        Some(c) => c,
        None => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(chat_id, "no_cookie", &())
                .await?;
            return Ok(());
        }
    };

    match scheduler::fetch_single_user(&cookie, uid, state).await {
        Ok(true) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "refetch_uid_success",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
        Ok(false) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "refetch_uid_not_public",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "refetch_uid_error",
                    &serde_json::json!({ "uid": uid, "error": e.to_string() }),
                )
                .await?;
        }
    }

    Ok(())
}

// ── /da <uid>, /shiyu <uid> ──

async fn cmd_detail_command(
    bot: &Bot,
    chat_id: ChatId,
    kind: &str,
    input: Option<&str>,
    state: &BotState,
) -> anyhow::Result<()> {
    let Some(input) = input.filter(|value| !value.trim().is_empty()) else {
        return send_uid_choice(bot, chat_id, kind, state).await;
    };

    let uid = match resolve_uid(state, chat_id, input).await? {
        Ok(uid) => uid,
        Err(rendered) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_rendered_message(chat_id, rendered)
                .await?;
            return Ok(());
        }
    };
    send_detail(bot, chat_id, kind, &uid, state).await
}

async fn send_detail(
    bot: &Bot,
    chat_id: ChatId,
    kind: &str,
    uid: &str,
    state: &BotState,
) -> anyhow::Result<()> {
    match kind {
        "da" => send_da(bot, chat_id, uid, state).await,
        "sd" => send_shiyu(bot, chat_id, uid, state).await,
        _ => anyhow::bail!("unknown detail kind"),
    }
}

async fn send_da(bot: &Bot, chat_id: ChatId, uid: &str, state: &BotState) -> anyhow::Result<()> {
    // Try DB first
    if let Some(json) = state
        .db
        .get_latest_result_json(uid, "deadly_assault")
        .await?
        && let Ok(data) = serde_json::from_str::<rustverse::models::zzz::ZZZDeadlyAssault>(&json)
        && data.has_data.unwrap_or(true)
    {
        let nick = resolve_nickname(state, uid, data.nick_name.as_deref()).await?;
        let png = render_da(data).await?;
        send_detail_photo(
            bot,
            state,
            chat_id,
            InputFile::memory(png),
            "da",
            &nick,
            uid,
        )
        .await?;
        return Ok(());
    }

    // Fallback: live API
    let cookie = match state.db.get_cookie().await? {
        Some(c) => c,
        None => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(chat_id, "no_cookie", &())
                .await?;
            return Ok(());
        }
    };

    let client = match rustverse::client::zzz::ZZZClient::from_cookie_string(&cookie) {
        Ok(c) => c,
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "error_generic",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await?;
            return Ok(());
        }
    };

    log::info!("Cache miss: /da {uid}");
    let _ = notify_cache_miss(bot, "da", uid, state).await;

    match client.get_deadly_assault(uid, None, "1").await {
        Ok(data) => {
            let nick = resolve_nickname(state, uid, data.nick_name.as_deref()).await?;
            let png = render_da(data).await?;
            send_detail_photo(
                bot,
                state,
                chat_id,
                InputFile::memory(png),
                "da",
                &nick,
                uid,
            )
            .await?;
        }
        Err(rustverse::error::HoyoverseError::DataNotPublic) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "data_not_public",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "error_generic",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await?;
        }
    }

    Ok(())
}

async fn send_shiyu(bot: &Bot, chat_id: ChatId, uid: &str, state: &BotState) -> anyhow::Result<()> {
    // Try DB first
    if let Some(json) = state
        .db
        .get_latest_result_json(uid, "shiyu_defense")
        .await?
        && let Ok(data) = serde_json::from_str::<rustverse::models::zzz::ZZZShiyuDefense>(&json)
        && data.hadal_begin_time.is_some()
    {
        let nick = resolve_nickname(state, uid, None).await?;
        let png = render_shiyu(data).await?;
        send_detail_photo(
            bot,
            state,
            chat_id,
            InputFile::memory(png),
            "sd",
            &nick,
            uid,
        )
        .await?;
        return Ok(());
    }

    // Fallback: live API
    let cookie = match state.db.get_cookie().await? {
        Some(c) => c,
        None => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(chat_id, "no_cookie", &())
                .await?;
            return Ok(());
        }
    };

    let client = match rustverse::client::zzz::ZZZClient::from_cookie_string(&cookie) {
        Ok(c) => c,
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "error_generic",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await?;
            return Ok(());
        }
    };

    log::info!("Cache miss: /shiyu {uid}");
    let _ = notify_cache_miss(bot, "shiyu", uid, state).await;

    match client.get_shiyu_defense(uid, None, "1").await {
        Ok(data) => {
            let nick = resolve_nickname(state, uid, None).await?;
            let png = render_shiyu(data).await?;
            send_detail_photo(
                bot,
                state,
                chat_id,
                InputFile::memory(png),
                "sd",
                &nick,
                uid,
            )
            .await?;
        }
        Err(rustverse::error::HoyoverseError::DataNotPublic) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "data_not_public",
                    &serde_json::json!({ "uid": uid }),
                )
                .await?;
        }
        Err(e) => {
            BotTemplateSender::new(bot, &state.templates)
                .send_message(
                    chat_id,
                    "error_generic",
                    &serde_json::json!({ "error": e.to_string() }),
                )
                .await?;
        }
    }

    Ok(())
}

async fn render_da(data: rustverse::models::zzz::ZZZDeadlyAssault) -> anyhow::Result<Vec<u8>> {
    rustverse_svg::preload_da_images(&data).await?;
    tokio::task::spawn_blocking(move || rustverse_svg::da(&data))
        .await
        .map_err(|error| anyhow::anyhow!("Deadly Assault renderer task panicked: {error}"))
}

async fn render_shiyu(data: rustverse::models::zzz::ZZZShiyuDefense) -> anyhow::Result<Vec<u8>> {
    rustverse_svg::preload_shiyu_images(&data).await?;
    tokio::task::spawn_blocking(move || rustverse_svg::shiyu(&data))
        .await
        .map_err(|error| anyhow::anyhow!("Shiyu Defense renderer task panicked: {error}"))
}

fn parse_detail_callback(data: &str) -> Option<(&str, &str)> {
    let mut parts = data.split(':');
    if parts.next()? != "detail" {
        return None;
    }
    let kind = parts.next()?;
    let uid = parts.next()?;
    if parts.next().is_some()
        || !matches!(kind, "da" | "sd")
        || !(6..=12).contains(&uid.len())
        || !uid.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((kind, uid))
}

fn uid_choice_keyboard(
    users: &[crate::db::UserRow],
    kind: &str,
) -> anyhow::Result<InlineKeyboardMarkup> {
    if !matches!(kind, "da" | "sd") {
        anyhow::bail!("unknown detail kind");
    }
    let rows = users
        .iter()
        .map(|user| {
            let label = user.nickname.as_deref().unwrap_or(&user.uid);
            vec![InlineKeyboardButton::callback(
                label,
                format!("detail:{kind}:{}", user.uid),
            )]
        })
        .collect::<Vec<_>>();
    Ok(InlineKeyboardMarkup::new(rows))
}

async fn send_uid_choice(
    bot: &Bot,
    chat_id: ChatId,
    kind: &str,
    state: &BotState,
) -> anyhow::Result<()> {
    let users = state.db.get_users_by_chat(chat_id.0).await?;
    if users.is_empty() {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "uids_empty", &())
            .await?;
        return Ok(());
    }
    let event = if kind == "da" {
        "Deadly Assault"
    } else {
        "Shiyu Defense"
    };
    BotTemplateSender::new(bot, &state.templates)
        .send_message_with_keyboard(
            chat_id,
            "choose_uid",
            &serde_json::json!({ "event": event }),
            uid_choice_keyboard(&users, kind)?,
        )
        .await?;
    Ok(())
}

async fn send_detail_photo(
    bot: &Bot,
    state: &BotState,
    chat_id: ChatId,
    photo: InputFile,
    kind: &str,
    nickname: &str,
    uid: &str,
) -> anyhow::Result<()> {
    let endgame_type = if kind == "da" {
        "deadly_assault"
    } else {
        "shiyu_defense"
    };
    let event = if kind == "da" {
        "Deadly Assault"
    } else {
        "Shiyu Defense"
    };
    let comparison = season_comparison(state, uid, endgame_type).await?;
    let data = serde_json::json!({
        "event": event,
        "nickname": nickname,
        "uid": uid,
        "comparison": comparison,
    });
    let sender = BotTemplateSender::new(bot, &state.templates);
    if let Some(base) = &state.public_web_url {
        let mut url = url::Url::parse(base)?;
        url.set_path("/");
        url.query_pairs_mut()
            .append_pair("chat", &chat_id.0.to_string())
            .append_pair("uid", uid)
            .append_pair("kind", endgame_type);
        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::url(
            "История и график",
            url,
        )]]);
        sender
            .send_photo_with_keyboard(chat_id, photo, "detail_caption", &data, keyboard)
            .await?;
    } else {
        sender
            .send_photo(chat_id, photo, "detail_caption", &data)
            .await?;
    }
    Ok(())
}

async fn season_comparison(
    state: &BotState,
    uid: &str,
    endgame_type: &str,
) -> anyhow::Result<Option<String>> {
    let seasons = state
        .db
        .get_current_and_previous_result(uid, endgame_type)
        .await?;
    Ok(format_season_comparison(endgame_type, &seasons))
}

fn format_signed(value: i64) -> String {
    if value > 0 {
        format!("+{value}")
    } else {
        value.to_string()
    }
}

fn format_season_comparison(
    endgame_type: &str,
    seasons: &[crate::db::SeasonResult],
) -> Option<String> {
    let [current, previous, ..] = seasons else {
        return None;
    };
    let mut parts = vec![format!(
        "очки {}",
        format_signed(current.total_score - previous.total_score)
    )];
    let current_json: serde_json::Value = serde_json::from_str(&current.data_json).ok()?;
    let previous_json: serde_json::Value = serde_json::from_str(&previous.data_json).ok()?;
    match endgame_type {
        "deadly_assault" => {
            let current_stars = current_json.get("total_star")?.as_i64()?;
            let previous_stars = previous_json.get("total_star")?.as_i64()?;
            parts.push(format!(
                "звёзды {}",
                format_signed(current_stars - previous_stars)
            ));
        }
        "shiyu_defense" => {
            let current_layers = current_json
                .pointer("/brief/cur_period_zone_layer_count")
                .and_then(serde_json::Value::as_i64);
            let previous_layers = previous_json
                .pointer("/brief/cur_period_zone_layer_count")
                .and_then(serde_json::Value::as_i64);
            if let (Some(current_layers), Some(previous_layers)) = (current_layers, previous_layers)
            {
                parts.push(format!(
                    "этажи {}",
                    format_signed(current_layers - previous_layers)
                ));
            }
        }
        _ => return None,
    }
    Some(parts.join(", "))
}

// ── /uids ──

async fn cmd_uids(bot: &Bot, chat_id: ChatId, state: &BotState) -> anyhow::Result<()> {
    let users = state.db.get_users_by_chat(chat_id.0).await?;

    if users.is_empty() {
        BotTemplateSender::new(bot, &state.templates)
            .send_message(chat_id, "uids_empty", &())
            .await?;
        return Ok(());
    }

    let uids: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            serde_json::json!({
                "uid": u.uid,
                "nickname": u.nickname.as_deref().unwrap_or("—"),
            })
        })
        .collect();

    BotTemplateSender::new(bot, &state.templates)
        .send_message(chat_id, "uids_list", &serde_json::json!({ "users": uids }))
        .await?;

    Ok(())
}

// ── Template data helpers ──

fn fmt_agent(av: &rustverse::models::zzz::ZZZAvatar) -> String {
    let name = rustverse::client::agent_cache::resolve_name(av.id);
    let show_rank = match av.rarity.as_str() {
        "S" => av.rank != 0,
        "A" => av.rank != 6,
        _ => true,
    };
    if show_rank {
        format!(
            "{name} ({rarity}, lv{lv}, M{rank})",
            name = name,
            rarity = av.rarity,
            lv = av.level,
            rank = av.rank
        )
    } else {
        format!(
            "{name} ({rarity}, lv{lv})",
            name = name,
            rarity = av.rarity,
            lv = av.level
        )
    }
}

/// Convert Deadly Assault API data into template variables for `da_detail`.
#[allow(dead_code)]
fn prepare_da_data(data: &rustverse::models::zzz::ZZZDeadlyAssault) -> serde_json::Value {
    let rooms: Vec<serde_json::Value> = data
        .list
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let boss_name = room.boss.first().map(|b| b.name.as_str()).unwrap_or("?");
            let buffs: Vec<&str> = room
                .buffer
                .iter()
                .filter_map(|b| b.title.as_deref())
                .collect();
            let buffs_str = if buffs.is_empty() {
                None
            } else {
                Some(buffs.join(", "))
            };
            let agents_str: String = room
                .avatar_list
                .iter()
                .map(fmt_agent)
                .collect::<Vec<_>>()
                .join(", ");

            serde_json::json!({
                "index": i + 1,
                "boss_name": boss_name,
                "star": room.star.unwrap_or(0),
                "score": room.score.unwrap_or(0),
                "buffs": buffs_str,
                "agents": agents_str,
            })
        })
        .collect();

    let rank_percent = data.rank_percent.map(|rp| format!("{:.2}", rp / 100.0));

    serde_json::json!({
        "nick_name": data.nick_name.as_deref().unwrap_or("?"),
        "total_star": data.total_star.unwrap_or(0),
        "total_score": data.total_score.unwrap_or(0),
        "rank_percent": rank_percent,
        "rooms": rooms,
    })
}

/// Convert Shiyu Defense API data into template variables for `shiyu_detail`.
#[allow(dead_code)]
fn prepare_shiyu_data(
    data: &rustverse::models::zzz::ZZZShiyuDefense,
    nick: &str,
) -> serde_json::Value {
    let score = data.brief.as_ref().and_then(|b| b.score);
    let max_score = data.brief.as_ref().and_then(|b| b.max_score);
    let rating = data.brief.as_ref().and_then(|b| b.rating.as_deref());
    let rank_percent = data
        .brief
        .as_ref()
        .and_then(|b| b.rank_percent)
        .map(|rp| format!("{:.2}", rp as f64 / 100.0));

    let rooms: Vec<serde_json::Value> = data
        .layers
        .get("fifth_layer_detail")
        .or_else(|| data.layers.get("fitfh_layer_detail"))
        .map(|layer| {
            layer
                .layer_challenge_info_list
                .iter()
                .enumerate()
                .map(|(i, ch)| {
                    let agents_str: Option<String> = if ch.avatar_list.is_empty() {
                        None
                    } else {
                        Some(
                            ch.avatar_list
                                .iter()
                                .map(fmt_agent)
                                .collect::<Vec<_>>()
                                .join(", "),
                        )
                    };
                    let buff = ch.buffer.as_ref().and_then(|b| b.title.as_deref());

                    serde_json::json!({
                        "index": i + 1,
                        "score": ch.score.unwrap_or(0),
                        "max_score": ch.max_score.unwrap_or(0),
                        "rating": ch.rating.as_deref().unwrap_or("?"),
                        "agents": agents_str,
                        "buff": buff,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    serde_json::json!({
        "nick_name": nick,
        "score": score,
        "max_score": max_score,
        "rating": rating,
        "rank_percent": rank_percent,
        "rooms": rooms,
    })
}

// ── Helpers ──

async fn notify_cache_miss(bot: &Bot, cmd: &str, uid: &str, state: &BotState) {
    let _ = BotTemplateSender::new(bot, &state.templates)
        .send_message(
            ChatId(state.admin_id),
            "cache_miss",
            &serde_json::json!({ "cmd": cmd, "uid": uid }),
        )
        .await;
}

/// Resolve a display nickname for a UID.
///
/// Check `nick_name`, the users table, and stored Deadly Assault results.
/// Use the UID if these sources do not contain a nickname.
async fn resolve_nickname(
    state: &BotState,
    uid: &str,
    api_nick: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(n) = api_nick.filter(|n| !n.is_empty()) {
        return Ok(n.to_string());
    }
    if let Some(n) = state.db.get_nickname_for_uid(uid).await? {
        return Ok(n);
    }
    // Try DA result_json
    if let Some(json) = state
        .db
        .get_latest_result_json(uid, "deadly_assault")
        .await?
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&json)
        && let Some(n) = v.get("nick_name").and_then(|s| s.as_str())
        && !n.is_empty()
    {
        return Ok(n.to_string());
    }
    Ok(uid.to_string())
}

/// Resolve user input to a UID.
///
/// Accept a numeric UID or find a nickname in the specified chat.
/// The result contains the UID or an error message.
async fn resolve_uid(
    state: &BotState,
    chat_id: ChatId,
    input: &str,
) -> anyhow::Result<Result<String, RenderedTemplate>> {
    let input = input.trim();

    // Numeric UID
    if !input.is_empty() && input.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Ok(input.to_string()));
    }

    // Try nickname lookup
    if let Some(uid) = state.db.find_uid_by_nickname(chat_id.0, input).await? {
        return Ok(Ok(uid));
    }

    Ok(Err(state
        .templates
        .render("invalid_uid", &serde_json::json!({ "uid": input }))
        .unwrap_or_else(RenderedTemplate::error)))
}

enum VerificationError {
    DataNotPublic,
    Other(anyhow::Error),
}

/// Fetch data to verify that a UID is valid and public.
///
/// Return the Deadly Assault `nick_name` value after a successful request.
async fn verify_uid(state: &BotState, uid: &str) -> Result<Option<String>, VerificationError> {
    let cookie = state
        .db
        .get_cookie()
        .await
        .map_err(VerificationError::Other)?
        .ok_or_else(|| VerificationError::Other(anyhow::anyhow!("Cookie not set")))?;

    let client = rustverse::client::zzz::ZZZClient::from_cookie_string(&cookie)
        .map_err(|e| VerificationError::Other(e.into()))?;

    // Primary: Deadly Assault — returns `nick_name` (real player nickname)
    match client.get_deadly_assault(uid, None, "1").await {
        Ok(data) => {
            return Ok(data.nick_name);
        }
        Err(rustverse::error::HoyoverseError::DataNotPublic) => {
            return Err(VerificationError::DataNotPublic);
        }
        Err(e) => {
            log::debug!("Deadly Assault check failed for {uid}: {e}, trying Shiyu...");
        }
    }

    // Fallback: Shiyu Defense — just checks data accessibility, no nickname
    match client.get_shiyu_defense(uid, None, "1").await {
        Ok(_) => {
            Ok(None) // data is public, but no nickname available
        }
        Err(rustverse::error::HoyoverseError::DataNotPublic) => {
            Err(VerificationError::DataNotPublic)
        }
        Err(e) => {
            log::debug!("Shiyu check failed for {uid}: {e}");
            Err(VerificationError::Other(anyhow::anyhow!(
                "Не удалось проверить UID: {e}. Возможно, нет активного сезона. Попробуйте позже."
            )))
        }
    }
}

/// Build a top leaderboard message for the given endgame type.
/// Build the PNG image and caption for a leaderboard.
pub async fn build_top_image_and_caption(
    state: &BotState,
    chat_id: i64,
    endgame_type: &str,
    checkpoint: &str,
) -> anyhow::Result<(Vec<u8>, RenderedTemplate)> {
    let event_name = endgame_name(endgame_type);

    let season_start = match state.db.get_latest_season_start(endgame_type).await? {
        Some(s) => s,
        None => {
            let text = state
                .templates
                .render("top_empty", &())
                .unwrap_or_else(RenderedTemplate::error);
            return Ok((Vec::new(), text));
        }
    };

    let checkpoint_label = match checkpoint {
        "6h" => "6 часов после начала",
        "24h" => "24 часа после начала",
        "14d" => "14 дней (итог)",
        _ => "текущий топ",
    };

    let entries = state
        .db
        .get_latest_results(chat_id, endgame_type, &season_start)
        .await?;

    if entries.is_empty() {
        let text = state
            .templates
            .render("top_empty", &())
            .unwrap_or_else(RenderedTemplate::error);
        return Ok((Vec::new(), text));
    }

    let season_end = &entries[0].season_end;

    let caption = state
        .templates
        .render(
            "top_header",
            &serde_json::json!({
                "name": event_name,
                "checkpoint_label": checkpoint_label,
                "start": season_start,
                "end": season_end,
            }),
        )
        .unwrap_or_else(RenderedTemplate::error);

    let png = match endgame_type {
        "deadly_assault" => {
            let mut top: Vec<rustverse_svg::TopDAItem> = entries
                .iter()
                .filter_map(|e| {
                    let data: rustverse::models::zzz::ZZZDeadlyAssault =
                        serde_json::from_str(&e.data_json).ok()?;
                    let stars = data.total_star? as u8;
                    let nickname = e.nickname.clone().unwrap_or_else(|| e.uid.clone());
                    let normal_score = u32::try_from(data.normal_score()).ok()?;
                    let hard_score = u32::try_from(data.hard_score()).ok()?;
                    Some(rustverse_svg::TopDAItem {
                        nickname,
                        stars,
                        total_score: normal_score.saturating_add(hard_score),
                        normal_score,
                        hard_score,
                    })
                })
                .collect();
            top.sort_by_key(|item| std::cmp::Reverse(item.total_score));
            rustverse_svg::top_da(&rustverse_svg::TopDA { top })
        }
        "shiyu_defense" => {
            let top: Vec<rustverse_svg::TopShiyuItem> = entries
                .iter()
                .filter_map(|e| {
                    let v: serde_json::Value = serde_json::from_str(&e.data_json).ok()?;
                    let rating = v.get("brief")?.get("rating")?.as_str()?.to_string();
                    let nickname = e.nickname.clone().unwrap_or_else(|| e.uid.clone());
                    Some(rustverse_svg::TopShiyuItem {
                        nickname,
                        rating,
                        score: e.total_score as u32,
                    })
                })
                .collect();
            rustverse_svg::top_shiyu(&rustverse_svg::TopShiyu { top })
        }
        _ => return Ok((Vec::new(), caption)),
    };

    Ok((png, caption))
}

/// Legacy text-only version kept for compatibility.
#[allow(dead_code)]
pub async fn build_top_message(
    state: &BotState,
    chat_id: i64,
    endgame_type: &str,
    checkpoint: &str,
) -> anyhow::Result<String> {
    let event_name = endgame_name(endgame_type);

    // Get the latest season start from DB
    let season_start = match state.db.get_latest_season_start(endgame_type).await? {
        Some(s) => s,
        None => {
            return Ok(state
                .templates
                .render("top_empty", &())
                .map(|rendered| rendered.text)
                .unwrap_or_else(|e| format!("Error: {e}")));
        }
    };

    let checkpoint_label = match checkpoint {
        "6h" => "6 часов после начала",
        "24h" => "24 часа после начала",
        "14d" => "14 дней (итог)",
        _ => "текущий топ",
    };

    let entries = state
        .db
        .get_latest_results(chat_id, endgame_type, &season_start)
        .await?;

    if entries.is_empty() {
        return Ok(state
            .templates
            .render("top_empty", &())
            .map(|rendered| rendered.text)
            .unwrap_or_else(|e| format!("Error: {e}")));
    }

    // Use the season_end from DB (already formatted as "YYYY-MM-DD HH:MM:SS")
    let season_end = &entries[0].season_end;

    let header = state
        .templates
        .render(
            "top_header",
            &serde_json::json!({
                "name": event_name,
                "checkpoint_label": checkpoint_label,
                "start": season_start,
                "end": season_end,
            }),
        )
        .map(|rendered| rendered.text)
        .unwrap_or_default();

    let mut body = String::new();
    for (i, entry) in entries.iter().enumerate() {
        let display_name = entry.nickname.as_deref().unwrap_or(&entry.uid);
        // Format score
        let score_str = format!("{} очков", entry.total_score);
        // Try to extract extra info from data_json
        let extra = extract_extra(endgame_type, &entry.data_json);

        let line = state
            .templates
            .render(
                "top_entry",
                &serde_json::json!({
                    "position": i + 1,
                    "display_name": display_name,
                    "score_str": score_str,
                    "extra": extra,
                }),
            )
            .map(|rendered| rendered.text)
            .unwrap_or_default();
        body.push_str(&line);
    }

    let footer = state
        .templates
        .render("top_footer", &())
        .map(|rendered| rendered.text)
        .unwrap_or_default();

    Ok(format!("{header}\n{body}{footer}"))
}

fn extract_extra(endgame_type: &str, data_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(data_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match endgame_type {
        "deadly_assault" => {
            let stars = v.get("total_star").and_then(|s| s.as_i64()).unwrap_or(0);
            let rp = v
                .get("rank_percent")
                .and_then(|s| s.as_f64())
                .unwrap_or(0.0);
            format!("{}★ | Топ {:.2}%", stars, rp / 100.0)
        }
        "shiyu_defense" => {
            let rating = v
                .get("brief")
                .and_then(|b| b.get("rating"))
                .and_then(|r| r.as_str())
                .unwrap_or("?");
            let rp = v
                .get("brief")
                .and_then(|b| b.get("rank_percent"))
                .and_then(|r| r.as_i64())
                .unwrap_or(0) as f64
                / 100.0;
            format!("Ранг {} | Топ {:.2}%", rating, rp)
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn season_meta(sort: u32, begin: &str, end: &str) -> SeasonMeta {
        SeasonMeta {
            sort,
            en: String::new(),
            ko: String::new(),
            zh: String::new(),
            ja: String::new(),
            begin: Some(begin.to_owned()),
            end: Some(end.to_owned()),
            live_begin: None,
            live_end: None,
        }
    }

    #[test]
    fn selects_previous_current_and_next_indexed_seasons() {
        let seasons = HashMap::from([
            (
                "69031".to_owned(),
                season_meta(9, "2026-06-01 04:00:00", "2026-06-15 04:00:00"),
            ),
            (
                "69041".to_owned(),
                season_meta(9, "2026-06-15 04:00:00", "2026-06-29 04:00:00"),
            ),
            (
                "69051".to_owned(),
                season_meta(9, "2026-06-29 04:00:00", "2026-07-13 04:00:00"),
            ),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap();

        let selected = |position| {
            select_indexed_season(&seasons, EndgameType::DeadlyAssault, 9, position, now)
                .unwrap()
                .id
        };
        assert_eq!(selected(SeasonPosition::Previous), 69031);
        assert_eq!(selected(SeasonPosition::Current), 69041);
        assert_eq!(selected(SeasonPosition::Next), 69051);
    }

    #[test]
    fn next_season_is_absent_when_current_is_last_published() {
        let seasons = HashMap::from([
            (
                "69031".to_owned(),
                season_meta(9, "2026-06-01 04:00:00", "2026-06-15 04:00:00"),
            ),
            (
                "69041".to_owned(),
                season_meta(9, "2026-06-15 04:00:00", "2026-06-29 04:00:00"),
            ),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap();

        assert!(
            select_indexed_season(
                &seasons,
                EndgameType::DeadlyAssault,
                9,
                SeasonPosition::Next,
                now,
            )
            .is_none()
        );
    }

    #[test]
    fn next_season_uses_the_immediate_test_server_preview() {
        let seasons = HashMap::from([
            (
                "69040".to_owned(),
                season_meta(9, "2026-07-03 04:00:00", "2026-07-17 04:00:00"),
            ),
            (
                "69041".to_owned(),
                season_meta(9, "2026-07-17 04:00:00", "2026-07-29 04:00:00"),
            ),
            (
                "690421".to_owned(),
                season_meta(9, "2026-06-10 04:00:00", "2026-08-19 03:59:59"),
            ),
            (
                "690431".to_owned(),
                season_meta(9, "2026-06-10 04:00:00", "2026-08-19 03:59:59"),
            ),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap();

        assert_eq!(
            select_indexed_season(
                &seasons,
                EndgameType::DeadlyAssault,
                9,
                SeasonPosition::Next,
                now,
            )
            .unwrap()
            .id,
            690421
        );
    }

    #[test]
    fn next_season_does_not_skip_a_missing_test_preview() {
        let seasons = HashMap::from([
            (
                "69041".to_owned(),
                season_meta(9, "2026-07-17 04:00:00", "2026-07-29 04:00:00"),
            ),
            (
                "690431".to_owned(),
                season_meta(9, "2026-06-10 04:00:00", "2026-08-19 03:59:59"),
            ),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap();

        assert!(
            select_indexed_season(
                &seasons,
                EndgameType::DeadlyAssault,
                9,
                SeasonPosition::Next,
                now,
            )
            .is_none()
        );
    }

    #[test]
    fn six_digit_season_becomes_current_after_production_season_ends() {
        let seasons = HashMap::from([
            (
                "69041".to_owned(),
                season_meta(9, "2026-07-17 04:00:00", "2026-07-29 03:59:59"),
            ),
            (
                "690421".to_owned(),
                season_meta(9, "2026-07-29 04:00:00", "2026-08-14 03:59:59"),
            ),
            (
                "690431".to_owned(),
                season_meta(9, "2026-08-14 04:00:00", "2026-08-28 03:59:59"),
            ),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 6, 0, 0).unwrap();

        let selected = |position| {
            select_indexed_season(&seasons, EndgameType::DeadlyAssault, 9, position, now)
                .unwrap()
                .id
        };
        assert_eq!(selected(SeasonPosition::Previous), 69041);
        assert_eq!(selected(SeasonPosition::Current), 690421);
        assert_eq!(selected(SeasonPosition::Next), 690431);
    }

    #[test]
    fn broad_six_digit_preview_does_not_replace_active_production_season() {
        let seasons = HashMap::from([
            (
                "69041".to_owned(),
                season_meta(9, "2026-07-17 04:00:00", "2026-07-29 03:59:59"),
            ),
            (
                "690421".to_owned(),
                season_meta(9, "2026-06-10 04:00:00", "2026-08-19 03:59:59"),
            ),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap();

        assert_eq!(
            select_indexed_season(
                &seasons,
                EndgameType::DeadlyAssault,
                9,
                SeasonPosition::Current,
                now,
            )
            .unwrap()
            .id,
            69041
        );
    }

    #[test]
    fn callback_data_is_strict_and_does_not_carry_a_chat_id() {
        assert_eq!(
            parse_detail_callback("detail:da:150000001"),
            Some(("da", "150000001"))
        );
        assert_eq!(
            parse_detail_callback("detail:sd:150000001"),
            Some(("sd", "150000001"))
        );
        assert!(parse_detail_callback("detail:da:150000001:42").is_none());
        assert!(parse_detail_callback("detail:../../cookie:150000001").is_none());
        assert!(parse_detail_callback("detail:da:1 OR 1=1").is_none());
        assert!(parse_detail_callback("detail:da:123").is_none());
    }

    #[test]
    fn missing_uid_commands_are_recognized_without_stealing_commands_with_arguments() {
        assert_eq!(missing_detail_kind(Some("/da")), Some("da"));
        assert_eq!(missing_detail_kind(Some("/SHIYU@my_bot")), Some("sd"));
        assert_eq!(missing_detail_kind(Some("/da 150000001")), None);
        assert_eq!(missing_detail_kind(Some("/status")), None);
        assert_eq!(missing_detail_kind(None), None);
    }

    #[test]
    fn choice_keyboard_contains_only_supplied_chat_members() {
        let users = vec![crate::db::UserRow {
            chat_id: 10,
            telegram_user_id: 20,
            uid: "150000001".to_owned(),
            nickname: Some("Alice".to_owned()),
        }];
        let keyboard = uid_choice_keyboard(&users, "da").unwrap();
        assert_eq!(keyboard.inline_keyboard.len(), 1);
        assert_eq!(keyboard.inline_keyboard[0][0].text, "Alice");
        assert!(
            format!("{:?}", keyboard.inline_keyboard[0][0].kind).contains("detail:da:150000001")
        );
    }

    #[test]
    fn compares_current_season_with_previous_and_handles_missing_history() {
        let seasons = vec![
            crate::db::SeasonResult {
                season_start: "2026-07-01".to_owned(),
                total_score: 60_000,
                data_json: r#"{"total_star":9}"#.to_owned(),
            },
            crate::db::SeasonResult {
                season_start: "2026-06-15".to_owned(),
                total_score: 55_000,
                data_json: r#"{"total_star":8}"#.to_owned(),
            },
        ];
        assert_eq!(
            format_season_comparison("deadly_assault", &seasons).as_deref(),
            Some("очки +5000, звёзды +1")
        );
        assert!(format_season_comparison("deadly_assault", &seasons[..1]).is_none());
    }
}
