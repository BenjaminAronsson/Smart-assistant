use serde::{Deserialize, Serialize};

/// `[lists]` (FR-34, ADR-024, docs/09 §1). Lists and quick notes are **on by
/// default**, for the same reason timers are: the whole module reaches nothing
/// outside this machine — it parses an utterance with a pure function and writes
/// a local row. There is nothing here to gate.
///
/// Nothing else is configurable on purpose. The item bound, the name-key
/// normalization and the promotion threshold are domain constants (ADR-024): a
/// deployment that could retune them would be a deployment where the grammar's
/// behaviour is not the same everywhere it is tested.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListsConfig {
    /// Set false to run with no list surface at all: no routes, nothing
    /// resident.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ListsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub(crate) fn default_true() -> bool {
    true
}
