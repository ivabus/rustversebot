//! Chat lifecycle for jobs using the rustverse_ai agent loop.
mod telegram;
use telegram::{LiveMessage, LiveOutput};
pub(crate) use telegram::{MessageAddress, topic};

use crate::{
    BotState, agent_access,
    bot_templates::{MESSAGE_LIMIT, RenderedTemplate},
    telegram_html,
    templates::TemplateEngine,
};
use anyhow::{Context, Result};
use rustverse_ai::{ModelSelection, PromptProgress, PromptStage};
use serde_json::{Value, json};
use std::{future::Future, sync::Arc};
use teloxide::{prelude::*, types::User};
use tokio::{sync::watch, time::Instant};

pub(crate) enum Status<'a> {
    Question(&'a str),
    Activity {
        template: &'static str,
        context: Value,
    },
}

impl Status<'_> {
    fn render(
        &self,
        templates: &TemplateEngine,
        model: &str,
        p: &PromptProgress,
    ) -> Result<RenderedTemplate> {
        match self {
            Self::Question(question) => {
                Ok(messages(templates, model, p, question, &p.text, true)?.remove(0))
            }
            Self::Activity { template, context } => {
                status_footer(templates, model, p, template, context)
            }
        }
    }
}

pub(crate) enum Outcome {
    Answer {
        text: String,
        image_ids: Vec<String>,
        parent: Option<i32>,
        author: Option<i64>,
        references: rustverse_ai::TurnReferences,
    },
    Notice(RenderedTemplate),
    MediaDelivered,
}

pub(crate) trait ChatAgent: Send + Sync + 'static {
    fn initial_status(&self) -> Status<'_>;
    fn execute(
        &self,
        run: &mut AgentRun<'_>,
        state: &BotState,
    ) -> impl Future<Output = Result<Outcome>> + Send;
}

pub(crate) struct AgentRun<'a> {
    output: LiveOutput<'a>,
    target: LiveMessage,
    model: ModelSelection,
    totals: PromptProgress,
    last: String,
    retry_at: Option<Instant>,
}

pub(crate) async fn launch(
    bot: &Bot,
    address: MessageAddress,
    user: Option<&User>,
    command: &str,
    state: &Arc<BotState>,
    agent: impl ChatAgent,
) -> Result<()> {
    let Some(slot) =
        agent_access::admit(bot, address.chat, address.thread, user, command, state).await?
    else {
        return Ok(());
    };
    let model = state.ai_model().await?;
    let totals = PromptProgress::default();
    let initial = agent.initial_status();
    let rendered = initial.render(&state.templates, &model.to_string(), &totals)?;
    let last = rendered.text.clone();
    let target = LiveOutput {
        bot,
        templates: &state.templates,
        address,
    }
    .start(rendered, matches!(initial, Status::Question(_)))
    .await?;
    let bot = bot.clone();
    let state = state.clone();
    let span = tracing::info_span!(target: "rustverse::ai", "telegram.command",
        otel.name = command, model = %model, source = "telegram",
        image_count = tracing::field::Empty, history_image_count = tracing::field::Empty,
        otel.status_code = tracing::field::Empty);
    slot.send(Box::pin(async move {
        let mut run = AgentRun {
            output: LiveOutput {
                bot: &bot,
                templates: &state.templates,
                address,
            },
            model,
            target,
            totals,
            last,
            retry_at: None,
        };
        let outcome = rustverse_ai::telemetry::observe(span, agent.execute(&mut run, &state)).await;
        run.finish(agent.initial_status(), outcome, &state.db).await
    }));
    Ok(())
}

