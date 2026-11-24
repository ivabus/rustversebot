//! OTLP trace export shared by the bot and the diff CLI.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use opentelemetry::{
    Context as OtelContext, KeyValue,
    trace::{Span as _, TracerProvider},
};
use opentelemetry_otlp::{
    HttpExporterBuilderSet, Protocol, SpanExporter, SpanExporterBuilder, WithExportConfig,
    WithHttpConfig,
};
use opentelemetry_sdk::{
    Resource,
    error::OTelSdkResult,
    trace::{
        BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider, Span as SdkSpan, SpanData,
        SpanProcessor,
    },
};
use std::{collections::HashMap, env, time::Duration};
use tracing::{Instrument, Span, Subscriber};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{Layer, filter::filter_fn, prelude::*};

mod tools;
pub(crate) use tools::GenerationTrace;

pub struct Telemetry(SdkTracerProvider);

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Err(error) = self.0.shutdown_with_timeout(Duration::from_secs(5)) {
            eprintln!("Trace export shutdown failed: {error}");
        }
    }
}

fn setting(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value).filter(|s| !s.trim().is_empty())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(_) => bail!("{name} must be valid UTF-8"),
    }
}

pub fn init(service: &'static str) -> Result<Option<Telemetry>> {
    let whole_bot = setting("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")?.is_some()
        || setting("OTEL_EXPORTER_OTLP_ENDPOINT")?.is_some();
    let exporter = if whole_bot {
        // The SDK resolves the signal-specific endpoint and headers before the base settings.
        SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
    } else {
        let (public, secret) = match (
            setting("LANGFUSE_PUBLIC_KEY")?,
            setting("LANGFUSE_SECRET_KEY")?,
        ) {
            (None, None) => return Ok(None),
            (Some(public), Some(secret)) => (public, secret),
            _ => bail!("Set both LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY"),
        };
        let region = setting("LANGFUSE_REGION")?;
        if setting("OTEL_EXPORTER_OTLP_TRACES_HEADERS")?.is_some()
            || setting("OTEL_EXPORTER_OTLP_HEADERS")?.is_some()
        {
            bail!("Set the OTLP endpoint for the configured OTLP headers before enabling export");
        }
        cloud_exporter(region.as_deref().unwrap_or("eu"), &public, &secret)?
    };
    let content = match setting("RUSTVERSE_TRACE_AI_CONTENT")?.as_deref() {
        None => !whole_bot,
        Some("true") => true,
        Some("false") => false,
        Some(_) => bail!("RUSTVERSE_TRACE_AI_CONTENT must be true or false"),
    };
    let exporter = exporter
        .build()
        .context("Could not initialize the OTLP exporter")?;
    // A single turn can carry 24 MiB of images before Base64 encoding. Combining
    // several turns in one OTLP request can exceed the exporter's 64 MiB limit.
    let mut batch = BatchConfigBuilder::default();
    if content {
        batch = batch.with_max_export_batch_size(1);
    }
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch.build())
        .build();
    let provider = SdkTracerProvider::builder()
        .with_resource(Resource::builder_empty().with_service_name(service).build())
        .with_span_processor(SessionProcessor(processor))
        .build();
    tracing::subscriber::set_global_default(subscriber(&provider, whole_bot, content))?;
    Ok(Some(Telemetry(provider)))
}

#[derive(Clone)]
struct PromptSessionId(String);

/// Attach the conversation UUID while keeping the current Telegram trace as parent.
pub fn set_prompt_session(root: &Span, session_id: String) {
    if !root.is_disabled()
        && let Err(error) = root.set_parent(
            Span::current()
                .context()
                .with_value(PromptSessionId(session_id)),
        )
    {
        log::warn!("Could not attach the Langfuse session: {error}");
    }
}

#[derive(Debug)]
pub(crate) struct SessionProcessor<P>(pub(crate) P);

