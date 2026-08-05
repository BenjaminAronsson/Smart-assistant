//! Deterministic, quota-free routes that run before a reasoning provider.
//!
//! The wrapper is deliberately a [`ModelProvider`] rather than a shortcut in
//! the HTTP layer: every recognized request still goes through the ordinary
//! orchestrator, checkpoints, cancellation, and streamed response path. The
//! wrapper only decides whether a provider invocation is needed.

use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use futures_core::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::model::{ModelError, ModelEvent, ModelProvider, ModelRequest, ProfileId};
use jarvis_domain::math::{MathCommand, parse_math_command};

/// A model provider that answers the bounded deterministic grammar locally
/// before invoking its inner provider. Unrecognized input is never guessed.
pub struct DeterministicFirstProvider {
    inner: std::sync::Arc<dyn ModelProvider>,
}

impl DeterministicFirstProvider {
    pub fn new(inner: std::sync::Arc<dyn ModelProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ModelProvider for DeterministicFirstProvider {
    fn id(&self) -> ProfileId {
        self.inner.id()
    }

    async fn run(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ModelEvent>, ModelError> {
        let Some(command) = parse_math_command(&request.prompt) else {
            return self.inner.run(request, cancel).await;
        };

        let answer = render_math_answer(&command);
        Ok(Box::pin(OneShotStream::new([
            ModelEvent::TextDelta(answer),
            ModelEvent::Done(crate::model::FinishReason::Stop),
        ])))
    }
}

fn render_math_answer(command: &MathCommand) -> String {
    let result = command.evaluate();
    let value = jarvis_domain::math::format_number(result.value);
    match result.unit {
        Some(unit) => format!("{} = {} {}", result.expression, value, unit.symbol()),
        None => format!("{} = {}", result.expression, value),
    }
}

/// Small allocation-free stream for the two events produced by a local route.
/// Keeping this here avoids adding an executor or stream-combinator dependency
/// to the pure application crate.
struct OneShotStream {
    events: std::array::IntoIter<ModelEvent, 2>,
}

impl OneShotStream {
    fn new(events: [ModelEvent; 2]) -> Self {
        Self {
            events: events.into_iter(),
        }
    }
}

impl Stream for OneShotStream {
    type Item = ModelEvent;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.events.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_math_answer_has_no_provider_specific_formatting() {
        let command = parse_math_command("15% of 230").expect("fixture parses");
        let answer = render_math_answer(&command);
        assert_eq!(answer, "15% of 230 = 34.5");
    }
}
