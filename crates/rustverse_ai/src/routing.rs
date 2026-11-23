use anyhow::{Context, Result, bail, ensure};
use rig_core::{
    catalog::ModelFacts,
    completion::Usage,
    message::{DocumentSourceKind, MimeType},
};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::{Span, field::Empty};

use crate::{ImagePromptTurn, PromptInput, PromptProgress, transport::Provider};

const MODEL: &str = "gpt-6-luna";
const QUESTION: &str = include_str!("research-tier.json");
const MEDIUM_TIER: (&str, &str) = ("medium", "gpt-6.2-sol@medium");

fn payload(input: &PromptInput, history: &[ImagePromptTurn]) -> Result<String> {
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
    let text = json!({"recent":recent,"question":input.text}).to_string();
    let mut content = vec![json!({"type":"input_text","text":text})];
    for image in &input.images {
        let DocumentSourceKind::Base64(data) = &image.0.data else {
            bail!("Expected inline router image")
        };
        let mime = image
            .0
            .media_type
            .as_ref()
            .context("Missing router image format")?
            .to_mime_type();
        content
            .push(json!({"type":"input_image","image_url":format!("data:{mime};base64,{data}")}));
    }
    let question: Value = serde_json::from_str(QUESTION)?;
    Ok(
        json!({"model":MODEL,"input":[{"role":"user","content":content}],"questions":[question]})
            .to_string(),
    )
}

fn decision_usage(raw: &Value) -> Usage {
    let mut usage = Usage::new()
        .input_tokens(raw["input_tokens"].as_u64())
        .cached_input_tokens(
            raw.pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
        )
        .cache_creation_input_tokens(
            raw.pointer("/input_tokens_details/cache_write_tokens")
                .and_then(Value::as_u64),
        )
        .output_tokens(0);
    // Decisions bills input (including cache reads and writes), with free output.
    usage.cost = ModelFacts::default()
        .for_model("openai", MODEL)
        .and_then(|s| s.pricing.as_ref()?.cost(&usage));
    usage.output_tokens = raw["output_tokens"].as_u64();
    usage.total_tokens = raw["total_tokens"].as_u64();
    usage
}

pub(super) async fn choose(
    provider: &Provider,
    input: &PromptInput,
    history: &[ImagePromptTurn],
    progress: Option<&watch::Sender<PromptProgress>>,
) -> &'static str {
    let span = tracing::info_span!(target: "rustverse::ai", "route_question",
        langfuse.observation.type = "generation", gen_ai.operation.name = "decisions",
        gen_ai.provider.name = "openai", gen_ai.request.model = MODEL, gen_ai.response.model = Empty,
        langfuse.observation.input = Empty, langfuse.observation.output = Empty,
        langfuse.observation.metadata.tier = Empty, selected_model = Empty,
        langfuse.observation.metadata.fallback = Empty, langfuse.observation.status_message = Empty,
        gen_ai.usage.input_tokens = Empty, gen_ai.usage.output_tokens = Empty,
        gen_ai.usage.cache_read.input_tokens = Empty, gen_ai.usage.cache_creation.input_tokens = Empty,
        gen_ai.usage.cost = Empty, otel.status_code = Empty);
    if let Some(tx) = progress {
        tx.send_modify(|p| p.turns += 1);
    }
    let mut usage = Usage::new();
    let decision = crate::telemetry::observe(span.clone(), async {
        let body = payload(input, history)?;
        Span::current().record("langfuse.observation.input", body.as_str());
        let response = provider.decide(body).await?;
        usage = decision_usage(&response["usage"]);
        span.record("gen_ai.usage.input_tokens", usage.input_tokens);
        span.record("gen_ai.usage.output_tokens", usage.output_tokens);
        span.record(
            "gen_ai.usage.cache_read.input_tokens",
            usage.cached_input_tokens,
        );
        span.record(
            "gen_ai.usage.cache_creation.input_tokens",
            usage.cache_creation_input_tokens,
        );
        span.record(
            "gen_ai.usage.cost",
            usage.cost.filter(|c| c.is_complete()).map(|c| c.total),
        );
        span.record("gen_ai.response.model", response["model"].as_str());
        let answers = response["answers"]
            .as_array()
            .context("Missing Decisions answers")?;
        let [answer] = answers.as_slice() else {
            bail!("Expected one Decisions answer")
        };
        if !span.is_disabled() {
            span.record("langfuse.observation.output", answer.to_string());
        }
        ensure!(
            answer["name"] == "research_tier",
            "Unexpected Decisions answer name"
        );
        ensure!(
            answer["type"] != "refusal",
            "Decisions refused classification"
        );
        ensure!(answer["type"] == "choice", "Expected a Decisions choice");
        Ok(match answer["choice"].as_str() {
            Some("low") => ("low", "gpt-6.1-terra@high"),
            Some("medium") => MEDIUM_TIER,
            Some("high") => ("high", "gpt-6.2-sol@high"),
            _ => bail!("Unknown Decisions research tier"),
        })
    })
    .await;
    if let Some(tx) = progress {
        tx.send_modify(|p| p.record_usage(usage));
    }
    span.record("langfuse.observation.metadata.fallback", decision.is_err());
    let (tier, model) = decision.unwrap_or_else(|err| {
        span.record("langfuse.observation.status_message", err.to_string());
        log::warn!("Question routing failed; using the middle research tier: {err}");
        MEDIUM_TIER
    });
    span.record("langfuse.observation.metadata.tier", tier);
    span.record("selected_model", model);
    model
}
