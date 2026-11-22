use crate::context::InputBudget;
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::Mutex;

use rig_core::{
    error::{ErrorReport, ProviderError},
    http_client::{
        Bytes, HttpClientExt, LazyBody, MultipartForm, Request, Response, Result, StreamingResponse,
    },
};
use rig_reqwest::{ReqwestClient, reqwest};

/// Retry a rejected Responses request in place, retaining the agent's transcript.
#[derive(Clone)]
pub(super) struct ReportHttp {
    client: rig_core::http_client::DynHttpClient,
    budget: Arc<Mutex<InputBudget>>,
}

pub(super) struct Provider {
    pub config: rig_core::providers::openai::OpenAIConfig,
    pub client: rig_core::http_client::DynHttpClient,
}

impl Provider {
    pub(super) fn endpoint(
        self,
        selection: &crate::ModelSelection,
        progress: Option<tokio::sync::watch::Sender<crate::PromptProgress>>,
    ) -> rig_core::Model<rig_core::providers::openai::responses_api::wire::Responses> {
        let http = crate::DynHttpClient::new(ReportHttp::new(
            self.client,
            InputBudget::new(&selection.slug, progress),
        ));
        let wire = rig_core::providers::openai::responses_api::wire::Responses::new(
            self.config,
            selection.slug.clone(),
        )
        .with_strict_tools();
        rig_core::Model::new(wire, http)
    }

    pub fn new(key: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            config: rig_core::providers::openai::OpenAIConfig::new(key.to_owned()),
            client: rig_core::http_client::DynHttpClient::new(ReqwestClient::from(client)),
        })
    }
}

#[allow(
    clippy::result_large_err,
    reason = "Rig requires its transport error, including Retry-After headers"
)]
pub(super) async fn retry_rejected<T, F: Future<Output = Result<T>>>(
    mut send: impl FnMut() -> F,
) -> Result<T> {
    for seconds in [1, 2] {
        let err = match send().await {
            Err(err) if err.is_transient() => err,
            other => return other,
        };
        let Some(status) = err.non_success_status() else {
            return Err(err);
        };
        let report = ErrorReport::from(
            ProviderError::from_http_response(status, err.non_success_body().unwrap_or_default())
                .with_response_headers(err.non_success_headers().cloned()),
        );
        let wait = report
            .retry_after(SystemTime::now())
            .unwrap_or(Duration::from_secs(seconds));
        if !report.is_retryable() || wait > Duration::from_secs(60) {
            return Err(err);
        }
        tokio::time::sleep(wait).await;
    }
    send().await
}

pub(super) fn context_rejected(error: &rig_core::http_client::Error) -> bool {
    error
        .non_success_body()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .is_some_and(|body| {
            body.pointer("/error/code")
                .is_some_and(|code| code == "context_length_exceeded")
        })
}

impl ReportHttp {
    pub fn new(client: rig_core::http_client::DynHttpClient, budget: InputBudget) -> Self {
        Self {
            client,
            budget: Arc::new(Mutex::new(budget)),
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "Rig expects its original transport error with provider headers"
    )]
    async fn send_response<T, F: Future<Output = Result<T>>>(
        &self,
        request: Request<Bytes>,
        send: impl Fn(Request<Bytes>) -> F,
    ) -> Result<T> {
        let mut budget = self.budget.lock().await;
        loop {
            let (prepared, count) = budget
                .prepare(&self.client, request.clone())
                .await
                .map_err(rig_core::http_client::Error::permanent)?;
            match retry_rejected(|| send(prepared.clone())).await {
                Err(err) if context_rejected(&err) => budget.restrict(count),
                result => return result,
            }
        }
    }
}

impl HttpClientExt for ReportHttp {
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<LazyBody>>> + Send + 'static {
        let transport = self.clone();
        async move {
            transport
                .send_response(request, |r| transport.client.send(r))
                .await
        }
    }

    fn send_multipart(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody>>> + Send + 'static {
        self.client.send_multipart(request)
    }

    async fn send_streaming(&self, request: Request<Bytes>) -> Result<StreamingResponse> {
        self.send_response(request, |r| self.client.send_streaming(r))
            .await
    }
}
