use anyhow::Result;
use std::{future::Future, pin::Pin};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

pub(crate) type Job = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
const ACTIVE: usize = 4;

pub(crate) fn channel() -> (mpsc::Sender<Job>, mpsc::Receiver<Job>) {
    mpsc::channel(8)
}

pub(crate) async fn run(mut queue: mpsc::Receiver<Job>, mut shutdown: watch::Receiver<bool>) {
    let mut active = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop|*stop) => break,
            Some(result) = active.join_next(), if !active.is_empty() => match result {
                Ok(Ok(())) => {},
                Ok(Err(e)) => log::warn!("AI job failed: {e:#}"),
                Err(e) => log::error!("AI job stopped: {e}"),
            },
            job = queue.recv(), if active.len()<ACTIVE => match job {
                Some(job) => { active.spawn(job); }
                None => break,
            },
        }
    }
    active.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BotState, Command, handlers};
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use teloxide::Bot;

    #[tokio::test]
    async fn active_and_waiting_jobs_are_bounded_and_stop_on_shutdown() {
        let (queue, rx) = channel();
        let (stop, shutdown) = watch::channel(false);
        let worker = tokio::spawn(run(rx, shutdown));
        let (started, mut starts) = mpsc::unbounded_channel();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let job = || -> Job {
            let started = started.clone();
            let gate = gate.clone();
            Box::pin(async move {
                started.send(())?;
                gate.acquire().await?.forget();
                Ok(())
            })
        };
        for _ in 0..ACTIVE {
            queue.send(job()).await.unwrap();
            starts.recv().await.unwrap();
        }
        for _ in 0..queue.max_capacity() {
            assert!(queue.try_send(job()).is_ok());
        }
        assert!(matches!(
            queue.try_reserve(),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        gate.add_permits(1);
        starts.recv().await.unwrap();
        assert!(queue.try_reserve().is_ok());
        stop.send(true).unwrap();
        worker.await.unwrap();
        assert!(queue.is_closed());
        assert_eq!(Arc::strong_count(&gate), 1);
    }

    #[tokio::test]
    async fn same_chat_accepts_start_while_prompt_is_pending() {
        let (events, mut received) = mpsc::unbounded_channel::<String>();
        let router=Router::new().fallback(post(move |Json(body):Json<Value>| {
            let events=events.clone();
            async move {
                events.send(body["text"].as_str().unwrap().to_owned()).unwrap();
                Json(json!({"ok":true,"result":{"message_id":2,"date":1,"chat":{"id":-123,"type":"group","title":"Test"},"text":"ok"}}))
            }
        }));
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
        let bot = Bot::new("1:test").set_api_url(url.parse().unwrap());
        let (ai_jobs, mut pending) = channel();
        let state = Arc::new(BotState {
            ai_jobs,
            beta: crate::beta::BetaPages::new().unwrap(),
            db: crate::db::Db::new_test().await.unwrap(),
            templates: crate::templates::TemplateEngine::new().unwrap(),
            admin_id: 1,
            public_web_url: None,
            nanoka: nanoka::NanokaClient::new(),
            http: reqwest::Client::new(),
        });
        let msg:teloxide::types::Message=serde_json::from_value(json!({"message_id":1,"date":1,"chat":{"id":-123,"type":"group","title":"Test"},"from":{"id":1,"is_bot":false,"first_name":"Tester"},"text":"/prompt Вопрос"})).unwrap();
        handlers::command_handler(
            bot.clone(),
            msg.clone(),
            Command::Prompt("Вопрос".into()),
            state.clone(),
        )
        .await
        .unwrap();
        assert!(received.try_recv().unwrap().contains("Готовлю ответ"));
        handlers::command_handler(bot, msg, Command::Start, state.clone())
            .await
            .unwrap();
        assert_eq!(
            received.try_recv().unwrap(),
            state.templates.render("welcome", &()).unwrap().text
        );
        assert!(pending.try_recv().is_ok());
        server.abort();
    }
}
