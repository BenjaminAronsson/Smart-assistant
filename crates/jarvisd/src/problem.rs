//! RFC 9457 problem responses (docs/05 §2, §7). The gateway maps every
//! boundary-crossing error through here — no inline problem bodies anywhere
//! else. Detail strings are for the owner's client; they must never carry
//! secret values or raw driver/internal error text (docs/06 §5) — stable
//! codes and short human sentences only.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use jarvis_contracts::errors::{ErrorCode, ProblemDetails};

pub fn problem(
    status: StatusCode,
    code: ErrorCode,
    title: &str,
    detail: Option<String>,
) -> Response {
    let body = ProblemDetails {
        problem_type: "about:blank".to_owned(),
        title: title.to_owned(),
        status: status.as_u16(),
        detail,
        instance: None,
        code,
    };
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        serde_json::to_string(&body).expect("ProblemDetails serializes"),
    )
        .into_response()
}
