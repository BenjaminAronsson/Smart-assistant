//! F0.6: Session construction invariants (docs/04 §2).

use jarvis_domain::conversations::{Session, SessionStatus};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn new_session_is_active_with_equal_timestamps() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session = Session::new(id, Some("title".into()), now);
    assert_eq!(session.status, SessionStatus::Active);
    assert_eq!(session.created_at, session.updated_at);
    assert_eq!(session.created_at, now);
}

#[test]
fn new_session_title_is_optional() {
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let session = Session::new(id, None, SystemTime::now());
    assert_eq!(session.title, None);
}