impl<P: SpanProcessor> SpanProcessor for SessionProcessor<P> {
    fn on_start(&self, span: &mut SdkSpan, cx: &OtelContext) {
        if let Some(session) = cx.get::<PromptSessionId>() {
            span.set_attribute(KeyValue::new("gen_ai.conversation.id", session.0.clone()));
            span.set_attribute(KeyValue::new("langfuse.session.id", session.0.clone()));
        }
        self.0.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        // instrument(err) formats the original error, including provider bodies and URLs.
        // Only application-supplied diagnostic attributes are safe as status descriptions.
        span.events.events.clear();
        if matches!(span.status, opentelemetry::trace::Status::Error { .. }) {
            let attr = |name| {
                span.attributes
                    .iter()
                    .find(|kv| kv.key.as_str() == name)
                    .map(|kv| &kv.value)
            };
            let application_span = attr("target")
                .is_some_and(|v| matches!(v.as_str().as_ref(), "rustverse::bot" | "rustverse::ai"));
            let message = if let Some(reason) = attr("error.message").filter(|_| application_span) {
                reason.to_string()
            } else if let Some(opentelemetry::Value::I64(status)) =
                attr("http.response.status_code")
                && *status >= 400
            {
                format!("HTTP {status} при выполнении {}.", span.name)
            } else {
                match attr("outcome").map(|v| v.as_str()).as_deref() {
                    Some("delivery_failed") => "Не удалось доставить ответ в Telegram.".to_owned(),
                    Some("generation_failed") => {
                        "Не удалось подготовить ответ. Подробности в дочернем спане генерации."
                            .to_owned()
                    }
                    _ => format!(
                        "Операция {} завершилась ошибкой. Проверьте дочерние спаны и журнал бота.",
                        span.name
                    ),
                }
            };
            span.status = opentelemetry::trace::Status::error(message);
        }
        self.0.on_end(span);
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

fn cloud_exporter(
    region: &str,
    public: &str,
    secret: &str,
) -> Result<SpanExporterBuilder<HttpExporterBuilderSet>> {
    let host = match region {
        "eu" => "cloud.langfuse.com",
        "us" => "us.cloud.langfuse.com",
        "jp" => "jp.cloud.langfuse.com",
        "hipaa" => "hipaa.cloud.langfuse.com",
        _ => bail!("LANGFUSE_REGION must be eu, us, jp or hipaa"),
    };
    let auth = STANDARD.encode(format!("{public}:{secret}"));
    Ok(SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(format!("https://{host}/api/public/otel/v1/traces"))
        .with_headers(HashMap::from([
            ("Authorization".into(), format!("Basic {auth}")),
            ("x-langfuse-ingestion-version".into(), "4".into()),
        ])))
}

struct TraceFilter {
    whole_bot: bool,
    content: bool,
}

impl<S> tracing_subscriber::layer::Filter<S> for TraceFilter {
    fn enabled(
        &self,
        meta: &tracing::Metadata<'_>,
        _: &tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        match meta.target() {
            "rustverse::bot" => {
                self.whole_bot
                    && (meta.is_span()
                        || (*meta.level() == tracing::Level::ERROR
                            && meta.fields().field("error").is_some()))
            }
            "rustverse::ai" => meta.is_span(),
            target if target.starts_with("rig::") || target.starts_with("rig_agent::") => {
                meta.is_span() && meta.fields().field("gen_ai.operation.name").is_some()
            }
            _ => false,
        }
    }
}

pub(crate) fn subscriber(
    provider: &SdkTracerProvider,
    whole_bot: bool,
    content: bool,
) -> impl Subscriber + Send + Sync {
    let diagnostics = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter_fn(|meta| {
            meta.is_event()
                && meta.target() == "opentelemetry_sdk"
                && *meta.level() <= tracing::Level::WARN
        }));
    tracing_subscriber::registry().with(diagnostics).with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("rustverse"))
            .with_location(false)
            .with_threads(false)
            .with_filter(TraceFilter { whole_bot, content }),
    )
}

pub fn diff_span(
    source: &'static str,
    model: &str,
    previous: &str,
    current: &str,
    agent_count: usize,
) -> Span {
    tracing::info_span!(target: "rustverse::ai", "diff.generate", langfuse.observation.type = "span",
        source, selected_model = model, previous, current,
        agent_count, otel.status_code = tracing::field::Empty)
}

/// Read the content policy of this dispatcher, independently of enabled span targets.
pub fn content_enabled() -> bool {
    tracing::dispatcher::get_default(|dispatch| {
        dispatch
            .downcast_ref::<TraceFilter>()
            .is_some_and(|f| f.content)
    })
}

/// Record failure without exposing provider errors or user input in the trace.
pub async fn observe<T>(span: Span, work: impl Future<Output = Result<T>>) -> Result<T> {
    async {
        let result = work.await;
        if result.is_err() {
            Span::current().record("otel.status_code", "ERROR");
        }
        result
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
    use std::sync::mpsc;

    #[test]
    fn exporter_uses_the_public_error_message_and_drops_raw_errors() {
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SimpleSpanProcessor};
        let export = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SessionProcessor(SimpleSpanProcessor::new(export.clone())))
            .build();
        let _guard = tracing::subscriber::set_default(subscriber(&provider, true, false));
        let message = "HoYoLAB отклонил запрос (retcode 42).";
        let span = tracing::info_span!(target: "rustverse::bot", "hoyolab.shiyu_defense",
            error.message = tracing::field::Empty, error.type = "api", hoyolab.retcode = 42_i64);
        span.in_scope(|| {
            Span::current().record("error.message", message);
            tracing::error!(target: "rustverse::bot", error = "private cookie and response body");
        });
        drop(span);
        let spans = export.get_finished_spans().unwrap();
        assert_eq!(
            spans[0].status,
            opentelemetry::trace::Status::error(message)
        );
        assert!(spans[0].events.is_empty());
        assert!(!format!("{spans:?}").contains("private cookie"));
    }

    #[derive(Debug)]
    struct Capture(mpsc::Sender<Request<Bytes>>);

    #[async_trait::async_trait]
    impl HttpClient for Capture {
        async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            self.0.send(request)?;
            Ok(Response::new(Bytes::new()))
        }
    }

    #[tokio::test]
    async fn shutdown_exports_a_scheduled_diff_to_the_project_region() {
        let (tx, rx) = mpsc::channel();
        let exporter = cloud_exporter("us", "pk", "sk")
            .unwrap()
            .with_http_client(Capture(tx))
            .build()
            .unwrap();
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .build();
        let _guard = tracing::subscriber::set_default(subscriber(&provider, false, true));
        observe(
            diff_span("scheduler", "model", "before", "after", 2),
            async { Ok(()) },
        )
        .await
        .unwrap();
        drop(Telemetry(provider));

        let request = rx.try_recv().unwrap();
        assert_eq!(request.method(), "POST");
        assert_eq!(
            request.uri(),
            "https://us.cloud.langfuse.com/api/public/otel/v1/traces"
        );
        assert_eq!(request.headers()["authorization"], "Basic cGs6c2s=");
        assert_eq!(request.headers()["x-langfuse-ingestion-version"], "4");
        assert_eq!(request.headers()["content-type"], "application/x-protobuf");
        let payload = String::from_utf8_lossy(request.body());
        assert!(payload.contains("diff.generate") && payload.contains("scheduler"));
        assert!(
            payload.contains("langfuse.observation.type") && payload.contains("selected_model")
        );
    }
}
