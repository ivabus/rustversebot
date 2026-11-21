use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;
use teloxide::{prelude::*, types::ThreadId};
use tokio::sync::watch;

use crate::{BotState, bot_templates::BotTemplateSender, telegram_html};

// teloxide 0.13 predates sendMessageDraft. Its client and API URL also work
// with self-hosted Bot API servers. Never log errors containing the tokenized URL.
async fn previews(
    bot: &Bot,
    chat: ChatId,
    thread: Option<ThreadId>,
    draft_id: i32,
    updates: watch::Receiver<String>,
) {
    let Ok(url) = bot
        .api_url()
        .join(&format!("./bot{}/sendMessageDraft", bot.token()))
    else {
        return;
    };
    let mut interval = tokio::time::interval(Duration::from_millis(1500));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = None;
    let mut sent = Instant::now();
    loop {
        interval.tick().await;
        let text = updates.borrow().clone();
        if last.as_ref() == Some(&text) && sent.elapsed() < Duration::from_secs(20) {
            continue;
        }
        let mut body = json!({"chat_id":chat.0,"draft_id":draft_id,"text":telegram_html::preview(&text),"parse_mode":"HTML"});
        if let Some(thread) = thread {
            body["message_thread_id"] = json!(thread);
        }
        let result = bot
            .client()
            .post(url.clone())
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        if !result.is_ok_and(|r| r.status().is_success()) {
            return;
        }
        last = Some(text);
        sent = Instant::now();
    }
}

pub async fn command(bot: &Bot, msg: &Message, question: &str, state: &BotState) -> Result<()> {
    let sender = BotTemplateSender::new(bot, &state.templates);
    let chat = msg.chat.id;
    let thread = msg.thread_id;
    if question.trim().is_empty() {
        sender
            .send_to_thread(chat, thread, "prompt_usage", &())
            .await?;
        return Ok(());
    }
    if !crate::agent_access::admit(
        bot,
        chat,
        thread,
        crate::agent_access::sender(msg),
        "prompt",
        state,
    )
    .await?
    {
        return Ok(());
    }
    let result = async {
        let key = rustverse_ai::api_key()?;
        let (updates, rx) = watch::channel(String::new());
        let private = msg.chat.is_private();
        let answer = rustverse_ai::prompt(
            &state.beta.client,
            &key,
            &state.beta.model,
            question,
            private.then_some(updates),
        );
        tokio::pin!(answer);
        if private {
            tokio::select! {
                result=&mut answer => result,
                _=previews(bot,chat,thread,msg.id.0.max(1),rx) => answer.await,
            }
        } else {
            answer.await
        }
    }
    .await;
    match result {
        Ok(text) => {
            for chunk in telegram_html::chunks(&text) {
                if !chunk.trim().is_empty() {
                    sender
                        .send_to_thread(chat, thread, "prompt_answer", &json!({"text":chunk}))
                        .await?;
                }
            }
        }
        Err(err) => {
            sender
                .send_to_thread(
                    chat,
                    thread,
                    "error_generic",
                    &json!({"error":err.to_string()}),
                )
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::Value;
    use teloxide::types::MessageId;

    #[tokio::test]
    async fn draft_posts_balanced_html_using_the_original_topic() {
        let (record, mut requests) = tokio::sync::mpsc::channel(1);
        let router = Router::new().route(
            "/bot1:test/sendMessageDraft",
            post(move |Json(body): Json<Value>| {
                let record = record.clone();
                async move {
                    record.send(body).await.unwrap();
                    Json(json!({"ok":true,"result":true}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let bot = Bot::new("1:test").set_api_url(endpoint);
        let (_updates, rx) = watch::channel("Эффект **A & B".to_owned());
        let send = previews(&bot, ChatId(123), Some(ThreadId(MessageId(5))), 17, rx);
        tokio::select! {
            body=requests.recv()=>assert_eq!(body.unwrap(),json!({"chat_id":123,"draft_id":17,"message_thread_id":5,"parse_mode":"HTML","text":"Эффект <b>A &amp; B</b>"})),
            _=send=>panic!("draft delivery failed"),
            _=tokio::time::sleep(Duration::from_secs(5))=>panic!("no draft"),
        }
    }
}
