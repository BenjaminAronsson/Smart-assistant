//! F2.5 approval-surface integration (docs/06 §3-§4, invariant #1/#6).
//!
//! Proves the live [`JarvisApprovalGate`]: a request parks the caller until a
//! human decision arrives, the approved (possibly *edited*) arguments are what
//! bind, and BOTH lifecycle events land durably — on the outbox (the resync
//! source, docs/05 §3) and on the hash-chained audit log — so the record the
//! human saw and the record the audit keeps cannot diverge.
//!
//! These drive the gate directly, standing in for the orchestrator's
//! `WaitingApproval` park (which is wired to a real tool proposal in F2.6). The
//! REST handler is a thin shell over `gate.resolve`, exercised here at the gate
//! boundary.

use std::time::Duration;

use jarvis_application::policy::{ApprovalGate, ApprovalOutcome, ApprovalRequest};
use jarvis_contracts::approvals::{ApprovalDecision, ApprovalDecisionDto};
use jarvis_domain::ids::{ApprovalId, RunId};
use jarvis_domain::policy::{DataEgress, RiskLevel};
use jarvis_domain::tools::CanonicalValue;
use jarvisd::approvals::{JarvisApprovalGate, ResolveError};
use sqlx::{PgPool, Row};
use tokio_util::sync::CancellationToken;

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn sample_request() -> ApprovalRequest {
    ApprovalRequest {
        run_id: RUN.parse().unwrap(),
        tool_id: "message.send".parse().unwrap(),
        exact_effect: "message.send {to=\"bob@example.com\"}".to_owned(),
        proposed_arguments: CanonicalValue::obj([("to", CanonicalValue::str("bob@example.com"))]),
        risk: RiskLevel::R2,
        reversible: false,
        egress: DataEgress::External,
    }
}

