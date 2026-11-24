use teloxide::{
    RequestError,
    requests::{Output, Payload, Request},
};
use tracing::{Instrument, Span, field::Empty};

pub(crate) trait TraceRequest: Request<Err = RequestError> + Sized {
    fn traced(self) -> impl Future<Output = Result<Output<Self>, RequestError>> + Send {
        let span = tracing::info_span!(target: "rustverse::bot", "telegram.request",
            otel.kind = "client", rpc.method = Self::Payload::NAME, error.type = Empty, error.message = Empty, otel.status_code = Empty);
        let request = self.send();
        async move {
            let result = request.await;
            if let Err(error) = &result {
                let (kind, message) = match error {
                    RequestError::Api(teloxide::ApiError::Unknown(_)) => {
                        ("api", "Telegram отклонил запрос.".into())
                    }
                    RequestError::Api(teloxide::ApiError::CantParseEntities(_)) => (
                        "invalid_entities",
                        "Telegram не смог разобрать разметку сообщения.".into(),
                    ),
                    // All other ApiError variants have fixed descriptions without payloads.
                    RequestError::Api(known) => ("api", known.to_string()),
                    RequestError::RetryAfter(wait) => (
                        "rate_limit",
                        format!(
                            "Превышен лимит Telegram. Повтор через {} с.",
                            wait.seconds()
                        ),
                    ),
                    RequestError::MigrateToChatId(_) => (
                        "chat_migrated",
                        "Чат Telegram преобразован в супергруппу.".into(),
                    ),
                    RequestError::Network(e) if e.is_timeout() => {
                        ("timeout", "Истекло время ожидания Telegram.".into())
                    }
                    RequestError::Network(_) => (
                        "network",
                        "Не удалось выполнить сетевой запрос к Telegram.".into(),
                    ),
                    RequestError::InvalidJson { .. } => (
                        "invalid_response",
                        "Некорректный JSON в ответе Telegram.".into(),
                    ),
                    RequestError::Io(e) => (
                        "io",
                        format!("Ошибка ввода-вывода при отправке файла: {}.", e.kind()),
                    ),
                };
                Span::current()
                    .record("error.type", kind)
                    .record("error.message", message.as_str())
                    .record("otel.status_code", "ERROR");
            }
            result
        }
        .instrument(span)
    }
}

impl<R: Request<Err = RequestError>> TraceRequest for R {}
