use anyhow::{Context, Result};
use rustverse_ai::{PromptProgress, PromptStage};
use serde_json::json;
use std::time::Duration;
use teloxide::{
    prelude::*,
    types::{MessageId, ParseMode},
};
use tokio::{sync::watch, time::Instant};

use crate::{
    BotState,
    bot_templates::{BotTemplateSender, MESSAGE_LIMIT, RenderedTemplate},
    telegram_html,
    templates::TemplateEngine,
};

const DRAFT_INTERVAL: Duration = Duration::from_millis(1500);
const GROUP_INTERVAL: Duration = Duration::from_secs(15);
const DRAFT_REFRESH: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
enum LiveMessage {
    Draft,
    Group(MessageId),
}

struct PromptOutput<'a> {
    bot: &'a Bot,
    msg: &'a Message,
    state: &'a BotState,
    question: &'a str,
}

fn footer(
    templates: &TemplateEngine,
    model: &str,
    progress: &PromptProgress,
) -> Result<RenderedTemplate> {
    // Keep even a malformed configured model name within Telegram's message limit.
    let model: String = model.chars().take(128).collect();
    templates.render("prompt_status", &json!({"model":model,"p":progress}))
}

fn messages(
    templates: &TemplateEngine,
    model: &str,
    progress: &PromptProgress,
    question: &str,
    text: &str,
    live: bool,
) -> Result<Vec<RenderedTemplate>> {
    let footer = footer(templates, model, progress)?;
    // The source length also counts the footer's tags and escapes, giving a conservative bound.
    let limit = MESSAGE_LIMIT
        .checked_sub(footer.text.encode_utf16().count() + 2)
        .filter(|n| *n >= 2)
        .context("Статус превышает лимит сообщения")?;
    let mut parts = if live {
        vec![telegram_html::preview(question, text, limit)]
    } else if progress.stage == PromptStage::Failed {
        telegram_html::plain_chunks(question, text, limit)
    } else {
        telegram_html::chunks(question, text, limit)
    };
    parts.retain(|s| !s.trim().is_empty());
    if parts.is_empty() {
        return Ok(vec![footer]);
    }
    let last = parts.len() - 1;
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            templates.render(
                "prompt_answer",
                &json!({"text":part,"footer":(index==last).then_some(&footer.text)}),
            )
        })
        .collect()
}

enum Update {
    Sent,
    Wait(Duration),
    Unavailable,
}

