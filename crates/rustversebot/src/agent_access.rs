use crate::{BotState, bot_templates::BotTemplateSender};
use anyhow::Result;
use serde_json::json;
use teloxide::{
    prelude::*,
    types::{ThreadId, User},
};

pub fn sender(msg: &Message) -> Option<&User> {
    msg.from.as_ref().filter(|_| msg.sender_chat.is_none())
}

pub fn is_admin(user: Option<&User>, state: &BotState) -> bool {
    user.and_then(|u| i64::try_from(u.id.0).ok()) == Some(state.admin_id)
}

pub(crate) async fn check_chat(
    bot: &Bot,
    address: crate::agent::MessageAddress,
    feature: crate::db::AiFeature,
    state: &BotState,
) -> Result<bool> {
    if state.db.feature_enabled(address.chat.0, feature).await? {
        return Ok(true);
    }
    BotTemplateSender::new(bot, &state.templates)
        .send_to_thread(address.chat, address.thread, "ai_chat_disabled", &())
        .await?;
    Ok(false)
}

pub async fn admit<'a>(
    bot: &Bot,
    chat: ChatId,
    thread: Option<ThreadId>,
    user: Option<&User>,
    command: &str,
    state: &'a BotState,
) -> Result<Option<tokio::sync::mpsc::Permit<'a, crate::ai_jobs::Job>>> {
    let slot = state.ai_jobs.try_reserve().ok();
    let template = if let Some(user) = user {
        let decision = state
            .db
            .admit_ai(
                i64::try_from(user.id.0)?,
                user.username.as_deref(),
                &user.full_name(),
                command,
                state.admin_id,
                slot.is_some(),
            )
            .await?;
        log::info!(
            "AI access user={} command={} decision={}",
            user.id.0,
            command,
            decision
        );
        match decision.as_str() {
            "admin" | "allowed" => return Ok(slot),
            "busy" => "ai_busy",
            "banned" => "ai_banned",
            "limited" => "ai_limit",
            _ => anyhow::bail!("unknown AI access decision"),
        }
    } else {
        "ai_identity"
    };
    BotTemplateSender::new(bot, &state.templates)
        .send_to_thread(chat, thread, template, &())
        .await?;
    Ok(None)
}

pub enum Action {
    Model,
    Users,
    Reset,
    Ban,
    Unban,
}

pub async fn manage(
    bot: &Bot,
    msg: &Message,
    action: Action,
    args: &str,
    state: &BotState,
) -> Result<()> {
    let sender_ui = BotTemplateSender::new(bot, &state.templates);
    let id = args.trim().parse::<i64>().ok().filter(|id| *id > 0);
    let mut data = json!({"id":id});
    let template = if !is_admin(sender(msg), state) {
        "not_admin"
    } else {
        match (action, id) {
            (Action::Model, _) => {
                let setting = args.trim();
                if setting.is_empty() {
                    data = json!({"current":state.ai_model().await?.to_string()});
                } else if setting.contains('@') {
                    match setting.parse::<rustverse_ai::ModelSelection>() {
                        Ok(model) => {
                            state.db.set_ai_model(&model).await?;
                            data = json!({"updated":model.to_string()});
                        }
                        Err(error) => data = json!({"error":error.to_string()}),
                    }
                }
                "ai_model"
            }
            (Action::Users, _) if !msg.chat.is_private() => "ai_users_private",
            (Action::Users, _) => {
                let page = if args.trim().is_empty() {
                    Some(1)
                } else {
                    args.trim().parse::<u32>().ok().filter(|p| *p > 0)
                };
                if let Some(page) = page {
                    data = json!({"page":page,"users":state.db.ai_users(page).await?});
                    "ai_users"
                } else {
                    "ai_admin_usage"
                }
            }
            (Action::Reset, Some(id)) => {
                state.db.reset_ai_limit(id).await?;
                "ai_reset_done"
            }
            (Action::Ban, Some(id)) if id == state.admin_id => "ai_admin_protected",
            (Action::Ban, Some(id)) => {
                state.db.ban_ai(id, true).await?;
                "ai_ban_done"
            }
            (Action::Unban, Some(id)) => {
                state.db.ban_ai(id, false).await?;
                "ai_unban_done"
            }
            _ => "ai_admin_usage",
        }
    };
    sender_ui
        .send_to_thread(msg.chat.id, msg.thread_id, template, &data)
        .await?;
    Ok(())
}
