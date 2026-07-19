//! Discriminated content blocks (docs/05 §2) — never one overloaded string.
//! Seeded with `text` only; `image_ref`, `tool_call`, `approval_ref`, and
//! `artifact_ref` are added with the features that produce them (M1–M3).
//! Variants grow additively within a contract version, so every reader —
//! including Rust consumers like `jarvis-agent` that may lag a rebuild —
//! must tolerate unknown tags: they deserialize to [`ContentBlock::Unknown`]
//! and render as an opaque block instead of failing the whole message.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Forward-compatibility catch-all (docs/05 §5). Never produced by this
    /// version's writers; only ever the result of reading a newer peer.
    #[serde(other)]
    Unknown,
}
