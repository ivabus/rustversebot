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
        let Ok(response) = result else { return };
        if response.status().as_u16() == 429 {
            let Ok(body) = response.json::<serde_json::Value>().await else {
                return;
            };
            let Some(seconds) = body
                .pointer("/parameters/retry_after")
                .and_then(|v| v.as_u64())
            else {
                return;
            };
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            // Read the latest preview after the wait; the answer may already be complete.
            continue;
        }
        if !response.status().is_success() {
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
            let attempts = crate::scheduler::SchedulerSettings::from_env().retry_attempts;
            send_answer(&sender, chat, thread, &text, attempts).await?;
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

async fn send_answer(
    sender: &BotTemplateSender<'_>,
    chat: ChatId,
    thread: Option<ThreadId>,
    text: &str,
    attempts: usize,
) -> Result<()> {
    for chunk in telegram_html::chunks(text)
        .into_iter()
        .filter(|c| !c.trim().is_empty())
    {
        let data = json!({"text":chunk});
        for attempt in 1..=attempts.max(1) {
            match sender
                .send_to_thread(chat, thread, "prompt_answer", &data)
                .await
            {
                Ok(_) => break,
                Err(err) => match err.downcast_ref::<teloxide::RequestError>() {
                    Some(teloxide::RequestError::RetryAfter(wait)) if attempt < attempts => {
                        tokio::time::sleep(wait.duration()).await;
                    }
                    _ => return Err(err),
                },
            }
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
    async fn retry_after_resends_only_the_rejected_html_chunk() {
        use axum::response::IntoResponse;
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let log = received.clone();
        let router=Router::new().fallback(post(move |Json(body):Json<Value>| {
            let log=log.clone();
            async move {
                let mut calls=log.lock().unwrap();
                calls.push(body);
                if calls.len()==2 {
                    return (axum::http::StatusCode::TOO_MANY_REQUESTS,Json(json!({"ok":false,"error_code":429,
                        "description":"Too Many Requests: retry after 0","parameters":{"retry_after":0}}))).into_response();
                }
                Json(json!({"ok":true,"result":{"message_id":1,"date":1,"chat":{"id":123,"type":"private"},"text":"ok"}})).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let bot = Bot::new("1:test").set_api_url(endpoint);
        let templates = crate::templates::TemplateEngine::new().unwrap();
        let answer = format!("**{}х**", "я".repeat(crate::bot_templates::MESSAGE_LIMIT));
        send_answer(
            &BotTemplateSender::new(&bot, &templates),
            ChatId(123),
            Some(ThreadId(MessageId(5))),
            &answer,
            2,
        )
        .await
        .unwrap();
        let calls = received.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_ne!(calls[0]["text"], calls[1]["text"]);
        assert_eq!(calls[1], calls[2]);
        assert_eq!(calls[2]["text"], "<b>х</b>");
        assert!(
            calls
                .iter()
                .all(|c| c["parse_mode"] == "HTML" && c["message_thread_id"] == 5)
        );
    }

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