impl AgentRun<'_> {
    pub fn bot(&self) -> &Bot {
        self.output.bot
    }
    pub fn address(&self) -> MessageAddress {
        self.output.address
    }
    pub fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub async fn show(&mut self, status: &Status<'_>) -> Result<()> {
        let rendered =
            status.render(self.output.templates, &self.model.to_string(), &self.totals)?;
        if rendered.text != self.last
            && self
                .output
                .show_stage(self.target, &rendered.text, &mut self.retry_at)
                .await
        {
            self.last = rendered.text;
        }
        Ok(())
    }

    /// Observe one invocation of the model loop. Join the last Telegram edit before
    /// advancing to another stage or publishing the result.
    pub async fn track<T, F: Future<Output = Result<T>>>(
        &mut self,
        status: Status<'_>,
        generate: impl FnOnce(watch::Sender<PromptProgress>) -> F,
    ) -> Result<T> {
        self.show(&status).await?;
        let (tx, rx) = watch::channel(PromptProgress::default());
        let model = self.model.to_string();
        let (result, ()) = tokio::join!(
            generate(tx),
            self.output.updates(
                self.target,
                rx.clone(),
                &mut self.last,
                &mut self.retry_at,
                |p| {
                    let mut progress = p.clone();
                    add_usage(&mut progress, &self.totals);
                    Ok(status
                        .render(self.output.templates, &model, &progress)?
                        .text)
                }
            )
        );
        add_usage(&mut self.totals, &rx.borrow());
        result
    }

    async fn finish(
        &mut self,
        status: Status<'_>,
        outcome: Result<Outcome>,
        db: &crate::db::Db,
    ) -> Result<()> {
        if let Some(at) = self.retry_at.take() {
            tokio::time::sleep_until(at).await;
        }
        let mut edit = match self.target {
            LiveMessage::Message(id) => Some(id),
            LiveMessage::Draft => None,
        };
        let templates = self.output.templates;
        let answer = match outcome {
            Ok(Outcome::MediaDelivered) => {
                if let Some(id) = edit {
                    self.output.delete(id).await?;
                }
                return Ok(());
            }
            Ok(Outcome::Notice(message)) => {
                self.output.send(message, edit).await?;
                return Ok(());
            }
            Ok(Outcome::Answer {
                text,
                image_ids,
                parent,
                author,
                references,
            }) => {
                self.totals.stage = PromptStage::Complete;
                (text, parent, author, references, image_ids)
            }
            Err(error) => {
                let message =
                    templates.render("error_generic", &json!({"error":error.to_string()}))?;
                if matches!(status, Status::Activity { .. }) {
                    self.output.send(message, edit).await?;
                    return Ok(());
                }
                self.totals.stage = PromptStage::Failed;
                (message.text, None, None, Default::default(), vec![])
            }
        };
        let Status::Question(question) = status else {
            anyhow::bail!("expected question for text answer")
        };
        let (text, parent, author, references, image_ids) = answer;
        let parts = messages(
            templates,
            &self.model.to_string(),
            &self.totals,
            question,
            &text,
            false,
        )?;
        let turn = crate::db::DeliveredTurn {
            question,
            image_ids: &image_ids,
            answer: &text,
            author,
            references,
        };
        let mut first_saved = None;
        let address = self.output.address;
        for part in parts {
            let id = self.output.send(part, edit.take()).await?.0;
            if self.totals.stage == PromptStage::Complete {
                let saved = match first_saved {
                    Some(first) => db.save_prompt_part(address.chat.0, first, id).await,
                    None => {
                        db.save_prompt_turn(
                            address.chat.0,
                            address.thread.map(|t| t.0.0),
                            id,
                            parent,
                            &turn,
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
}

fn add_usage(progress: &mut PromptProgress, other: &PromptProgress) {
    progress.tools += other.tools;
    progress.usage_reports += other.usage_reports;
    progress.tokens = progress
        .tokens
        .zip(other.tokens)
        .and_then(|(a, b)| a.checked_add(b));
    progress.cost_usd = match (progress.turns, other.turns) {
        (_, 0) => progress.cost_usd,
        (0, _) => other.cost_usd,
        _ => progress.cost_usd.zip(other.cost_usd).map(|(a, b)| a + b),
    };
    progress.turns += other.turns;
}

fn footer(
    templates: &TemplateEngine,
    model: &str,
    progress: &PromptProgress,
) -> Result<RenderedTemplate> {
    status_footer(templates, model, progress, "prompt_status", &Value::Null)
}

fn status_footer(
    templates: &TemplateEngine,
    model: &str,
    progress: &PromptProgress,
    template: &str,
    context: &Value,
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
        template,
        &json!({"model":model,"p":progress,"cost":cost,"activity":context}),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rendering_status_keeps_total_usage_across_agents_and_cache_hits() {
        let templates = crate::templates::TemplateEngine::new().unwrap();
        let mut totals = PromptProgress::default();
        let usage = PromptProgress {
            turns: 2,
            tools: 3,
            tokens: Some(150),
            cost_usd: Some(0.025),
            ..Default::default()
        };
        add_usage(&mut totals, &usage);
        add_usage(&mut totals, &usage);
        add_usage(&mut totals, &PromptProgress::default());
        let rendered = status_footer(
            &templates,
            "model",
            &totals,
            "diff_status",
            &json!({"previous":"beta_1", "current":"beta_2", "stage":"render", "character":"A&B"}),
        )
        .unwrap();
        assert!(
            rendered
                .text
                .contains("<b>A&amp;B</b>\n<i>Рендеринг карточки…</i>")
        );
        assert!(rendered.text.starts_with(
            "<b>Создаю дифф между версиями</b> · <code>beta_1</code> → <code>beta_2</code>"
        ));
        assert!(
            rendered
                .text
                .contains("Вызовы инструментов: 6 · Ходы: 4 · Токены: 300 · Стоимость: ≈ $0,05")
        );
        add_usage(
            &mut totals,
            &PromptProgress {
                turns: 1,
                tokens: None,
                cost_usd: None,
                ..Default::default()
            },
        );
        let done = footer(&templates, "model", &totals).unwrap();
        assert!(done.text.contains("Токены: — · Стоимость: —"));
    }
    #[test]
    fn compacting_stage_temporarily_replaces_the_previous_research_status() {
        let templates = TemplateEngine::new().unwrap();
        let mut p = PromptProgress {
            stage: PromptStage::Compacting,
            research_status: Some("Ищу босса".into()),
            ..Default::default()
        };
        for template in ["prompt_status", "diff_status"] {
            let rendered = status_footer(
                &templates,
                "model",
                &p,
                template,
                &json!({"stage":"generate"}),
            )
            .unwrap();
            assert!(rendered.text.contains("Сжимаю контекст…"));
            assert!(!rendered.text.contains("Ищу босса"));
        }
        p.stage = PromptStage::Complete;
        let final_text = footer(&templates, "model", &p).unwrap().text;
        assert!(
            !["Сжимаю контекст", "Пик входа", "Сжатия:"]
                .iter()
                .any(|s| final_text.contains(s))
        );
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
