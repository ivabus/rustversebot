use anyhow::{Context, Result};
use rustverse_ai::{PromptProgress, PromptStage};
use serde_json::json;
use std::time::Duration;
use teloxide::{
    prelude::*,
    types::{MessageId, ParseMode, ThreadId},
};
use tokio::{sync::watch, time::Instant};

use crate::{
    BotState,
    bot_templates::{BotTemplateSender, MESSAGE_LIMIT, RenderedTemplate},
    db::PromptSession,
    telegram_html,
    templates::TemplateEngine,
};

const DRAFT_INTERVAL: Duration = Duration::from_millis(1500);
const GROUP_INTERVAL: Duration = Duration::from_secs(15);
const DRAFT_REFRESH: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
pub(crate) enum LiveMessage {
    Draft,
    Group(MessageId),
}

impl LiveMessage {
    fn new_research_stage(self, progress: &PromptProgress, shown: Option<&str>) -> bool {
        matches!(self, Self::Group(_))
            && progress.research_status.is_some()
            && progress.research_status.as_deref() != shown
    }
}

async fn next_update(
    progress: &mut watch::Receiver<PromptProgress>,
    target: LiveMessage,
    shown: Option<&str>,
    next: Instant,
    retry_at: Option<Instant>,
) -> Option<PromptProgress> {
    if let Some(resume) = retry_at {
        tokio::select! {
            _ = progress.wait_for(|_| false) => return None, // generation finished
            _ = tokio::time::sleep_until(resume) => {}
        }
    }
    tokio::select! {
        changed = progress.wait_for(|p| target.new_research_stage(p, shown)) => {
            changed.ok()?;
        }
        _ = tokio::time::sleep_until(next) => {}
    }
    if progress.has_changed().is_err() {
        return None;
    }
    Some(progress.borrow_and_update().clone())
}

struct PromptOutput {
    bot: Bot,
    msg: Message,
    state: std::sync::Arc<BotState>,
    question: String,
    model: String,
}

pub(crate) struct LiveOutput<'a> {
    pub bot: &'a Bot,
    pub templates: &'a TemplateEngine,
    pub address: MessageAddress,
}

#[derive(Clone, Copy)]
pub(crate) struct MessageAddress {
    pub chat: ChatId,
    pub thread: Option<ThreadId>,
    pub message_id: MessageId,
}

impl From<&Message> for MessageAddress {
    fn from(msg: &Message) -> Self {
        Self {
            chat: msg.chat.id,
            thread: topic(msg),
            message_id: msg.id,
        }
    }
}

impl PromptOutput {
    fn live(&self) -> LiveOutput<'_> {
        LiveOutput {
            bot: &self.bot,
            templates: &self.state.templates,
            address: (&self.msg).into(),
        }
    }
}

// Ordinary supergroup replies may carry a reply-chain ID even outside a forum.
// Only a Telegram topic identifies a separate conversation within a chat.
fn topic(msg: &Message) -> Option<teloxide::types::ThreadId> {
    msg.thread_id
        .filter(|_| msg.is_topic_message || msg.chat.is_private())
}

fn footer(
    templates: &TemplateEngine,
    model: &str,
    progress: &PromptProgress,
) -> Result<RenderedTemplate> {
    status_footer(templates, model, progress, None, None)
}

