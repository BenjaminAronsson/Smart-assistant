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

use crate::home::{HomeAction, parse_home_intent};
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
            if let Some(intent) = parse_home_intent(&request.prompt) {
                let action = match intent.action {
                    HomeAction::TurnOn => "turning on",
                    HomeAction::TurnOff => "turning off",
                };
                return Ok(Box::pin(OneShotStream::new([
                    ModelEvent::TextDelta(format!("{action} {}", intent.target)),
                    ModelEvent::Done(crate::model::FinishReason::Stop),
                ])));
            }
            return self.inner.run(request, cancel).await;
        };

        let Some(answer) = render_math_answer(&command) else {
            return self.inner.run(request, cancel).await;
        };
        Ok(Box::pin(OneShotStream::new([
            ModelEvent::TextDelta(answer),
            ModelEvent::Done(crate::model::FinishReason::Stop),
        ])))
    }
}

fn render_math_answer(command: &MathCommand) -> Option<String> {
    let result = command.evaluate()?;
    let value = jarvis_domain::math::format_number(result.value);
    match result.unit {
        Some(unit) => Some(format!(
            "{} = {} {}",
            result.expression,
            value,
            unit.symbol()
        )),
        None => Some(format!("{} = {}", result.expression, value)),
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
    use crate::model::ModelProvider;
    use crate::testing::FakeModel;

    #[test]
    fn local_math_answer_has_no_provider_specific_formatting() {
        let command = parse_math_command("15% of 230").expect("fixture parses");
        let answer = render_math_answer(&command).unwrap();
        assert_eq!(answer, "15% of 230 = 34.5");
    }

    #[tokio::test]
    async fn recognized_math_does_not_open_the_inner_provider() {
        let inner = std::sync::Arc::new(FakeModel::streaming(["should not run"]));
        let provider = DeterministicFirstProvider::new(inner.clone());
        let _stream = provider
            .run(
                ModelRequest {
                    prompt: "15% of 230".to_owned(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!inner.opened());
    }

    #[tokio::test]
    async fn unrecognized_input_delegates_to_the_inner_provider() {
        let inner = std::sync::Arc::new(FakeModel::streaming(["delegated"]));
        let provider = DeterministicFirstProvider::new(inner.clone());
        let _stream = provider
            .run(
                ModelRequest {
                    prompt: "tell me a story".to_owned(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(inner.opened());
    }

    #[tokio::test]
    async fn recognized_home_command_does_not_open_the_inner_provider() {
        let inner = std::sync::Arc::new(FakeModel::streaming(["should not run"]));
        let provider = DeterministicFirstProvider::new(inner.clone());
        let _stream = provider
            .run(
                ModelRequest {
                    prompt: "turn on living room lights".to_owned(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!inner.opened());
    }
}
