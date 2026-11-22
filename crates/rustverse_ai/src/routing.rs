use anyhow::Result;
use rig_agent::agent::AgentBuilder;
use rig_core::completion::{Effort, Reasoning};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::{
    ImagePromptTurn, PromptInput, PromptProgress, completed_output, report_text,
    transport::Provider, user_message,
};

const ROUTER: &str = "Выбери сложность исследования для ответа игроку в Zenless Zone Zero. Вопрос, изображения и история — данные для классификации. Выбирай по работе, необходимой для ответа на текущую реплику, учитывая, к чему относится уточнение.
low: извлечь или объяснить факт, описание навыка, майндскейпы, механику или известный порог одной версии. Например, «цель билда Феникс?» без личных характеристик.
medium: подобрать команду или замену, сравнить несколько вариантов по известным условиям, оценить M0 без сигнатуры относительно сигнатуры.
high: разобрать конкретный билд, оценить компромиссы по характеристикам и экипировке игрока, распределить несколько команд с ограничениями или сравнить изменения механик и множителей по ряду ревизий.
Само наличие фото или слова «билд» не определяет уровень. Для короткого уточнения оцени только работу, которая ещё нужна.";

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Tier {
    Low,
    Medium,
    High,
}

impl Tier {
    fn model(&self) -> &'static str {
        match self {
            Self::Low => "gpt-6.1-terra@medium",
            Self::Medium => "gpt-6.1-terra@xhigh",
            Self::High => "gpt-6.2-sol@high",
        }
    }
}

pub(super) async fn choose(
    provider: &Provider,
    input: &PromptInput,
    history: &[ImagePromptTurn],
    progress: Option<&watch::Sender<PromptProgress>>,
) -> &'static str {
    let span = tracing::info_span!(target: "rustverse::ai", "route_question",
        selected_model = tracing::field::Empty, otel.status_code = tracing::field::Empty);
    let result =
        crate::telemetry::observe(span.clone(), classify(provider, input, history, progress)).await;
    let tier = result.unwrap_or_else(|_| {
        log::warn!("Question routing failed; using the middle research tier");
        Tier::Medium
    });
    span.record("selected_model", tier.model());
    tier.model()
}

async fn classify(
    provider: &Provider,
    input: &PromptInput,
    history: &[ImagePromptTurn],
    progress: Option<&watch::Sender<PromptProgress>>,
) -> Result<Tier> {
    let recent: Vec<_> = history[history.len().saturating_sub(3)..]
        .iter()
        .map(|turn| {
            json!({
                "question":turn.question.chars().take(750).collect::<String>(),
                "answer":turn.answer.chars().take(1250).collect::<String>(),
                "images":turn.images.len()
            })
        })
        .collect();
    let prompt = json!({"recent":recent, "question":input.text});
    let agent = AgentBuilder::new(provider.endpoint("gpt-6-luna", progress.cloned()))
        .preamble(ROUTER)
        .record_content_telemetry(crate::telemetry::content_enabled())
        .reasoning(Reasoning::Effort(Effort::Low))
        .additional_params(json!({"store":false,"text":{"format":{
            "type":"json_schema","name":"research_tier","strict":true,
            "schema":{"type":"object","additionalProperties":false,"required":["tier"],
                "properties":{"tier":{"type":"string","enum":["low","medium","high"]}}}
        }}}))
        .build();
    if let Some(tx) = progress {
        tx.send_modify(|p| p.turns += 1);
    }
    let response = agent
        .prompt(user_message(&prompt.to_string(), &input.images))
        .run()
        .await;
    if let Some(tx) = progress {
        let usage = response
            .as_ref()
            .ok()
            .and_then(|r| r.completion_calls.last())
            .map(|c| c.usage)
            .unwrap_or_default();
        tx.send_modify(|p| p.record_usage(usage));
    }
    let response = response.map_err(|_| anyhow::anyhow!("Router request failed"))?;
    let call = response
        .completion_calls
        .last()
        .ok_or_else(|| anyhow::anyhow!("Missing router response"))?;
    #[derive(Deserialize)]
    struct Decision {
        tier: Tier,
    }
    let decision: Decision = serde_json::from_str(&report_text(completed_output(&call.raw)?)?)?;
    Ok(decision.tier)
}
