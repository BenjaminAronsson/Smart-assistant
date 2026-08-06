//! Calendar vocabulary and the read-only application port (FR-35, ADR-025).
//!
//! The application layer does not parse provider formats or resolve time zones.
//! A caller supplies the two instants that delimit a local calendar day; an
//! adapter expands occurrences into [`CalendarEvent`] values behind
//! [`CalendarReader`]. Calendar data is personal context, so every event keeps
//! its sensitivity label at this boundary.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use jarvis_domain::location::Sensitivity;
use tokio_util::sync::CancellationToken;

/// A deliberately generous upper bound for one local day represented as UTC
/// instants. It accommodates ordinary daylight-saving transitions without
/// allowing an accidental multi-day provider query.
pub const MAX_LOCAL_DAY_WINDOW: Duration = Duration::from_secs(48 * 60 * 60);

/// Maximum number of occurrences a reader may return for one agenda read.
pub const MAX_AGENDA_EVENTS: usize = 256;

/// The instant window for one local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDayWindow {
    pub start: SystemTime,
    pub end: SystemTime,
}

impl LocalDayWindow {
    /// Creates a bounded, non-empty half-open window `[start, end)`.
    pub fn new(start: SystemTime, end: SystemTime) -> Result<Self, CalendarValidationError> {
        let length = end
            .duration_since(start)
            .map_err(|_| CalendarValidationError::WindowReversed)?;
        if length.is_zero() {
            return Err(CalendarValidationError::WindowEmpty);
        }
        if length > MAX_LOCAL_DAY_WINDOW {
            return Err(CalendarValidationError::WindowTooLarge {
                max: MAX_LOCAL_DAY_WINDOW,
            });
        }
        Ok(Self { start, end })
    }

    pub fn contains(&self, instant: SystemTime) -> bool {
        instant >= self.start && instant < self.end
    }
}

/// One expanded calendar occurrence suitable for an agenda card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub title: String,
    pub start: SystemTime,
    pub end: SystemTime,
    pub all_day: bool,
    pub sensitivity: Sensitivity,
}

impl CalendarEvent {
    pub fn new(
        title: impl Into<String>,
        start: SystemTime,
        end: SystemTime,
        all_day: bool,
        sensitivity: Sensitivity,
    ) -> Result<Self, CalendarValidationError> {
        let title = title.into();
        if title.trim().is_empty() {
            return Err(CalendarValidationError::EmptyTitle);
        }
        if end.duration_since(start).is_err() {
            return Err(CalendarValidationError::EventReversed);
        }
        if start == end {
            return Err(CalendarValidationError::EventEmpty);
        }
        Ok(Self {
            title,
            start,
            end,
            all_day,
            sensitivity,
        })
    }
}

/// Validation failures at the pure calendar vocabulary boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CalendarValidationError {
    #[error("calendar window ends before it starts")]
    WindowReversed,
    #[error("calendar window must not be empty")]
    WindowEmpty,
    #[error("calendar window exceeds the maximum of {max:?}")]
    WindowTooLarge { max: Duration },
    #[error("calendar event title must not be empty")]
    EmptyTitle,
    #[error("calendar event ends before it starts")]
    EventReversed,
    #[error("calendar event must not be empty")]
    EventEmpty,
}

/// The deterministic subset of calendar questions handled without a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarQuery {
    Today,
}

/// Classifies the exact, deterministic "what's on today" family of requests.
/// Punctuation at the end and repeated whitespace are ignored; other wording
/// is deliberately left for the normal router rather than guessed here.
pub fn classify_calendar_query(query: &str) -> Option<CalendarQuery> {
    let normalized = query
        .trim()
        .trim_end_matches(['?', '.', '!'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();

    match normalized.as_str() {
        "what's on today"
        | "what is on today"
        | "what's on my calendar today"
        | "what is on my calendar today" => Some(CalendarQuery::Today),
        _ => None,
    }
}

/// Why a read-only agenda lookup failed. Provider-specific diagnostics must be
/// reduced by the adapter before reaching this boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalendarReaderError {
    #[error("calendar read was cancelled")]
    Cancelled,
    #[error("calendar is unavailable")]
    Unavailable,
    #[error("calendar read failed: {0}")]
    Failed(String),
}

/// Read-only calendar capability for the application layer (R0, ADR-025).
#[async_trait]
pub trait CalendarReader: Send + Sync {
    /// Reads occurrences intersecting `window`, ordered by start time.
    /// Implementations must honor cancellation and return at most
    /// [`MAX_AGENDA_EVENTS`] events.
    async fn read(
        &self,
        window: LocalDayWindow,
        cancel: CancellationToken,
    ) -> Result<Vec<CalendarEvent>, CalendarReaderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn rejects_malformed_and_unbounded_windows() {
        assert_eq!(
            LocalDayWindow::new(instant(2), instant(1)),
            Err(CalendarValidationError::WindowReversed)
        );
        assert_eq!(
            LocalDayWindow::new(instant(1), instant(1)),
            Err(CalendarValidationError::WindowEmpty)
        );
        assert!(matches!(
            LocalDayWindow::new(instant(0), instant(MAX_LOCAL_DAY_WINDOW.as_secs() + 1)),
            Err(CalendarValidationError::WindowTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_malformed_events_and_preserves_sensitivity() {
        assert_eq!(
            CalendarEvent::new(" ", instant(1), instant(2), false, Sensitivity::Sensitive),
            Err(CalendarValidationError::EmptyTitle)
        );
        assert_eq!(
            CalendarEvent::new(
                "meeting",
                instant(2),
                instant(1),
                false,
                Sensitivity::Normal,
            ),
            Err(CalendarValidationError::EventReversed)
        );
        assert_eq!(
            CalendarEvent::new("meeting", instant(1), instant(1), true, Sensitivity::Normal),
            Err(CalendarValidationError::EventEmpty)
        );
        let event = CalendarEvent::new(
            "private appointment",
            instant(1),
            instant(2),
            false,
            Sensitivity::Sensitive,
        )
        .unwrap();
        assert_eq!(event.sensitivity, Sensitivity::Sensitive);
    }

    #[test]
    fn today_classifier_is_deterministic_and_conservative() {
        for query in [
            "What's on today?",
            " what   is on my calendar today! ",
            "WHAT'S ON TODAY",
        ] {
            assert_eq!(classify_calendar_query(query), Some(CalendarQuery::Today));
        }
        for query in [
            "what's on tomorrow",
            "what's on today and tomorrow",
            "show my calendar",
        ] {
            assert_eq!(classify_calendar_query(query), None);
        }
    }

    #[test]
    fn local_day_window_is_half_open() {
        let window = LocalDayWindow::new(instant(10), instant(20)).unwrap();
        assert!(window.contains(instant(10)));
        assert!(window.contains(instant(19)));
        assert!(!window.contains(instant(20)));
    }
}
