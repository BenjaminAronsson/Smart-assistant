use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Secret *reference* (`env:VAR` or `keyring:service/entry`) resolving to
    /// the postgres URL. Literal URLs are rejected at validation.
    pub url_secret: String,
    pub max_connections: u32,
}
