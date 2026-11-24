use teloxide::{
    RequestError,
    requests::{Output, Payload, Request},
};
use tracing::{Instrument, Span, field::Empty};

pub(crate) trait TraceRequest: Request<Err = RequestError> + Sized {
    fn traced(self) -> impl Future<Output = Result<Output<Self>, RequestError>> + Send {
        let span = tracing::info_span!(target: "rustverse::bot", "telegram.request",
            otel.kind = "client", rpc.method = Self::Payload::NAME, error.type = Empty, otel.status_code = Empty);
        let request = self.send();
        async move {
            let result = request.await;
            if let Err(error) = &result {
                let kind = match error {
                    RequestError::Api(_) => "api",
                    RequestError::RetryAfter(_) => "rate_limit",
                    RequestError::MigrateToChatId(_) => "chat_migrated",
                    RequestError::Network(_) => "network",
                    RequestError::InvalidJson { .. } => "invalid_response",
                    RequestError::Io(_) => "io",
                };
                Span::current()
                    .record("error.type", kind)
                    .record("otel.status_code", "ERROR");
            }
            result
        }
        .instrument(span)
    }
}

impl<R: Request<Err = RequestError>> TraceRequest for R {}
