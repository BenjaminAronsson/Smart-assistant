//! Tracing + OTel wiring (docs/02 §14, NFR-14). Journal (fmt) output always;
//! OTLP export only when configured — the collector is off by default on
//! low-power hosts (docs/09 §5). Secrets never reach spans structurally:
//! they travel as `jarvis_domain::secrecy::Redacted`, which prints
//! `[REDACTED]` from any Debug/Display formatting a layer might do.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialized telemetry; shut down explicitly so buffered spans drain
/// (invariant 4: graceful shutdown checkpoints and drains).
pub struct Telemetry {
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

pub fn init(otlp_endpoint: Option<&str>) -> anyhow::Result<Telemetry> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt::layer().with_target(true);

    match otlp_endpoint {
        Some(endpoint) => {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint.to_owned())
                .build()?;
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .build();
            let otel = tracing_opentelemetry::layer().with_tracer(provider.tracer("jarvisd"));
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt)
                .with(otel)
                .try_init()?;
            Ok(Telemetry {
                provider: Some(provider),
            })
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt)
                .try_init()?;
            Ok(Telemetry { provider: None })
        }
    }
}

impl Telemetry {
    pub fn shutdown(self) {
        if let Some(provider) = self.provider
            && let Err(e) = provider.shutdown()
        {
            // eprintln! on purpose: the tracing pipeline is being torn down
            // right here, so tracing::error! would go nowhere (don't "fix").
            eprintln!("otel span drain on shutdown failed: {e}");
        }
    }
}
