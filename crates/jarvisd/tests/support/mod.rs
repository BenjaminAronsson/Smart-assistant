//! Support doubles shared across jarvisd's own integration tests (F9.4).
//!
//! Included the same way `golden11_support` already was: each consuming
//! `tests/*.rs` file declares `mod support;` for this path, so the code is
//! written once even though each integration-test binary compiles its own
//! copy. `RecordingCanvas` implements `jarvisd::cards::CanvasSink`, defined in
//! this crate — it cannot live in the cross-crate `jarvis-test-support` crate
//! without that crate depending on `jarvisd`, which would invert the layering
//! `cargo xtask arch-test` enforces.

#![allow(dead_code)]

use std::sync::Mutex;

/// Records every canvas `publish` call, verified identical (modulo the
/// `published()` accessor, additive) across its two original call sites
/// before being consolidated here.
#[derive(Default)]
pub struct RecordingCanvas {
    pub published: Mutex<Vec<jarvis_contracts::deepdive::HudCanvasDto>>,
}

impl RecordingCanvas {
    pub fn published(&self) -> Vec<jarvis_contracts::deepdive::HudCanvasDto> {
        self.published.lock().unwrap().clone()
    }
}

impl jarvisd::cards::CanvasSink for RecordingCanvas {
    fn publish(&self, canvas: jarvis_contracts::deepdive::HudCanvasDto) {
        self.published.lock().unwrap().push(canvas);
    }
}
