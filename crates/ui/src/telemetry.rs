//! Logging and tracing setup.
//!
//! - Terminal: always, filtered by `RUST_LOG` (default: info).
//! - `TESTUDO_LOG_FILE=<path>`: the same events, without colours, appended to
//!   a file. `make dev` sets it so logs survive the terminal.
//! - `TESTUDO_OTLP_ENDPOINT=<url>` (e.g. `http://localhost:7520`, Pitot or any
//!   OTLP/HTTP collector): spans and log events exported over OTLP/HTTP, with
//!   `TESTUDO_OTLP_TOKEN` sent as a bearer token. Testudo-specific names so
//!   the user's `OTEL_*` settings (meant for the apps under test) don't send
//!   Testudo's own telemetry anywhere, and ours don't leak into test runs.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

pub const ENDPOINT_VAR: &str = "TESTUDO_OTLP_ENDPOINT";
pub const TOKEN_VAR: &str = "TESTUDO_OTLP_TOKEN";
pub const LOG_FILE_VAR: &str = "TESTUDO_LOG_FILE";

const DEFAULT_FILTER: &str = "info,wgpu=warn,naga=warn";
/// What is exported: Testudo's own spans and logs, plus warnings from
/// dependencies. Not their info-level spans (zbus traces every D-Bus call),
/// nor the exporter's own HTTP stack (feedback loop). Override with
/// `TESTUDO_OTLP_FILTER` (EnvFilter syntax).
const OTLP_FILTER: &str = "warn,testudo=info,testudo_core=info,testudo_store=info,reqwest=off,hyper=off,hyper_util=off,h2=off,opentelemetry=off";
pub const OTLP_FILTER_VAR: &str = "TESTUDO_OTLP_FILTER";

fn otlp_filter() -> EnvFilter {
    std::env::var(OTLP_FILTER_VAR)
        .ok()
        .and_then(|f| EnvFilter::try_new(f).ok())
        .unwrap_or_else(|| EnvFilter::new(OTLP_FILTER))
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into())
}

/// Flushes and stops OTLP export when dropped.
#[derive(Default)]
pub struct Telemetry {
    pub log_file: Option<PathBuf>,
    pub otlp_endpoint: Option<String>,
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(t) = self.tracer.take()
            && let Err(e) = t.shutdown()
        {
            eprintln!("testudo: flushing traces: {e}");
        }
        if let Some(l) = self.logger.take()
            && let Err(e) = l.shutdown()
        {
            eprintln!("testudo: flushing logs: {e}");
        }
    }
}

/// Install the global subscriber. Keep the result alive until exit.
pub fn init() -> Telemetry {
    let mut out = Telemetry::default();
    let terminal = fmt::layer().with_filter(filter());

    let path = std::env::var_os(LOG_FILE_VAR).map(PathBuf::from);
    let file = path.as_ref().and_then(|p| {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match OpenOptions::new().create(true).append(true).open(p) {
            Ok(f) => Some(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(Mutex::new(f))
                    .with_filter(filter()),
            ),
            Err(e) => {
                eprintln!("testudo: cannot open log file {}: {e}", p.display());
                None
            }
        }
    });
    out.log_file = file.is_some().then_some(path).flatten();

    let endpoint = std::env::var(ENDPOINT_VAR).ok().filter(|e| !e.is_empty());
    let (spans, logs) = match endpoint.as_deref().map(otlp) {
        Some(Ok((tracer, logger))) => {
            let spans = tracing_opentelemetry::layer()
                .with_tracer(tracer.tracer("testudo"))
                .with_filter(otlp_filter());
            let logs =
                OpenTelemetryTracingBridge::new(&logger).with_filter(otlp_filter());
            out.tracer = Some(tracer);
            out.logger = Some(logger);
            out.otlp_endpoint = endpoint;
            (Some(spans), Some(logs))
        }
        Some(Err(e)) => {
            eprintln!("testudo: OTLP export disabled: {e:#}");
            (None, None)
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(terminal)
        .with(file)
        .with(spans)
        .with(logs)
        .init();
    out
}

fn otlp(base: &str) -> anyhow::Result<(SdkTracerProvider, SdkLoggerProvider)> {
    let base = base.trim_end_matches('/');
    let headers: std::collections::HashMap<String, String> = std::env::var(TOKEN_VAR)
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| ("authorization".to_owned(), format!("Bearer {t}")))
        .into_iter()
        .collect();
    let resource = Resource::builder()
        .with_service_name("testudo")
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .build();

    let spans = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(format!("{base}/v1/traces"))
        .with_headers(headers.clone())
        .build()?;
    let logs = LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(format!("{base}/v1/logs"))
        .with_headers(headers)
        .build()?;
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(spans)
        .with_resource(resource.clone())
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(logs)
        .with_resource(resource)
        .build();
    Ok((tracer, logger))
}
