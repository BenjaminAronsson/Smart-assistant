/// Delivery of a resolved display placement to connected desktop agents
/// (FR-09/10, docs/02 §8). The agent is a display-channel client, so this is a
/// best-effort, fire-and-forget broadcast: with no agent connected the directive
/// is audited-but-undelivered, which is a reportable outcome, not an error. The
/// port takes the *domain* placement; the jarvisd implementation maps it to the
/// wire `DisplayDirective` (deriving the surface's app-id) and broadcasts it.
#[async_trait::async_trait]
pub trait DisplayDirectiveSink: Send + Sync {
    /// Dispatch the placement to `target` — a paired device id (F7.5) — or to
    /// every presenter when `None`, which is what every placement meant before
    /// nodes existed. Returns true if at least one WS client was subscribed to
    /// receive it.
    ///
    /// Note the division of labour: the *caller* has already established that
    /// the target can take the placement (paired, active, has a screen, and is
    /// connected), because that is the check whose failure the owner must see.
    /// This is still fire-and-forget — a socket can drop between the two.
    async fn dispatch(
        &self,
        placement: &jarvis_domain::display::SurfacePlacement,
        target: Option<&str>,
    ) -> bool;
}

/// Opening a URL in the dedicated media window (FR-22, ADR-012 cast-a-link).
///
/// Separate from [`DisplayDirectiveSink`] because it carries a *payload* (the
/// URL) rather than only a placement, and because it is the one path that makes
/// the agent launch a process — keeping it its own port means a reader can find
/// every caller of that capability by finding this trait's users.
///
/// The implementation is best-effort fan-out to connected agents, same as a
/// placement: no agent connected ⇒ audited-but-undelivered, reported as `false`,
/// not an error.
#[async_trait::async_trait]
pub trait MediaWindowSink: Send + Sync {
    /// Dispatch "open this URL in the media window on this monitor". The caller
    /// has already validated the URL scheme and audited the request.
    /// Open `url` in the media window on `monitor`, addressed to `target` — a
    /// paired device id — or to every presenter when `None` (M7 gate D-M7-2).
    async fn open_url(
        &self,
        url: &str,
        monitor: &jarvis_domain::display::MonitorId,
        target: Option<&str>,
    ) -> bool;
}
