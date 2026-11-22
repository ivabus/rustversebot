use std::sync::Mutex;

use opentelemetry::trace::{SpanContext, TraceContextExt};
use rig_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, InvalidToolCallAction,
    InvalidToolCallContext, ObservationAction, OutcomeAction, OutcomeEvent, ReasoningDelta,
    TextDelta, ToolCallDelta,
};
use rig_core::effect::{Answered, EffectKind};
use serde_json::Value;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// One hook per agent run. Rig dispatches all tools of a turn before the next generation.
#[derive(Default)]
pub(crate) struct GenerationTrace(Mutex<Option<Generation>>);

struct Generation {
    context: SpanContext,
    awaiting_delta: Option<Span>,
}

impl GenerationTrace {
    fn first_delta(&self, delta: &str) -> ObservationAction {
        if !delta.is_empty()
            && let Some(span) = self
                .0
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|g| g.awaiting_delta.take())
        {
            span.set_attribute(
                "langfuse.observation.completion_start_time",
                chrono::Utc::now().to_rfc3339(),
            );
        }
        ObservationAction::Continue
    }

    fn link_tool(&self, span: &Span) {
        if let Some(g) = self.0.lock().unwrap().as_ref() {
            span.set_attribute(
                "langfuse.observation.metadata.generation_id",
                g.context.span_id().to_string(),
            );
            span.add_link(g.context.clone());
        }
    }
}

impl AgentHook for GenerationTrace {
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let span = Span::current();
        match event.kind {
            EffectKind::Completion { stream, .. } => {
                let context = span.context().span().span_context().clone();
                *self.0.lock().unwrap() = Some(Generation {
                    context,
                    awaiting_delta: stream.then_some(span),
                });
            }
            EffectKind::ToolCall { .. } => self.link_tool(&span),
            _ => {}
        }
        DispatchAction::Proceed
    }

    async fn on_text_delta(&self, _: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        self.first_delta(event.delta)
    }

    async fn on_reasoning_delta(
        &self,
        _: &HookContext,
        event: ReasoningDelta<'_>,
    ) -> ObservationAction {
        self.first_delta(event.delta)
    }

    async fn on_tool_call_delta(
        &self,
        _: &HookContext,
        event: ToolCallDelta<'_>,
    ) -> ObservationAction {
        self.first_delta(event.delta)
    }

    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        match event.kind {
            EffectKind::Completion { .. } => {
                if let Some(g) = self.0.lock().unwrap().as_mut() {
                    g.awaiting_delta = None;
                }
            }
            EffectKind::ToolCall { name, .. } => {
                let status = match event.outcome {
                    Ok(Answered::ToolResult { result }) if result.is_success() => result
                        .output()
                        .as_json()
                        .map_or("success", |v| result_status(name, v)),
                    _ => "error",
                };
                Span::current()
                    .set_attribute("langfuse.observation.metadata.result_status", status);
            }
            _ => {}
        }
        OutcomeAction::Proceed
    }

    async fn on_invalid_tool_call(
        &self,
        _: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        // Rig can also ask about invalid calls during generation, before a tool span exists.
        let span = Span::current();
        if span.metadata().is_some_and(|m| m.name() == "execute_tool") {
            self.link_tool(&span);
            span.record("gen_ai.tool.name", &event.tool_name);
            if let Some(id) = &event.tool_call_id {
                span.record("gen_ai.tool.call.id", tracing::field::display(id));
            }
            span.record("gen_ai.tool.call.outcome", "skipped");
            span.set_attribute("langfuse.observation.metadata.result_status", "error");
        }
        None
    }
}

fn result_status(name: &str, value: &Value) -> &'static str {
    if value.get("error").is_some() {
        return "error";
    }
    if name == "get_endgame_seasons"
        && let Some(items) = value["results"].as_array()
    {
        let mut statuses = items
            .iter()
            .map(|item| result_status("get_endgame_season", item));
        let successes = statuses.clone().filter(|s| *s == "success").count();
        return match successes {
            0 if statuses.any(|s| s == "error") => "error",
            0 => "unavailable",
            n if n == items.len() => "success",
            _ => "partial",
        };
    }
    if name == "get_endgame_season" && value["status"] == "unavailable" {
        return "unavailable";
    }
    let rows = match name {
        "search_gachabase" => value.get("matches"),
        "get_gachabase_entry" => value.pointer("/result/matches"),
        "list_endgame_seasons" => value.get("seasons"),
        "list_recent_revisions" => Some(value),
        _ => None,
    };
    if rows.and_then(Value::as_array).is_some_and(Vec::is_empty) {
        "empty"
    } else {
        "success"
    }
}
