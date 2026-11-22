//! Optional Langfuse Cloud export shared by the bot and the diff CLI.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{
    HttpExporterBuilderSet, Protocol, SpanExporter, SpanExporterBuilder, WithExportConfig,
    WithHttpConfig,
};
use opentelemetry_sdk::{
    Resource,
    trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider},
};
use std::{collections::HashMap, env, time::Duration};
use tracing::{Instrument, Span, Subscriber};
use tracing_subscriber::{Layer, filter::filter_fn, prelude::*};

pub struct Langfuse(SdkTracerProvider);

impl Drop for Langfuse {
    fn drop(&mut self) {
        if let Err(error) = self.0.shutdown_with_timeout(Duration::from_secs(5)) {
            eprintln!("Langfuse trace shutdown failed: {error}");
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

pub fn init(service: &'static str) -> Result<Option<Langfuse>> {
    let credentials = (
        setting("LANGFUSE_PUBLIC_KEY")?,
        setting("LANGFUSE_SECRET_KEY")?,
    );
    let (public, secret) = match credentials {
        (None, None) => return Ok(None),
        (Some(public), Some(secret)) => (public, secret),
        _ => bail!("Set both LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY"),
    };
    let region = setting("LANGFUSE_REGION")?;
    let exporter = cloud_exporter(region.as_deref().unwrap_or("eu"), &public, &secret)?
        .build()
        .context("Could not initialize the Langfuse exporter")?;
    // A single turn can carry 24 MiB of images before Base64 encoding. Combining
    // several turns in one OTLP request can exceed the exporter's 64 MiB limit.
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_export_batch_size(1)
                .build(),
        )
        .build();
    let provider = SdkTracerProvider::builder()
        .with_resource(Resource::builder_empty().with_service_name(service).build())
        .with_span_processor(processor)
        .build();
    tracing::subscriber::set_global_default(subscriber(&provider))?;
    Ok(Some(Langfuse(provider)))
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

pub(crate) fn subscriber(provider: &SdkTracerProvider) -> impl Subscriber + Send + Sync {
    // Only export our AI spans and Rig's GenAI operations. HTTP clients and Telegram
    // can emit events containing full URLs or response bodies.
    let filter = filter_fn(|meta| {
        meta.is_span()
            && (meta.target() == "rustverse::ai"
                || (meta.fields().field("gen_ai.operation.name").is_some()
                    && (meta.target().starts_with("rig::")
                        || meta.target().starts_with("rig_agent::"))))
    });
    let diagnostics = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter_fn(|meta| {
            meta.is_event()
                && meta.target() == "opentelemetry_sdk"
                && *meta.level() <= tracing::Level::WARN
        }));
    tracing_subscriber::registry().with(diagnostics).with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("rustverse_ai"))
            .with_location(false)
            .with_threads(false)
            .with_filter(filter),
    )
}

pub fn diff_span(
    source: &'static str,
    model: &str,
    previous: &str,
    current: &str,
    agent_id: u64,
) -> Span {
    tracing::info_span!(target: "rustverse::ai", "diff.generate", source, model, previous, current,
        agent_id, otel.status_code = tracing::field::Empty)
}

/// Build full Rig payloads only when the application is collecting AI spans.
pub(crate) fn content_enabled() -> bool {
    tracing::span_enabled!(target: "rustverse::ai", tracing::Level::INFO)
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
        let _guard = tracing::subscriber::set_default(subscriber(&provider));
        observe(
            diff_span("scheduler", "model", "before", "after", 1601),
            async { Ok(()) },
        )
        .await
        .unwrap();
        drop(Langfuse(provider));

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
    }
}
