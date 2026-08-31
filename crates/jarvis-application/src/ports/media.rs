/// Local media transport control (FR-22, docs/02 §11a, ADR-012). The universal
/// control plane is MPRIS over the session bus, but the application layer only
/// names the capability — no D-Bus type appears here (invariant 3).
///
/// Two properties are part of the contract, not the implementation's choice:
///
/// * **Absence is not an error.** No session bus, no player, or a player that
///   vanished between snapshot and command yields a clean empty/`PlayerGone`
///   outcome — a media integration must never fail a run because nothing
///   happened to be playing.
/// * **The cap is not enforced here.** `set_volume` performs exactly what it is
///   told; whether a level is allowed is decided by
///   [`jarvis_domain::media::VolumePct::within_cap`] at the policy boundary
///   (the R1 tool / the owner-driven REST surface), so the controller stays a
///   dumb effector and the hearing-protection decision lives in one place.
#[async_trait::async_trait]
pub trait MediaController: Send + Sync {
    /// Everything currently on the bus. An empty snapshot is a successful
    /// observation.
    async fn snapshot(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<jarvis_domain::media::MediaSnapshot, MediaError>;

    /// Apply a transport verb to a specific player.
    async fn transport(
        &self,
        player: &jarvis_domain::media::PlayerId,
        command: jarvis_domain::media::TransportCommand,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), MediaError>;

    /// Set a player's volume. The caller has already decided the level is
    /// authorized (see the trait note).
    async fn set_volume(
        &self,
        player: &jarvis_domain::media::PlayerId,
        volume: jarvis_domain::media::VolumePct,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), MediaError>;
}

/// Why a media operation could not be performed. Deliberately small and
/// content-free: no player-published text and no D-Bus error body reaches this
/// type (invariant 5 — these strings surface in captions and audit rows).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// The named player is no longer on the bus (it quit mid-command). A clean,
    /// user-explainable outcome — "that player is no longer running".
    #[error("that player is no longer running")]
    PlayerGone,
    /// The player is present but says it cannot do this (`CanGoNext = false`).
    #[error("the player does not support that control")]
    Unsupported,
    /// No session bus / media control disabled — the whole capability is absent.
    #[error("media control is unavailable")]
    Unavailable,
    #[error("media control was cancelled")]
    Cancelled,
    /// Anything else, already reduced to a short non-sensitive diagnostic.
    #[error("media control failed: {0}")]
    Failed(String),
}

/// Delivery of the current media state to connected clients (FR-22, docs/02
/// §11a). Like [`DisplayDirectiveSink`], this is best-effort fan-out with no
/// error channel: nobody listening is a normal state, not a failure. The
/// jarvisd implementation projects the domain snapshot into the transient
/// `media.state` WS event — it is deliberately **not** persisted (a
/// current-value readout is not timeline history, docs/05 §3).
#[async_trait::async_trait]
pub trait MediaStateSink: Send + Sync {
    async fn publish(&self, snapshot: &jarvis_domain::media::MediaSnapshot);
}
