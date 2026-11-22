use crate::{
    BotState,
    agent::{self, AgentRun, ChatAgent, Outcome, Status, topic},
    bot_templates::BotTemplateSender,
    db::PromptSession,
};
use anyhow::Result;
use teloxide::prelude::*;
pub(crate) mod media;

pub async fn command(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
) -> Result<()> {
    let images = msg
        .reply_to_message()
        .filter(|reply| reply.chat.id == msg.chat.id)
        .and_then(media::image_id)
        .map(str::to_owned)
        .into_iter()
        .collect();
    start(bot, msg, question, state, images).await
}

async fn start(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
    images: Vec<String>,
) -> Result<()> {
    if !crate::agent_access::check_chat(bot, msg.into(), crate::db::AiFeature::Prompt, state)
        .await?
    {
        return Ok(());
    }
    if question.trim().is_empty() && images.is_empty() {
        BotTemplateSender::new(bot, &state.templates)
            .send_to_thread(msg.chat.id, topic(msg), "prompt_usage", &())
            .await?;
        return Ok(());
    }
    let session = replied_session(msg, &state.db).await?.unwrap_or_default();
    answer(bot, msg, question, state, session, images).await
}

pub fn is_follow_up(msg: Message) -> bool {
    msg.reply_to_message().is_some()
        && msg
            .text()
            .is_some_and(|s| !s.trim().is_empty() && !s.trim_start().starts_with('/'))
}

async fn replied_session(msg: &Message, db: &crate::db::Db) -> Result<Option<PromptSession>> {
    let Some(reply) = msg.reply_to_message().filter(|r| r.chat.id == msg.chat.id) else {
        return Ok(None);
    };
    db.prompt_session(
        msg.chat.id.0,
        topic(msg).map(|id| id.0.0),
        reply.id.0,
        crate::agent_access::sender(msg).and_then(|u| i64::try_from(u.id.0).ok()),
    )
    .await
}

pub async fn follow_up(bot: Bot, msg: Message, state: std::sync::Arc<BotState>) -> Result<()> {
    continue_reply(&bot, &msg, msg.text().unwrap_or_default(), &state, vec![]).await
}

async fn continue_reply(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
    images: Vec<String>,
) -> Result<()> {
    if let Some(session) = replied_session(msg, &state.db).await?
        && crate::agent_access::check_chat(bot, msg.into(), crate::db::AiFeature::Prompt, state)
            .await?
    {
        answer(bot, msg, question, state, session, images).await?;
    }
    Ok(())
}

async fn answer(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
    session: PromptSession,
    images: Vec<String>,
) -> Result<()> {
    let sender = crate::agent_access::sender(msg);
    agent::launch(
        bot,
        msg.into(),
        sender,
        "prompt",
        state,
        Question {
            display_name: sender.map(|u| u.full_name()).unwrap_or_default(),
            author: sender.and_then(|u| i64::try_from(u.id.0).ok()),
            text: if question.trim().is_empty() {
                "Изображения без подписи".into()
            } else {
                question.to_owned()
            },
            images,
            session,
        },
    )
    .await
}

struct Question {
    display_name: String,
    author: Option<i64>,
    text: String,
    images: Vec<String>,
    session: PromptSession,
}

impl ChatAgent for Question {
    fn initial_status(&self) -> Status<'_> {
        Status::Question {
            text: &self.text,
            session_id: self.session.id,
        }
    }

    async fn execute(&self, run: &mut AgentRun<'_>, state: &BotState) -> Result<Outcome> {
        let model = run.model().to_string();
        let bot = run.bot().clone();
        let chat_id = run.address().chat.0;
        let answer = run
            .track(self.initial_status(), |tx| async move {
                let key = rustverse_ai::api_key()?;
                let mut remaining = media::IMAGE_BUDGET;
                let input = rustverse_ai::PromptInput {
                    text: self.text.clone(),
                    images: media::download(&bot, &self.images, &mut remaining).await?,
                };
                let mut history = Vec::with_capacity(self.session.turns.len());
                for turn in self.session.turns.iter().rev() {
                    let mut question = turn.question.clone();
                    let images = match media::download(&bot, &turn.images, &mut remaining).await {
                        Ok(images) => images,
                        Err(_) => {
                            question.push_str(" [Изображения этой реплики недоступны]");
                            vec![]
                        }
                    };
                    history.push(rustverse_ai::ImagePromptTurn {
                        question,
                        answer: turn.answer.clone(),
                        images,
                    });
                }
                history.reverse();
                tracing::Span::current().record("image_count", input.images.len());
                tracing::Span::current().record(
                    "history_image_count",
                    history.iter().map(|turn| turn.images.len()).sum::<usize>(),
                );
                let players = self.author.map(|author| crate::db::players::ChatPlayers {
                    db: state.db.clone(),
                    chat: chat_id,
                    author,
                    display_name: self.display_name.clone(),
                });
                rustverse_ai::prompt_with_chat_players(
                    &state.beta.client,
                    &key,
                    &model,
                    input,
                    &history,
                    players.map(|p| std::sync::Arc::new(p) as _),
                    tx,
                )
                .await
            })
            .await?;
        tracing::Span::current().record("langfuse.observation.output", &answer.text);
        Ok(Outcome::Answer {
            text: answer.text,
            image_ids: self.images.clone(),
            references: answer.references,
            author: self.author,
            parent: self.session.last_answer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn ordinary_group_replies_find_the_answer_without_treating_reply_threads_as_topics() {
        let db = crate::db::Db::new_test().await.unwrap();
        db.save_prompt_turn(
            -100123,
            None,
            42,
            None,
            &crate::db::DeliveredTurn {
                question: "Первый вопрос",
                answer: "Первый ответ",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let chat = json!({"id":-100123,"type":"supergroup","title":"ZZZ"});
        let mut data = json!({"message_id":43,"message_thread_id":42,"chat":chat,"date":1,"text":"А на прошлом патче?",
            "reply_to_message":{"message_id":42,"chat":chat,"date":1,"text":"Первый ответ"}});
        let parse = |v: &serde_json::Value| serde_json::from_value::<Message>(v.clone()).unwrap();
        assert!(is_follow_up(parse(&data)));
        assert_eq!(
            replied_session(&parse(&data), &db)
                .await
                .unwrap()
                .unwrap()
                .last_answer,
            Some(42)
        );
        data["is_topic_message"] = json!(true);
        assert!(replied_session(&parse(&data), &db).await.unwrap().is_none());
        data["text"] = json!("/status");
        assert!(!is_follow_up(parse(&data)));
        data.as_object_mut().unwrap().remove("reply_to_message");
        data["text"] = json!("Обычное сообщение");
        assert!(!is_follow_up(parse(&data)));
    }
}
