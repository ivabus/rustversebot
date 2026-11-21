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

pub async fn admit(
    bot: &Bot,
    chat: ChatId,
    thread: Option<ThreadId>,
    user: Option<&User>,
    command: &str,
    state: &BotState,
) -> Result<bool> {
    let template = if let Some(user) = user {
        let decision = state
            .db
            .admit_ai(
                i64::try_from(user.id.0)?,
                user.username.as_deref(),
                &user.full_name(),
                command,
                state.admin_id,
            )
            .await?;
        log::info!(
            "AI access user={} command={} decision={}",
            user.id.0,
            command,
            decision
        );
        match decision.as_str() {
            "admin" | "allowed" => return Ok(true),
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
    Ok(false)
}

pub enum Action {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::Value;
    use teloxide::utils::command::BotCommands;

    #[tokio::test]
    async fn telegram_admin_controls_and_picker_work_through_command_dispatch() {
        let replies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let capture = replies.clone();
        let app=Router::new().fallback(post(move |uri:axum::http::Uri,Json(request):Json<Value>| {
            let capture=capture.clone();
            async move {
                if uri.path().to_ascii_lowercase().ends_with("/deletemessage") {
                    return Json(json!({"ok":true,"result":true}));
                }
                capture.lock().unwrap().push(request);
                Json(json!({"ok":true,"result":{"message_id":2,"date":1,"chat":{"id":42,"type":"private"},"text":"ok"}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bot = Bot::new("1:test").set_api_url(url);
        let state = std::sync::Arc::new(BotState {
            beta: crate::beta::BetaPages::new().unwrap(),
            db: crate::db::Db::new_test().await.unwrap(),
            templates: crate::templates::TemplateEngine::new().unwrap(),
            admin_id: 99,
            public_web_url: None,
            nanoka: nanoka::NanokaClient::new(),
            http: reqwest::Client::new(),
        });
        let invoke = |id: i64, cmd: &str| {
            let msg: Message = serde_json::from_value(
                json!({"message_id":1,"date":1,"chat":{"id":id,"type":"private"},
                "from":{"id":id,"first_name":"Player","is_bot":false},"text":cmd}),
            )
            .unwrap();
            crate::handlers::command_handler(
                bot.clone(),
                msg,
                crate::Command::parse(cmd, "TestBot").unwrap(),
                state.clone(),
            )
        };
        invoke(42, "/diff").await.unwrap();
        let picker_reply = replies.lock().unwrap().clone();
        assert!(
            picker_reply.last().unwrap()["reply_markup"]
                .to_string()
                .contains("diff:branch:release"),
            "{picker_reply:?}"
        );
        invoke(42, "/aiban 42").await.unwrap();
        assert!(
            replies.lock().unwrap().last().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("администратору")
        );
        invoke(99, "/aiban 42").await.unwrap();
        invoke(42, "/prompt вопрос").await.unwrap();
        assert!(
            replies.lock().unwrap().last().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("закрыт")
        );
        invoke(99, "/resetlimit 42").await.unwrap();
        invoke(42, "/betadiff").await.unwrap();
        assert!(
            replies.lock().unwrap().last().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("закрыт")
        );
        invoke(99, "/aiusers").await.unwrap();
        let text = replies.lock().unwrap().last().unwrap()["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(text.contains("42") && text.contains("/betadiff") && text.contains("заблокирован"));
        invoke(99, "/aiunban 42").await.unwrap();
        assert_eq!(
            state
                .db
                .admit_ai(42, None, "Player", "diff", 99)
                .await
                .unwrap(),
            "allowed"
        );
    }
}
