//! RFC 9457 problem responses (docs/05 §2, §7). The gateway maps every
//! boundary-crossing error through here — no inline problem bodies anywhere
//! else. Detail strings are for the owner's client; they must never carry
//! secret values or raw driver/internal error text (docs/06 §5) — stable
//! codes and short human sentences only.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use jarvis_application::ports::RepositoryError;
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

/// The shared "resource not found" mapping (F9.9: was reimplemented at 5
/// call sites, all byte-identical). `detail` is the caller's own sentence —
/// what was not found is module-specific, the machine code and status never
/// are.
pub fn not_found(detail: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::ResourceNotFound,
        detail,
        None,
    )
}

/// Shared `RepositoryError` mapping (F9.9: was reimplemented at 6 call
/// sites) for the modules where an idempotency-key conflict earns its own
/// machine code, `ErrorCode::IdempotencyConflict`, distinct from a plain
/// version conflict — sessions, memories, runs. `component` names the
/// module in the (never user-facing) storage-failure log line; the detail
/// strings are the caller's own sentences.
pub fn repository_problem_distinct_idempotency(
    error: RepositoryError,
    component: &str,
    conflict_detail: &str,
    idempotency_detail: &str,
    storage_detail: &str,
) -> Response {
    match error {
        RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::IdempotencyConflict,
            idempotency_detail,
            None,
        ),
        RepositoryError::Conflict(_) => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            conflict_detail,
            None,
        ),
        RepositoryError::Storage(error) => {
            tracing::error!(%error, "{component} storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                storage_detail,
                None,
            )
        }
    }
}

/// Shared `RepositoryError` mapping (F9.9) for the modules where an
/// idempotency-key conflict collapses into the same machine code as a plain
/// version conflict, `ErrorCode::ResourceVersionConflict` — artifacts,
/// display, media. See [`repository_problem_distinct_idempotency`] for the
/// other behaviour; the two are kept as separate functions rather than one
/// parameterized by a flag so each call site's choice stays visible at the
/// call site instead of behind a boolean.
pub fn repository_problem_merged_idempotency(
    error: RepositoryError,
    component: &str,
    conflict_detail: &str,
    storage_detail: &str,
) -> Response {
    match error {
        RepositoryError::Conflict(_) | RepositoryError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            ErrorCode::ResourceVersionConflict,
            conflict_detail,
            None,
        ),
        RepositoryError::Storage(error) => {
            tracing::error!(%error, "{component} storage failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::ProviderUnavailable,
                storage_detail,
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_of(response: Response) -> jarvis_contracts::errors::ProblemDetails {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("problem body reads");
        serde_json::from_slice(&bytes).expect("problem body is valid JSON")
    }

    #[tokio::test]
    async fn distinct_idempotency_gives_it_its_own_code() {
        let response = repository_problem_distinct_idempotency(
            RepositoryError::IdempotencyConflict,
            "test",
            "conflict",
            "idempotency conflict",
            "storage down",
        );
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = body_of(response).await;
        assert_eq!(body.code, ErrorCode::IdempotencyConflict);
        assert_eq!(body.title, "idempotency conflict");
    }

    #[tokio::test]
    async fn distinct_idempotency_still_maps_conflict_and_storage() {
        let conflict = repository_problem_distinct_idempotency(
            RepositoryError::Conflict("v1 vs v2".to_owned()),
            "test",
            "version conflict",
            "idempotency conflict",
            "storage down",
        );
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_of(conflict).await.code,
            ErrorCode::ResourceVersionConflict
        );

        let storage = repository_problem_distinct_idempotency(
            RepositoryError::Storage("connection refused".to_owned()),
            "test",
            "version conflict",
            "idempotency conflict",
            "storage down",
        );
        assert_eq!(storage.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_of(storage).await;
        assert_eq!(body.code, ErrorCode::ProviderUnavailable);
        // The raw driver error text must never reach the client (docs/06 §5).
        assert!(!body.title.contains("connection refused"));
    }

    #[tokio::test]
    async fn merged_idempotency_collapses_into_the_conflict_code() {
        let response = repository_problem_merged_idempotency(
            RepositoryError::IdempotencyConflict,
            "test",
            "version conflict",
            "storage down",
        );
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_of(response).await.code,
            ErrorCode::ResourceVersionConflict
        );
    }

    #[tokio::test]
    async fn merged_idempotency_still_maps_conflict_and_storage() {
        let conflict = repository_problem_merged_idempotency(
            RepositoryError::Conflict("v1 vs v2".to_owned()),
            "test",
            "version conflict",
            "storage down",
        );
        assert_eq!(
            body_of(conflict).await.code,
            ErrorCode::ResourceVersionConflict
        );

        let storage = repository_problem_merged_idempotency(
            RepositoryError::Storage("connection refused".to_owned()),
            "test",
            "version conflict",
            "storage down",
        );
        assert_eq!(storage.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_of(storage).await.code, ErrorCode::ProviderUnavailable);
    }

    #[tokio::test]
    async fn not_found_uses_the_stable_resource_not_found_code() {
        let response = not_found("no such widget");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_of(response).await;
        assert_eq!(body.code, ErrorCode::ResourceNotFound);
        assert_eq!(body.title, "no such widget");
    }
}
