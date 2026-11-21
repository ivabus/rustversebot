use rig_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, ObservationAction, TextDelta,
};
use rig_core::effect::EffectKind;
use serde::Serialize;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStage {
    Preparing,
    Compacting,
    Researching,
    Writing,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PromptProgress {
    pub stage: PromptStage,
    pub turns: usize,
    #[serde(skip)]
    pub usage_reports: usize,
    pub tools: usize,
    pub research_status: Option<String>,
    /// Total confirmed input and output tokens across completed model turns.
    /// None if the provider omitted usage for any completed turn.
    pub tokens: Option<u64>,
    /// Estimated USD for completed turns in this request, if every turn has a complete cost.
    pub cost_usd: Option<f64>,
    #[serde(skip)]
    pub text: String,
}

impl Default for PromptProgress {
    fn default() -> Self {
        Self {
            stage: PromptStage::Preparing,
            turns: 0,
            usage_reports: 0,
            tools: 0,
            research_status: None,
            tokens: Some(0),
            cost_usd: None,
            text: String::new(),
        }
    }
}

pub(super) enum Preview {
    Text(watch::Sender<String>),
    Progress(watch::Sender<PromptProgress>),
}

impl AgentHook for Preview {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if let Self::Progress(tx) = self {
            tx.send_modify(|p| {
                p.turns = event.turn;
                p.stage = PromptStage::Preparing;
                p.text.clear();
            });
        }
        CompletionCallAction::Continue
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let Self::Progress(tx) = self
            && let EffectKind::ToolCall { args, .. } = event.kind
        {
            tx.send_modify(|p| p.record_tool_call(args));
        }
        DispatchAction::Proceed
    }

    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if let Self::Progress(tx) = self {
            tx.send_modify(|p| p.record_usage(event.usage));
        }
        ModelTurnAction::Continue
    }

    async fn on_text_delta(&self, _: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        let text = event.aggregated.chars().take(4096).collect();
        match self {
            Self::Text(tx) => {
                tx.send_replace(text);
            }
            Self::Progress(tx) => tx.send_modify(|p| {
                p.stage = PromptStage::Writing;
                p.text = text;
            }),
        }
        ObservationAction::Continue
    }
}

impl PromptProgress {
    pub(crate) fn record_usage(&mut self, usage: rig_core::completion::Usage) {
        self.tokens = self
            .tokens
            .zip(usage.total_tokens)
            .and_then(|(a, b)| a.checked_add(b));
        let previous = if self.usage_reports == 0 {
            Some(0.0)
        } else {
            self.cost_usd
        };
        self.usage_reports += 1;
        self.cost_usd = previous
            .zip(
                usage
                    .cost
                    .filter(|cost| cost.is_complete() && cost.total >= 0.0),
            )
            .map(|(sum, cost)| sum + cost.total)
            .filter(|sum| sum.is_finite());
    }

    fn record_tool_call(&mut self, args: &str) {
        self.tools += 1;
        self.stage = PromptStage::Researching;
        self.text.clear();
        if let Some(status) = research_status(args) {
            self.research_status = Some(status);
        }
    }
}

fn research_status(args: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Status {
        research_status: Option<String>,
    }
    let status: Status = serde_json::from_str(args).ok()?;
    let line = status
        .research_status?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return None;
    }
    let mut chars = line.chars();
    let mut short: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        short.pop();
        short.push('…');
    }
    Some(short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_cost_requires_pricing_for_every_completed_turn() {
        use rig_core::completion::{Cost, Usage};
        let mut progress = PromptProgress::default();
        let priced = |value| Usage::new().cost(Cost::from_total(value));
        assert_eq!(progress.cost_usd, None);
        progress.record_usage(priced(0.25));
        progress.record_usage(priced(0.5));
        assert_eq!(progress.cost_usd, Some(0.75));
        progress.record_usage(Usage::new());
        progress.record_usage(priced(0.125));
        assert_eq!(progress.cost_usd, None);
        let mut free = PromptProgress::default();
        free.record_usage(priced(0.0));
        assert_eq!(free.cost_usd, Some(0.0));
    }

    #[test]
    fn tool_calls_keep_the_current_research_objective_until_a_transition() {
        let mut p = PromptProgress::default();
        p.record_tool_call(r#"{"research_status":"Уточняю правила Refringe"}"#);
        for args in [
            r#"{"research_status":null}"#,
            "{}",
            r#"{"research_status":" "}"#,
        ] {
            p.record_tool_call(args);
            assert_eq!(
                p.research_status.as_deref(),
                Some("Уточняю правила Refringe")
            );
        }
        p.record_tool_call(r#"{"research_status":"Сравниваю урон по ревизиям"}"#);
        assert_eq!(
            p.research_status.as_deref(),
            Some("Сравниваю урон по ревизиям")
        );
        assert_eq!(p.tools, 5);
    }
}
