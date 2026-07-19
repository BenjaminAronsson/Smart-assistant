//! Structural secret containment (CLAUDE.md invariant 5).
//!
//! Secrets travel as [`Redacted`] values: Debug/Display print `[REDACTED]`
//! whatever the inner type, and the type implements neither `Serialize` nor
//! `Deserialize`, so a secret cannot reach logs, traces, prompts, or wire
//! DTOs by accident — leaking requires a deliberate `.expose()` at the
//! adapter boundary, which is exactly where review attention goes.

use std::fmt;

pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Deliberate access at the adapter boundary — grep for `expose(` in
    /// review; it must never feed a log, span field, prompt, or DTO.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> Clone for Redacted<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