impl PromptOutput<'_> {
    // teloxide 0.13 predates sendMessageDraft. Never expose its tokenized URL in errors.
    async fn draft(&self, text: &str) -> Update {
        let Ok(url) = self
            .bot
            .api_url()
            .join(&format!("./bot{}/sendMessageDraft", self.bot.token()))
        else {
            return Update::Unavailable;
        };
        let mut body = json!({"chat_id":self.msg.chat.id.0,"draft_id":self.msg.id.0.max(1),"text":text,"parse_mode":"HTML"});
        if let Some(thread) = self.msg.thread_id {
            body["message_thread_id"] = json!(thread);
        }
        let Ok(response) = self
            .bot
            .client()
            .post(url)
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        else {
            return Update::Unavailable;
        };
        if response.status().is_success() {
            return Update::Sent;
        }
        if response.status().as_u16() == 429
            && let Ok(body) = response.json::<serde_json::Value>().await
            && let Some(seconds) = body
                .pointer("/parameters/retry_after")
                .and_then(|v| v.as_u64())
        {
            return Update::Wait(Duration::from_secs(seconds));
        }
        Update::Unavailable
    }

    async fn edit(&self, id: MessageId, text: &str) -> Result<(), teloxide::RequestError> {
        match self
            .bot
            .edit_message_text(self.msg.chat.id, id, text)
            .parse_mode(ParseMode::Html)
            .await
        {
            Ok(_) | Err(teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)) => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn start(&self, status: RenderedTemplate) -> Result<LiveMessage> {
        if self.msg.chat.is_private() && matches!(self.draft(&status.text).await, Update::Sent) {
            return Ok(LiveMessage::Draft);
        }
        let message = self.send(status, None).await?;
        Ok(LiveMessage::Group(message))
    }

    async fn updates(
        &self,
        target: LiveMessage,
        mut progress: watch::Receiver<PromptProgress>,
        mut last: String,
    ) {
        let spacing = match target {
            LiveMessage::Draft => DRAFT_INTERVAL,
            LiveMessage::Group(_) => GROUP_INTERVAL,
        };
        let mut timer = tokio::time::interval_at(Instant::now() + spacing, spacing);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut sent = Instant::now();
        loop {
            tokio::select! {
                changed = progress.changed() => {
                    if changed.is_err() {return}
                    continue;
                }
                _ = timer.tick() => {}
            }
            let p = progress.borrow().clone();
            let Ok(parts) = messages(
                &self.state.templates,
                &self.state.beta.model,
                &p,
                self.question,
                &p.text,
                true,
            ) else {
                return;
            };
            let text = &parts[0].text;
            let refresh = matches!(target, LiveMessage::Draft) && sent.elapsed() >= DRAFT_REFRESH;
            if *text == last && !refresh {
                continue;
            }
            let update = match target {
                LiveMessage::Draft => self.draft(text).await,
                LiveMessage::Group(id) => match self.edit(id, text).await {
                    Ok(()) => Update::Sent,
                    Err(teloxide::RequestError::RetryAfter(wait)) => Update::Wait(wait.duration()),
                    Err(_) => Update::Unavailable,
                },
            };
            match update {
                Update::Sent => {
                    last.clone_from(text);
                    sent = Instant::now();
                }
                Update::Wait(wait) => timer.reset_after(wait.max(spacing)),
                Update::Unavailable => return,
            }
        }
    }

    // Replace the group's status with the first final chunk, and append subsequent
    // chunks. Retry only the chunk Telegram rejected. A deleted status can be replaced.
    async fn send(
        &self,
        rendered: RenderedTemplate,
        mut edit: Option<MessageId>,
    ) -> Result<MessageId> {
        let attempts = crate::scheduler::SchedulerSettings::from_env()
            .retry_attempts
            .max(1);
        let sender = BotTemplateSender::new(self.bot, &self.state.templates);
        for attempt in 1..=attempts {
            if let Some(id) = edit {
                match self.edit(id, &rendered.text).await {
                    Ok(()) => return Ok(id),
                    Err(teloxide::RequestError::Api(
                        teloxide::ApiError::MessageCantBeEdited
                        | teloxide::ApiError::MessageToEditNotFound,
                    )) => edit = None,
                    Err(teloxide::RequestError::RetryAfter(wait)) if attempt < attempts => {
                        tokio::time::sleep(wait.duration()).await;
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            match sender
                .send_rendered_to_thread(self.msg.chat.id, self.msg.thread_id, rendered.clone())
                .await
            {
                Ok(msg) => return Ok(msg.id),
                Err(err) => match err.downcast_ref::<teloxide::RequestError>() {
                    Some(teloxide::RequestError::RetryAfter(wait)) if attempt < attempts => {
                        tokio::time::sleep(wait.duration()).await
                    }
                    _ => return Err(err),
                },
            }
        }
        unreachable!("last delivery attempt always returns")
    }
}

pub async fn command(bot: &Bot, msg: &Message, question: &str, state: &BotState) -> Result<()> {
    let sender = BotTemplateSender::new(bot, &state.templates);
    if question.trim().is_empty() {
        sender
            .send_to_thread(msg.chat.id, msg.thread_id, "prompt_usage", &())
            .await?;
        return Ok(());
    }
    if !crate::agent_access::admit(
        bot,
        msg.chat.id,
        msg.thread_id,
        crate::agent_access::sender(msg),
        "prompt",
        state,
    )
    .await?
    {
        return Ok(());
    }
    let output = PromptOutput {
        bot,
        msg,
        state,
        question,
    };
    let model = &state.beta.model;
    let initial = PromptProgress::default();
    let status = messages(&state.templates, model, &initial, question, "", true)?.remove(0);
    let last = status.text.clone();
    let target = output.start(status).await?;
    let (tx, rx) = watch::channel(initial);
    let generate = async move {
        let key = rustverse_ai::api_key()?;
        rustverse_ai::prompt_with_progress(&state.beta.client, &key, model, question, tx).await
    };
    // Finish an in-flight Telegram edit before delivering the permanent answer,
    // so a delayed status update cannot overwrite it.
    let (answer, ()) = tokio::join!(generate, output.updates(target, rx.clone(), last));
    let mut stats = rx.borrow().clone();
    let parts = match answer {
        Ok(text) => {
            stats.stage = PromptStage::Complete;
            messages(&state.templates, model, &stats, question, &text, false)?
        }
        Err(err) => {
            stats.stage = PromptStage::Failed;
            let error = state
                .templates
                .render("error_generic", &json!({"error":err.to_string()}))?;
            messages(
                &state.templates,
                model,
                &stats,
                question,
                &error.text,
                false,
            )?
        }
    };
    let mut edit = match target {
        LiveMessage::Group(id) => Some(id),
        LiveMessage::Draft => None,
    };
    for part in parts {
        output.send(part, edit.take()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_and_final_footer_fit_and_only_the_last_part_has_counters() {
        let templates = TemplateEngine::new().unwrap();
        let initial = PromptProgress::default();
        let question = "Что значит **урон** <A&B>?\nИ для кого?";
        let header = "<blockquote>Что значит **урон** &lt;A&amp;B&gt;?\nИ для кого?</blockquote>";
        let status = messages(&templates, "model<&>", &initial, question, "", true).unwrap();
        assert!(status[0].text.starts_with(header));
        assert!(status[0].text.contains("Готовлю ответ") && status[0].text.contains("Ходы: 0"));
        let p = PromptProgress {
            stage: PromptStage::Complete,
            tools: 4,
            turns: 3,
            tokens: Some(1300),
            ..initial
        };
        let parts = messages(
            &templates,
            "model<&>",
            &p,
            question,
            &format!("**{}**", "🌙".repeat(6200)),
            false,
        )
        .unwrap();
        assert!(parts.len() > 1);
        assert!(parts[0].text.starts_with(header));
        assert!(parts[1..].iter().all(|part| !part.text.contains(header)));
        for part in &parts[..parts.len() - 1] {
            assert!(!part.text.contains("Токены"));
        }
        let last = &parts.last().unwrap().text;
        assert!(
            last.contains("Ответ готов")
                && last.contains("Вызовы инструментов: 4 · Ходы: 3 · Токены: 1300")
        );
        for part in parts {
            let visible = scraper::Html::parse_fragment(&part.text)
                .root_element()
                .text()
                .collect::<String>();
            assert!(visible.encode_utf16().count() <= MESSAGE_LIMIT);
        }
        let unknown = PromptProgress { tokens: None, ..p };
        let footer = footer(&templates, "model<&>", &unknown).unwrap().text;
        assert!(footer.contains("Токены: —") && footer.contains("&lt;&amp;&gt;"));
    }

    #[test]
    fn long_question_is_abbreviated_in_preview_and_preserved_in_full_in_the_answer() {
        let templates = TemplateEngine::new().unwrap();
        let question = "🌙<&>".repeat(1250);
        let p = PromptProgress::default();
        let draft = messages(&templates, "model", &p, &question, "Пишу", true).unwrap();
        assert!(draft[0].text.contains("…</blockquote>") && draft[0].text.contains("Пишу"));
        let selector = scraper::Selector::parse("blockquote").unwrap();
        for stage in [PromptStage::Complete, PromptStage::Failed] {
            let p = PromptProgress {
                stage,
                ..PromptProgress::default()
            };
            let parts = messages(&templates, "model", &p, &question, "Текст", false).unwrap();
            let mut quoted = String::new();
            for part in parts {
                let html = scraper::Html::parse_fragment(&part.text);
                assert!(
                    html.root_element()
                        .text()
                        .collect::<String>()
                        .encode_utf16()
                        .count()
                        <= MESSAGE_LIMIT
                );
                for block in html.select(&selector) {
                    quoted.extend(block.text());
                }
            }
            assert_eq!(quoted, question);
        }
    }
}
