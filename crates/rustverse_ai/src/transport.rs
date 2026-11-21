use std::time::{Duration, SystemTime};

use rig_core::{
    error::{ErrorReport, ProviderError},
    http_client::{
        Bytes, HttpClientExt, LazyBody, MultipartForm, Request, Response, Result, StreamingResponse,
    },
};
use rig_reqwest::{ReqwestClient, reqwest};

/// Retry a rejected Responses request in place, retaining the agent's transcript.
pub(super) struct ReportHttp(ReqwestClient);

impl ReportHttp {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self(ReqwestClient::from(client)))
    }
}

#[allow(
    clippy::result_large_err,
    reason = "Rig requires its transport error, including Retry-After headers"
)]
async fn retry_rejected<T, F: Future<Output = Result<T>>>(
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

impl HttpClientExt for ReportHttp {
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<LazyBody>>> + Send + 'static {
        let client = self.0.clone();
        async move { retry_rejected(|| client.send(request.clone())).await }
    }

    fn send_multipart(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody>>> + Send + 'static {
        self.0.send_multipart(request)
    }

    async fn send_streaming(&self, request: Request<Bytes>) -> Result<StreamingResponse> {
        retry_rejected(|| self.0.send_streaming(request.clone())).await
    }
}
