use rig_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, ObservationAction, TextDelta,
};
use rig_core::effect::EffectFamily;
use serde::Serialize;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStage {
    Preparing,
    Researching,
    Writing,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PromptProgress {
    pub stage: PromptStage,
    pub turns: usize,
    pub tools: usize,
    /// Total confirmed input and output tokens across completed model turns.
    /// None if the provider omitted usage for any completed turn.
    pub tokens: Option<u64>,
    #[serde(skip)]
    pub text: String,
}

impl Default for PromptProgress {
    fn default() -> Self {
        Self {
            stage: PromptStage::Preparing,
            turns: 0,
            tools: 0,
            tokens: Some(0),
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
            && event.kind.family() == EffectFamily::Tool
        {
            tx.send_modify(|p| {
                p.tools += 1;
                p.stage = PromptStage::Researching;
                p.text.clear();
            });
        }
        DispatchAction::Proceed
    }

    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if let Self::Progress(tx) = self {
            tx.send_modify(|p| {
                p.tokens = p
                    .tokens
                    .zip(event.usage.total_tokens)
                    .and_then(|(a, b)| a.checked_add(b));
            });
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