pub(crate) fn status_footer(
    templates: &TemplateEngine,
    model: &str,
    progress: &PromptProgress,
    diff_stage: Option<&str>,
    agent: Option<&str>,
) -> Result<RenderedTemplate> {
    // Keep even a malformed configured model name within Telegram's message limit.
    let model: String = model.chars().take(128).collect();
    let cost = progress.cost_usd.map(|usd| {
        if usd > 0.0 && usd < 0.000001 {
            return "< $0,000001".to_owned();
        }
        let amount = format!("{usd:.6}");
        format!(
            "≈ ${}",
            amount
                .trim_end_matches('0')
                .trim_end_matches('.')
                .replace('.', ",")
        )
    });
    templates.render(
        "prompt_status",
        &json!({"model":model,"p":progress,"cost":cost,"diff_stage":diff_stage,"agent":agent.map(|s| s.chars().take(100).collect::<String>())}),
    )
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

impl LiveOutput<'_> {
    // teloxide 0.13 predates sendMessageDraft. Never expose its tokenized URL in errors.
    async fn draft(&self, text: &str) -> Update {
        let Ok(url) = self
            .bot
            .api_url()
            .join(&format!("./bot{}/sendMessageDraft", self.bot.token()))
        else {
            return Update::Unavailable;
        };
        let mut body = json!({"chat_id":self.address.chat.0,"draft_id":self.address.message_id.0.max(1),"text":text,"parse_mode":"HTML"});
        if let Some(thread) = self.address.thread {
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

    async fn edit(
        &self,
        id: MessageId,
        text: &str,
        mode: Option<ParseMode>,
    ) -> Result<(), teloxide::RequestError> {
        let mut request = self.bot.edit_message_text(self.address.chat, id, text);
        if let Some(mode) = mode {
            request = request.parse_mode(mode);
        }
        match request.await {
            Ok(_) | Err(teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)) => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn start(&self, status: RenderedTemplate) -> Result<LiveMessage> {
        if self.address.chat.is_user() && matches!(self.draft(&status.text).await, Update::Sent) {
            return Ok(LiveMessage::Draft);
        }
        let message = self.send(status, None).await?;
        Ok(LiveMessage::Group(message))
    }

    pub(crate) async fn updates(
        &self,
        target: LiveMessage,
        mut progress: watch::Receiver<PromptProgress>,
        mut last: String,
        retry_at: &mut Option<Instant>,
        render: impl Fn(&PromptProgress) -> Result<String>,
    ) {
        let spacing = match target {
            LiveMessage::Draft => DRAFT_INTERVAL,
            LiveMessage::Group(_) => GROUP_INTERVAL,
        };
        let mut sent = Instant::now();
        let mut next = sent + spacing;
        let mut shown = None;
        while let Some(mut p) =
            next_update(&mut progress, target, shown.as_deref(), next, *retry_at).await
        {
            *retry_at = None;
            next = Instant::now() + spacing;
            // A text delta can arrive in the same watch snapshot as a new stage.
            // Make sure the first edit for that stage actually displays its name.
            if target.new_research_stage(&p, shown.as_deref()) {
                p.stage = PromptStage::Researching;
            }
            let Ok(text) = render(&p) else {
                return;
            };
            let refresh = matches!(target, LiveMessage::Draft) && sent.elapsed() >= DRAFT_REFRESH;
            if text == last && !refresh {
                shown = p.research_status;
                continue;
            }
            let update = self.update(target, &text, spacing).await;
            match update {
                Update::Sent => {
                    last = text;
                    sent = Instant::now();
                    shown = p.research_status;
                    next = sent + spacing;
                }
                Update::Wait(wait) => {
                    *retry_at = Some(Instant::now() + wait.max(Duration::from_secs(1)));
                }
                Update::Unavailable => return,
            }
        }
    }

    async fn update(&self, target: LiveMessage, text: &str, spacing: Duration) -> Update {
        match target {
            LiveMessage::Draft => self.draft(text).await,
            LiveMessage::Group(id) => match self.edit(id, text, Some(ParseMode::Html)).await {
                Ok(()) => Update::Sent,
                Err(teloxide::RequestError::RetryAfter(wait)) => Update::Wait(wait.duration()),
                Err(teloxide::RequestError::Network(_)) => Update::Wait(spacing),
                Err(_) => Update::Unavailable,
            },
        }
    }

    pub(crate) async fn show_stage(
        &self,
        target: LiveMessage,
        text: &str,
        retry_at: &mut Option<Instant>,
    ) {
        for _ in 0..crate::scheduler::SchedulerSettings::from_env()
            .retry_attempts
            .max(1)
        {
            if let Some(at) = retry_at.take() {
                tokio::time::sleep_until(at).await;
            }
            match self.update(target, text, GROUP_INTERVAL).await {
                Update::Wait(wait) => *retry_at = Some(Instant::now() + wait),
                _ => return,
            }
        }
    }

    // Replace the group's status with the first final chunk, and append subsequent
    // chunks. Retry only the chunk Telegram rejected. A deleted status can be replaced.
    pub(crate) async fn send(
        &self,
        rendered: RenderedTemplate,
        mut edit: Option<MessageId>,
    ) -> Result<MessageId> {
        let attempts = crate::scheduler::SchedulerSettings::from_env()
            .retry_attempts
            .max(1);
        let sender = BotTemplateSender::new(self.bot, self.templates);
        for attempt in 1..=attempts {
            if let Some(id) = edit {
                match self.edit(id, &rendered.text, rendered.parse_mode()).await {
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
                .send_rendered_to_thread(self.address.chat, self.address.thread, rendered.clone())
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

pub async fn command(
    bot: &Bot,
    msg: &Message,
    question: &str,
    state: &std::sync::Arc<BotState>,
) -> Result<()> {
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
    db.prompt_session(msg.chat.id.0, topic(msg).map(|id| id.0.0), reply.id.0)
        .await
}

pub async fn follow_up(bot: Bot, msg: Message, state: std::sync::Arc<BotState>) -> Result<()> {
    if let Some(session) = replied_session(&msg, &state.db).await?
        && let Some(question) = msg.text()
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
    let Some(slot) = crate::agent_access::admit(
        bot,
        msg.chat.id,
        topic(msg),
        crate::agent_access::sender(msg),
        "prompt",
        state,
    )
    .await?
    else {
        return Ok(());
    };
    let selection = state.ai_model().await?.to_string();
    let model = selection.as_str();
    let output = PromptOutput {
        bot: bot.clone(),
        msg: msg.clone(),
        state: state.clone(),
        question: question.to_owned(),
        model: model.to_owned(),
    };
    let initial = PromptProgress::default();
    let status = messages(&state.templates, model, &initial, question, "", true)?.remove(0);
    let last = status.text.clone();
    let target = output.live().start(status).await?;
    slot.send(Box::pin(deliver(output, session, target, last, initial)));
    Ok(())
}

async fn deliver(
    output: PromptOutput,
    session: PromptSession,
    target: LiveMessage,
    last: String,
    initial: PromptProgress,
) -> Result<()> {
    let state = &output.state;
    let msg = &output.msg;
    let model = output.model.as_str();
    let question = output.question.as_str();
    let (tx, rx) = watch::channel(initial);
    let history = &session.turns;
    let generate = async move {
        let key = rustverse_ai::api_key()?;
        rustverse_ai::continue_prompt(&state.beta.client, &key, model, question, history, tx).await
    };
    // Finish an in-flight Telegram edit before delivering the permanent answer,
    // so a delayed status update cannot overwrite it.
    let live = output.live();
    let mut retry_at = None;
    let (answer, ()) = tokio::join!(
        generate,
        live.updates(target, rx.clone(), last, &mut retry_at, |p| {
            Ok(
                messages(&state.templates, model, p, question, &p.text, true)?
                    .remove(0)
                    .text,
            )
        })
    );
    let mut stats = rx.borrow().clone();
    let parts = match &answer {
        Ok(text) => {
            stats.stage = PromptStage::Complete;
            messages(&state.templates, model, &stats, question, text, false)?
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
    let mut first_saved = None;
    if let Some(at) = retry_at {
        tokio::time::sleep_until(at).await;
    }
    for part in parts {
        let id = live.send(part, edit.take()).await?.0;
        if let Ok(text) = &answer {
            let saved = match first_saved {
                Some(first) => state.db.save_prompt_part(msg.chat.id.0, first, id).await,
                None => {
                    state
                        .db
                        .save_prompt_turn(
                            msg.chat.id.0,
                            topic(msg).map(|t| t.0.0),
                            id,
                            session.last_answer,
                            question,
                            text,
                        )
                        .await
                }
            };
            match saved {
                Ok(()) => {
                    first_saved.get_or_insert(id);
                }
                Err(error) => log::error!("Could not cache prompt message {id}: {error}"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_a_new_group_stage_bypasses_the_update_interval() {
        let (tx, mut rx) = watch::channel(PromptProgress::default());
        let group = LiveMessage::Group(MessageId(1));
        let later = Instant::now() + GROUP_INTERVAL;
        tx.send_modify(|p| {
            p.research_status = Some("Ищу правило".into());
            p.tools += 1;
        });
        let update = tokio::time::timeout(
            Duration::from_secs(1),
            next_update(&mut rx, group, None, later, None),
        )
        .await
        .unwrap()
        .unwrap();
        let shown = update.research_status.as_deref();
        tx.send_modify(|p| p.tools += 1);
        for (target, previous, limited) in [
            (group, shown, None),
            (LiveMessage::Draft, None, None),
            (group, None, Some(later)),
        ] {
            assert!(
                tokio::time::timeout(
                    Duration::ZERO,
                    next_update(&mut rx, target, previous, later, limited)
                )
                .await
                .is_err()
            );
        }
        tx.send_modify(|p| p.research_status = Some("Сравниваю версии".into()));
        let update = next_update(&mut rx, group, shown, later, Some(Instant::now()))
            .await
            .unwrap();
        assert_eq!(update.research_status.as_deref(), Some("Сравниваю версии"));
    }

    #[tokio::test]
    async fn ordinary_group_replies_find_the_answer_without_treating_reply_threads_as_topics() {
        let db = crate::db::Db::new_test().await.unwrap();
        db.save_prompt_turn(-100123, None, 42, None, "Первый вопрос", "Первый ответ")
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
            research_status: Some("Сверяю <A&B>".into()),
            tools: 4,
            turns: 3,
            tokens: Some(1300),
            cost_usd: Some(0.00284),
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
        assert!(!last.contains("Сверяю"));
        let researching = PromptProgress {
            stage: PromptStage::Researching,
            ..p.clone()
        };
        let live = messages(&templates, "model", &researching, question, "", true).unwrap();
        assert!(live[0].text.contains("<i>Сверяю &lt;A&amp;B&gt;</i>"));
        assert!(
            last.contains("Ответ готов")
                && last.contains("Вызовы инструментов: 4 · Ходы: 3 · Токены: 1300")
        );
        assert!(last.contains("Токены: 1300 · Стоимость: ≈ $0,00284"));
        for part in parts {
            let visible = scraper::Html::parse_fragment(&part.text)
                .root_element()
                .text()
                .collect::<String>();
            assert!(visible.encode_utf16().count() <= MESSAGE_LIMIT);
        }
        let unknown = PromptProgress {
            tokens: None,
            cost_usd: None,
            ..p
        };
        let footer = footer(&templates, "model<&>", &unknown).unwrap().text;
        assert!(footer.contains("Токены: — · Стоимость: —") && footer.contains("&lt;&amp;&gt;"));
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
