use crate::{
    BotState,
    agent::{self, AgentRun, Attachment, ChatAgent, Outcome, Status, topic},
    bot_templates::BotTemplateSender,
    db::PromptSession,
};
use anyhow::Result;
use teloxide::prelude::*;

pub async fn command(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
) -> Result<()> {
    if !crate::agent_access::check_chat(bot, msg.into(), crate::db::AiFeature::Prompt, state)
        .await?
    {
        return Ok(());
    }
    if question.trim().is_empty() {
        BotTemplateSender::new(bot, &state.templates)
            .send_to_thread(msg.chat.id, topic(msg), "prompt_usage", &())
            .await?;
        return Ok(());
    }
    let session = replied_session(msg, &state.db).await?.unwrap_or_default();
    answer(bot, msg, question, state, session).await
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
    let mut session = db
        .prompt_session(
            msg.chat.id.0,
            topic(msg).map(|id| id.0.0),
            reply.id.0,
            crate::agent_access::sender(msg).and_then(|u| i64::try_from(u.id.0).ok()),
        )
        .await?;
    if reply.document().is_some()
        && let Some(turn) = session.as_mut().and_then(|s| s.turns.last_mut())
    {
        turn.answer
            .push_str("\n\nПользователь отвечает на PNG-инфографику, отправленную с этим ответом.");
    }
    Ok(session)
}

pub async fn follow_up(bot: Bot, msg: Message, state: std::sync::Arc<BotState>) -> Result<()> {
    if let Some(session) = replied_session(&msg, &state.db).await?
        && let Some(question) = msg.text()
        && crate::agent_access::check_chat(
            &bot,
            (&msg).into(),
            crate::db::AiFeature::Prompt,
            &state,
        )
        .await?
    {
        answer(&bot, &msg, question, &state, session).await?;
    }
    Ok(())
}

async fn answer(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
    session: PromptSession,
) -> Result<()> {
    agent::launch(
        bot,
        msg.into(),
        crate::agent_access::sender(msg),
        "prompt",
        state,
        Question {
            author: crate::agent_access::sender(msg).and_then(|u| i64::try_from(u.id.0).ok()),
            text: question.to_owned(),
            session,
        },
    )
    .await
}

struct Question {
    author: Option<i64>,
    text: String,
    session: PromptSession,
}

impl ChatAgent for Question {
    fn initial_status(&self) -> Status<'_> {
        Status::Question(&self.text)
    }

    async fn execute(&self, run: &mut AgentRun<'_>, state: &BotState) -> Result<Outcome> {
        let model = run.model().to_string();
        let (answer, attachment) = run
            .track(self.initial_status(), |tx| async move {
                let key = rustverse_ai::api_key()?;
                let answer = rustverse_ai::continue_prompt_with_references(
                    &state.beta.client,
                    &key,
                    &model,
                    &self.text,
                    &self.session.turns,
                    tx.clone(),
                )
                .await?;
                let attachment = if let Some(brief) = &answer.infographic {
                    match rustverse_ai::infographic::create(
                        &key,
                        &model,
                        brief,
                        &answer.text,
                        tx,
                        rustverse_images::render_infographic,
                    )
                    .await
                    {
                        Ok(png) => Attachment::Infographic(png),
                        Err(_) => {
                            log::warn!("Could not produce a reviewed infographic");
                            Attachment::Unavailable
                        }
                    }
                } else {
                    Attachment::None
                };
                Ok((answer, attachment))
            })
            .await?;
        Ok(Outcome::Answer {
            text: answer.text,
            references: answer.references,
            attachment,
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
