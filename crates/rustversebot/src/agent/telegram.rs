use anyhow::Result;
use rustverse_ai::{PromptProgress, PromptStage};
use serde_json::json;
use std::time::Duration;
use teloxide::{
    prelude::*,
    types::{MessageId, ParseMode, ThreadId},
};
use tokio::{sync::watch, time::Instant};

use crate::{
    bot_templates::{BotTemplateSender, RenderedTemplate},
    templates::TemplateEngine,
};

const DRAFT_INTERVAL: Duration = Duration::from_millis(1500);
const GROUP_INTERVAL: Duration = Duration::from_secs(15);
const DRAFT_REFRESH: Duration = Duration::from_secs(20);

#[derive(Clone, Copy)]
pub(crate) enum LiveMessage {
    Draft,
    Message(MessageId),
}

impl LiveMessage {
    fn new_research_stage(self, progress: &PromptProgress, shown: Option<&str>) -> bool {
        matches!(self, Self::Message(_))
            && progress.research_status.is_some()
            && progress.research_status.as_deref() != shown
    }
}

fn priority_stage(stage: PromptStage) -> Option<PromptStage> {
    matches!(
        stage,
        PromptStage::Compacting
            | PromptStage::Editing
            | PromptStage::Designing
            | PromptStage::Rendering
            | PromptStage::ReviewingImage
    )
    .then_some(stage)
}

async fn next_update(
    progress: &mut watch::Receiver<PromptProgress>,
    target: LiveMessage,
    shown: Option<&str>,
    shown_phase: Option<PromptStage>,
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
        changed = progress.wait_for(|p| {
            priority_stage(p.stage) != shown_phase
                || (priority_stage(p.stage).is_none() && target.new_research_stage(p, shown))
        }) => {
            changed.ok()?;
        }
        _ = tokio::time::sleep_until(next) => {}
    }
    if progress.has_changed().is_err() {
        return None;
    }
    Some(progress.borrow_and_update().clone())
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

// Ordinary supergroup replies may carry a reply-chain ID even outside a forum.
// Only a Telegram topic identifies a separate conversation within a chat.
pub(crate) fn topic(msg: &Message) -> Option<teloxide::types::ThreadId> {
    msg.thread_id
        .filter(|_| msg.is_topic_message || msg.chat.is_private())
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

    pub(crate) async fn start(
        &self,
        status: RenderedTemplate,
        allow_draft: bool,
    ) -> Result<LiveMessage> {
        if allow_draft
            && self.address.chat.is_user()
            && matches!(self.draft(&status.text).await, Update::Sent)
        {
            return Ok(LiveMessage::Draft);
        }
        let message = self.send(status, None).await?;
        Ok(LiveMessage::Message(message))
    }

    pub(crate) async fn updates(
        &self,
        target: LiveMessage,
        mut progress: watch::Receiver<PromptProgress>,
        last: &mut String,
        retry_at: &mut Option<Instant>,
        render: impl Fn(&PromptProgress) -> Result<String>,
    ) {
        let spacing = match target {
            LiveMessage::Draft => DRAFT_INTERVAL,
            LiveMessage::Message(_) => GROUP_INTERVAL,
        };
        let mut sent = Instant::now();
        let mut next = sent + spacing;
        let mut shown = None;
        let mut shown_phase = None;
        while let Some(mut p) = next_update(
            &mut progress,
            target,
            shown.as_deref(),
            shown_phase,
            next,
            *retry_at,
        )
        .await
        {
            *retry_at = None;
            next = Instant::now() + spacing;
            // A text delta can arrive in the same watch snapshot as a new stage.
            // Make sure the first edit for that stage actually displays its name.
            let phase = priority_stage(p.stage);
            if phase.is_none() && target.new_research_stage(&p, shown.as_deref()) {
                p.stage = PromptStage::Researching;
            }
            let Ok(text) = render(&p) else {
                return;
            };
            let refresh = matches!(target, LiveMessage::Draft) && sent.elapsed() >= DRAFT_REFRESH;
            if text == *last && !refresh {
                shown = p.research_status;
                shown_phase = phase;
                continue;
            }
            let update = self.update(target, &text, spacing).await;
            match update {
                Update::Sent => {
                    *last = text;
                    sent = Instant::now();
                    shown = p.research_status;
                    shown_phase = phase;
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
            LiveMessage::Message(id) => match self.edit(id, text, Some(ParseMode::Html)).await {
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
    ) -> bool {
        for _ in 0..crate::scheduler::SchedulerSettings::from_env()
            .retry_attempts
            .max(1)
        {
            if let Some(at) = retry_at.take() {
                tokio::time::sleep_until(at).await;
            }
            match self.update(target, text, GROUP_INTERVAL).await {
                Update::Wait(wait) => *retry_at = Some(Instant::now() + wait),
                Update::Sent => return true,
                Update::Unavailable => return false,
            }
        }
        false
    }

    pub(super) async fn delete(&self, id: MessageId) -> Result<()> {
        let settings = crate::scheduler::SchedulerSettings::from_env();
        let attempts = settings.retry_attempts.max(1);
        for attempt in 1..=attempts {
            match self.bot.delete_message(self.address.chat, id).await {
                Ok(_)
                | Err(teloxide::RequestError::Api(teloxide::ApiError::MessageToDeleteNotFound)) => {
                    return Ok(());
                }
                Err(teloxide::RequestError::RetryAfter(wait)) if attempt < attempts => {
                    tokio::time::sleep(wait.duration()).await
                }
                Err(teloxide::RequestError::Network(_)) if attempt < attempts => {
                    tokio::time::sleep(settings.retry_base_delay * (1 << (attempt - 1).min(8)))
                        .await
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("last delete attempt always returns")
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn compaction_and_proofreading_update_both_drafts_and_group_messages() {
        for target in [LiveMessage::Draft, LiveMessage::Message(MessageId(1))] {
            let (tx, mut rx) = watch::channel(PromptProgress::default());
            for (shown_phase, stage) in [
                (None, PromptStage::Compacting),
                (Some(PromptStage::Compacting), PromptStage::Preparing),
                (None, PromptStage::Editing),
            ] {
                tx.send_modify(|p| p.stage = stage);
                let next = next_update(
                    &mut rx,
                    target,
                    None,
                    shown_phase,
                    Instant::now() + GROUP_INTERVAL,
                    None,
                );
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(1), next)
                        .await
                        .unwrap()
                        .unwrap()
                        .stage,
                    stage
                );
            }
        }
    }

    #[tokio::test]
    async fn only_a_new_group_stage_bypasses_the_update_interval() {
        let (tx, mut rx) = watch::channel(PromptProgress::default());
        let group = LiveMessage::Message(MessageId(1));
        let later = Instant::now() + GROUP_INTERVAL;
        tx.send_modify(|p| {
            p.research_status = Some("Ищу правило".into());
            p.tools += 1;
        });
        let update = tokio::time::timeout(
            Duration::from_secs(1),
            next_update(&mut rx, group, None, None, later, None),
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
                    next_update(&mut rx, target, previous, None, later, limited)
                )
                .await
                .is_err()
            );
        }
        tx.send_modify(|p| p.research_status = Some("Сравниваю версии".into()));
        let update = next_update(&mut rx, group, shown, None, later, Some(Instant::now()))
            .await
            .unwrap();
        assert_eq!(update.research_status.as_deref(), Some("Сравниваю версии"));
    }
}