/// The client learns the minted approval id from the persisted (and, in
/// production, WS-published) `approval.requested` card — mirror that here by
/// reading it back from the outbox rather than reaching into the gate.
async fn wait_for_requested_approval_id(pool: &PgPool) -> ApprovalId {
    for _ in 0..100 {
        let row = sqlx::query(
            "SELECT payload FROM outbox.outbox_events \
             WHERE event_type = 'approval.requested' ORDER BY id DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .expect("read outbox");
        if let Some(row) = row {
            let payload: serde_json::Value = row.get("payload");
            let id = payload["card"]["approvalId"]
                .as_str()
                .expect("card carries approvalId");
            return id.parse().expect("valid ULID");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("approval.requested was never persisted");
}

async fn outbox_tags(pool: &PgPool) -> Vec<String> {
    sqlx::query("SELECT event_type FROM outbox.outbox_events ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("read outbox")
        .into_iter()
        .map(|r| r.get::<String, _>("event_type"))
        .collect()
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn approve_with_edit_binds_edited_args_and_persists_both_events(pool: PgPool) {
    let gate = JarvisApprovalGate::new(pool.clone());

    // Park a request, as the orchestrator does at WaitingApproval.
    let parked = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.request(sample_request(), CancellationToken::new())
                .await
        })
    };

    let approval_id = wait_for_requested_approval_id(&pool).await;
    let run_id: RunId = RUN.parse().unwrap();

    // Approve, but EDIT the recipient — the grant must bind the edited set,
    // never the proposal (invalidation by rebinding, docs/06 §4).
    gate.resolve(
        &run_id,
        &approval_id,
        ApprovalDecisionDto {
            decision: ApprovalDecision::Approve,
            edited_arguments: Some(serde_json::json!({ "to": "carol@example.com" })),
        },
        "user:U",
    )
    .await
    .expect("resolve");

    let outcome = parked.await.expect("parked task joins");
    assert_eq!(
        outcome,
        ApprovalOutcome::Approved {
            arguments: CanonicalValue::obj([("to", CanonicalValue::str("carol@example.com"))]),
        },
        "the approved (edited) arguments bind, not the original proposal"
    );

    // Both lifecycle events are durable and replayable (docs/05 §3).
    let tags = outbox_tags(&pool).await;
    assert!(tags.contains(&"approval.requested".to_owned()));
    assert!(tags.contains(&"approval.resolved".to_owned()));

    // The audit chain (requested + resolved) verifies end to end (invariant #6).
    let mut conn = pool.acquire().await.expect("acquire");
    let verified = jarvis_infra::audit::verify_chain(&mut conn)
        .await
        .expect("verify chain");
    assert_eq!(verified, 2, "requested + resolved audit rows, chain intact");
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn approve_without_edits_binds_the_proposed_arguments(pool: PgPool) {
    let gate = JarvisApprovalGate::new(pool.clone());
    let parked = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.request(sample_request(), CancellationToken::new())
                .await
        })
    };
    let approval_id = wait_for_requested_approval_id(&pool).await;
    let run_id: RunId = RUN.parse().unwrap();

    gate.resolve(
        &run_id,
        &approval_id,
        ApprovalDecisionDto {
            decision: ApprovalDecision::Approve,
            edited_arguments: None,
        },
        "user:U",
    )
    .await
    .expect("resolve");

    assert_eq!(
        parked.await.expect("join"),
        ApprovalOutcome::Approved {
            arguments: CanonicalValue::obj([("to", CanonicalValue::str("bob@example.com"))]),
        },
        "no edits ⇒ the proposed arguments bind"
    );
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn deny_unblocks_the_run_as_denied(pool: PgPool) {
    let gate = JarvisApprovalGate::new(pool.clone());
    let parked = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.request(sample_request(), CancellationToken::new())
                .await
        })
    };
    let approval_id = wait_for_requested_approval_id(&pool).await;
    let run_id: RunId = RUN.parse().unwrap();

    gate.resolve(
        &run_id,
        &approval_id,
        ApprovalDecisionDto {
            decision: ApprovalDecision::Deny,
            edited_arguments: None,
        },
        "user:U",
    )
    .await
    .expect("resolve");

    assert_eq!(parked.await.expect("join"), ApprovalOutcome::Denied);
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn resolving_an_unknown_approval_is_not_found(pool: PgPool) {
    let gate = JarvisApprovalGate::new(pool);
    let run_id: RunId = RUN.parse().unwrap();
    let unknown: ApprovalId = "01BX5ZZKBKACTAV9WEVGEMMVS1".parse().unwrap();

    let err = gate
        .resolve(
            &run_id,
            &unknown,
            ApprovalDecisionDto {
                decision: ApprovalDecision::Approve,
                edited_arguments: None,
            },
            "user:U",
        )
        .await
        .expect_err("no such pending approval");
    assert!(matches!(err, ResolveError::NotFound));
}

#[sqlx::test(migrator = "jarvis_infra::MIGRATOR")]
async fn resolving_with_the_wrong_run_is_not_found(pool: PgPool) {
    let gate = JarvisApprovalGate::new(pool.clone());
    let parked = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.request(sample_request(), CancellationToken::new())
                .await
        })
    };
    let approval_id = wait_for_requested_approval_id(&pool).await;

    // A decision addressed to a DIFFERENT run must not resolve this approval
    // (no cross-run oracle); the real run's approval stays pending.
    let other_run: RunId = "01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap();
    let err = gate
        .resolve(
            &other_run,
            &approval_id,
            ApprovalDecisionDto {
                decision: ApprovalDecision::Approve,
                edited_arguments: None,
            },
            "user:U",
        )
        .await
        .expect_err("wrong run");
    assert!(matches!(err, ResolveError::NotFound));

    // The correct run can still resolve it afterwards — it was not consumed.
    let run_id: RunId = RUN.parse().unwrap();
    gate.resolve(
        &run_id,
        &approval_id,
        ApprovalDecisionDto {
            decision: ApprovalDecision::Deny,
            edited_arguments: None,
        },
        "user:U",
    )
    .await
    .expect("resolve by the right run");
    assert_eq!(parked.await.expect("join"), ApprovalOutcome::Denied);
}
