use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// OTLP gRPC endpoint. Off by default — the collector runs only while
    /// actively debugging (docs/09 §5); spans still go to the journal.
    pub otlp_endpoint: Option<String>,
}
