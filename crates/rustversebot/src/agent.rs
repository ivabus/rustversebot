//! Chat lifecycle for jobs using the rustverse_ai agent loop.
pub(crate) mod cancel;
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
use rustverse_ai::{PromptProgress, PromptStage};
use serde_json::{Value, json};
use std::{future::Future, sync::Arc};
use teloxide::{prelude::*, types::User};
use tokio::{sync::watch, time::Instant};

pub(crate) enum Status<'a> {
    Question {
        text: &'a str,
        session_id: uuid::Uuid,
    },
    Activity {
        template: &'static str,
        context: Value,
    },
}

impl Status<'_> {
    fn trace(&self, command: &'static str, model: &str) -> tracing::Span {
        use tracing::field::Empty;
        let span = tracing::info_span!(target: "rustverse::ai", "telegram.command",
            otel.name = command, langfuse.observation.type = "span", selected_model = model,
            source = "telegram", image_count = Empty, history_image_count = Empty,
            langfuse.observation.input = Empty, langfuse.observation.output = Empty,
            otel.status_code = Empty);
        if let Self::Question { text, session_id } = self {
            rustverse_ai::telemetry::set_prompt_session(&span, session_id.to_string());
            if rustverse_ai::telemetry::content_enabled() {
                span.record("langfuse.observation.input", text);
            }
        }
        span
    }

    fn render(
        &self,
        templates: &TemplateEngine,
        model: &str,
        p: &PromptProgress,
    ) -> Result<RenderedTemplate> {
        match self {
            Self::Question { text, .. } => Ok(messages(templates, model, p, text, None)?.remove(0)),
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
    Delivered,
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
    model: String,
    totals: PromptProgress,
    last: String,
    retry_at: Option<Instant>,
}

pub(crate) async fn launch(
    bot: &Bot,
    address: MessageAddress,
    user: Option<&User>,
    command: &'static str,
    state: &Arc<BotState>,
    agent: impl ChatAgent,
) -> Result<()> {
    let Some(slot) =
        agent_access::admit(bot, address.chat, address.thread, user, command, state).await?
    else {
        return Ok(());
    };
    let totals = PromptProgress::default();
    let initial = agent.initial_status();
    let model = match initial {
        Status::Question { .. } => state.research_setting().await?,
        Status::Activity { .. } => state.ai_model().await?.to_string(),
    };
    let rendered = initial.render(&state.templates, &model, &totals)?;
    let last = rendered.text.clone();
    let target = LiveOutput {
        bot,
        templates: &state.templates,
        address,
    }
    .start(rendered, matches!(initial, Status::Question { .. }))
    .await?;
    let bot = bot.clone();
    let state = state.clone();
    let registration = match initial {
        Status::Question { .. } => Some(state.prompts.register(
            address,
            target,
            user.context("Missing prompt author")?.id,
        )),
        Status::Activity { .. } => None,
    };
    let cancellation = registration
        .as_ref()
        .map(|(guard, abort, _)| (guard.id, abort.clone()));
    slot.send(
        cancellation,
        Box::pin(async move {
            let span = agent.initial_status().trace(command, &model);
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
            let generate = async {
                let outcome =
                    rustverse_ai::telemetry::observe(span, agent.execute(&mut run, &state)).await;
                if let Some(at) = run.retry_at.take() {
                    tokio::time::sleep_until(at).await;
                }
                outcome
            };
            let outcome = if let Some((guard, _, signal)) = registration {
                let Ok(outcome) = futures_util::future::Abortable::new(generate, signal).await
                else {
                    tracing::Span::current().record("outcome", "cancelled");
                    return Ok(());
                };
                if !guard.claim() {
                    tracing::Span::current().record("outcome", "cancelled");
                    return Ok(());
                }
                outcome
            } else {
                generate.await
            };
            let generation_failed = outcome.is_err();
            let delivered = run.finish(agent.initial_status(), outcome, &state.db).await;
            if generation_failed || delivered.is_err() {
                tracing::Span::current().record("otel.status_code", "ERROR");
            }
            tracing::Span::current().record(
                "outcome",
                if delivered.is_err() {
                    "delivery_failed"
                } else if generation_failed {
                    "generation_failed"
                } else {
                    "delivered"
                },
            );
            delivered
        }),
    );
    Ok(())
}

impl AgentRun<'_> {
    pub fn bot(&self) -> &Bot {
        self.output.bot
    }
    pub fn address(&self) -> MessageAddress {
        self.output.address
    }
    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn show(&mut self, status: &Status<'_>) -> Result<()> {
        let rendered = status.render(self.output.templates, &self.model, &self.totals)?;
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
        let (tx, rx) = watch::channel(PromptProgress::default());
        self.track_progress(status, rx, generate(tx)).await
    }

    /// Accumulate this report's usage after its final status update.
    pub async fn track_progress<T>(
        &mut self,
        status: Status<'_>,
        rx: watch::Receiver<PromptProgress>,
        result: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if rx.has_changed().is_ok() {
            self.show(&status).await?;
        }
        let model = &self.model;
        let (result, ()) = tokio::join!(
            result,
            self.output.updates(
                self.target,
                rx.clone(),
                &mut self.last,
                &mut self.retry_at,
                |p| {
                    let mut progress = p.clone();
                    add_usage(&mut progress, &self.totals);
                    Ok(status
                        .render(
                            self.output.templates,
                            progress.research_model.as_deref().unwrap_or(model),
                            &progress,
                        )?
                        .text)
                }
            )
        );
        add_usage(&mut self.totals, &rx.borrow());
        if let Some(selected) = &rx.borrow().research_model {
            self.model.clone_from(selected);
        }
        result
    }

    async fn finish(
        &mut self,
        status: Status<'_>,
        outcome: Result<Outcome>,
        db: &crate::db::Db,
    ) -> Result<()> {
        let mut edit = match self.target {
            LiveMessage::Message(id) => Some(id),
            LiveMessage::Draft => None,
        };
        let templates = self.output.templates;
        let answer = match outcome {
            Ok(Outcome::Delivered) => {
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
        let Status::Question {
            text: question,
            session_id,
        } = status
        else {
            anyhow::bail!("expected question for text answer")
        };
        let (text, parent, author, references, image_ids) = answer;
        let parts = messages(templates, &self.model, &self.totals, question, Some(&text))?;
        let turn = crate::db::DeliveredTurn {
            session_id,
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
    answer: Option<&str>,
) -> Result<Vec<RenderedTemplate>> {
    let footer = footer(templates, model, progress)?;
    let visible_len = |html: &str| {
        scraper::Html::parse_fragment(html)
            .root_element()
            .text()
            .map(|text| text.encode_utf16().count())
            .sum::<usize>()
    };
    let footer_space = visible_len(&footer.text) + 2;
    let mut parts = match answer {
        None => {
            let limit = MESSAGE_LIMIT
                .checked_sub(footer_space)
                .filter(|n| *n >= 2)
                .context("Статус превышает лимит сообщения")?;
            vec![telegram_html::question_preview(question, limit)]
        }
        Some(text) if progress.stage == PromptStage::Failed => {
            telegram_html::plain_chunks(question, text, MESSAGE_LIMIT)
        }
        Some(text) => telegram_html::chunks(question, text, MESSAGE_LIMIT),
    };
    parts.retain(|s| !s.trim().is_empty());
    if parts.is_empty() {
        return Ok(vec![footer]);
    }
    if parts
        .last()
        .is_some_and(|part| visible_len(part) + footer_space > MESSAGE_LIMIT)
    {
        parts.push(String::new());
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
            &json!({"previous":"beta_1", "current":"beta_2", "stage":"send"}),
        )
        .unwrap();
        assert!(rendered.text.contains("<i>Отправляю отчёт…</i>"));
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
        let status = messages(&templates, "model<&>", &initial, question, None).unwrap();
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
            Some(&format!("**{}**", "🌙".repeat(6200))),
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
        let live = messages(&templates, "model", &researching, question, None).unwrap();
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
    fn a_full_length_paragraph_stays_together_when_the_footer_needs_another_message() {
        let templates = TemplateEngine::new().unwrap();
        let progress = PromptProgress {
            stage: PromptStage::Complete,
            ..Default::default()
        };
        let paragraph = "🌙".repeat(MESSAGE_LIMIT / 2);
        let parts = messages(&templates, "model", &progress, "Вопрос", Some(&paragraph)).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].text, "<blockquote>Вопрос</blockquote>");
        assert_eq!(parts[1].text, paragraph);
        assert!(parts[2].text.contains("Ответ готов"));
    }

    #[test]
    fn live_question_status_hides_answer_text_until_delivery() {
        let templates = TemplateEngine::new().unwrap();
        let status = Status::Question {
            text: "Как собрать команду?",
            session_id: uuid::Uuid::nil(),
        };
        for (stage, label, text) in [
            (PromptStage::Writing, "Пишу черновик…", "Первый черновик"),
            (PromptStage::Editing, "Вычитываю ответ…", "Первый черновик"),
            (
                PromptStage::FactChecking,
                "Проверяю факты…",
                "Проверяемый ответ",
            ),
            (
                PromptStage::Editing,
                "Вычитываю ответ…",
                "Проверенный ответ",
            ),
        ] {
            let progress = PromptProgress {
                stage,
                text: text.into(),
                ..Default::default()
            };
            let rendered = status.render(&templates, "model", &progress).unwrap();
            assert!(rendered.text.contains(label));
            assert!(rendered.text.contains("Как собрать команду?"));
            assert!(!rendered.text.contains(text));
            if matches!(stage, PromptStage::Editing | PromptStage::FactChecking) {
                let report = status_footer(
                    &templates,
                    "model",
                    &progress,
                    "diff_status",
                    &json!({"stage":"report"}),
                )
                .unwrap();
                assert!(report.text.contains(label));
                assert!(!report.text.contains(text));
            }
        }
    }

    #[test]
    fn long_question_is_abbreviated_in_preview_and_preserved_in_full_in_the_answer() {
        let templates = TemplateEngine::new().unwrap();
        let question = "🌙<&>".repeat(1250);
        let p = PromptProgress::default();
        let status = messages(&templates, "model", &p, &question, None).unwrap();
        assert!(status[0].text.contains("…</blockquote>"));
        let selector = scraper::Selector::parse("blockquote").unwrap();
        for stage in [PromptStage::Complete, PromptStage::Failed] {
            let p = PromptProgress {
                stage,
                ..PromptProgress::default()
            };
            let parts = messages(&templates, "model", &p, &question, Some("Текст")).unwrap();
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
