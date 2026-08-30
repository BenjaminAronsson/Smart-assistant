use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jarvis_application::policy::{ToolDescriptor, ToolExecutor};
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::policy::{DataEgress, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue, ToolError, ToolId, ToolInvocation, ToolResult, ToolVersion,
};
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::*;

// ---------------------------------------------------------------------------
// home.set_area_lights (R1 + allowlist + fan-out cap) — F5.4, FR-28, ADR-018
// ---------------------------------------------------------------------------

/// The hard fan-out bound for one plural command.
///
/// This is the control that answers "a plural command has a strictly larger
/// blast radius than the single-entity R1 `set_light`": the difference between
/// the two is *exactly* the fan-out, so the fan-out is what gets bounded. A
/// house whose living room somehow resolves to 40 allowlisted lights gets a
/// refusal naming the count, not a 40-entity sweep. Sixteen comfortably covers a
/// real room while keeping the worst case reviewable in one spoken sentence.
pub(crate) const MAX_AREA_ENTITIES: usize = 16;
pub(crate) const MAX_AREA_NAME_CHARS: usize = 48;

/// Worst case for **one** HA round trip as the transport actually bounds it: the
/// request and the bounded body read are timed separately, each by
/// [`REQUEST_TIMEOUT`] (see `RestTransport::send`).
pub(crate) const HA_ROUND_TRIP: Duration = Duration::from_secs(REQUEST_TIMEOUT.as_secs() * 2);

/// Worst case for one entity of the fan-out: the fail-closed pre-read, then the
/// service call — two round trips through `apply_one`.
pub(crate) const AREA_ENTITY_WORST_CASE: Duration =
    Duration::from_secs(HA_ROUND_TRIP.as_secs() * 2);

/// The executor's **own** deadline for the mutating loop (M5 audit S1).
///
/// The fan-out is up to [`MAX_AREA_ENTITIES`] entities × two HA round trips, so
/// on a degraded HA it can far outlast any single-request timeout. Being cut off
/// from the outside is the dangerous ending: the lights already switched stay
/// switched, while the carefully-worded partial report ("Turned on 2 of 3 …") is
/// discarded, the run fails, and the audit row says the effect never happened
/// (docs/06 §7, invariant 6). So the loop watches the clock itself and *stops*,
/// returning the partial report with everything it never reached named as not
/// attempted — degrading gracefully instead of relying on the host wrapper being
/// generous enough.
///
/// Twenty seconds is chosen against both ends: a healthy LAN answers a round
/// trip in tens of milliseconds (all 16 entities finish in well under a second),
/// while 20 s still covers a badly degraded HA at ~600 ms per request across the
/// full bound — and it is about as long as an owner who just said "turn the
/// lights off" will wait before assuming nothing happened.
pub(crate) const AREA_FANOUT_BUDGET: Duration = Duration::from_secs(20);

/// The host-applied wrapper deadline for this tool ([`ToolPolicy::timeout`]),
/// derived from the work the tool may legitimately do rather than copied from
/// the single-request timeout.
///
/// `REQUEST_TIMEOUT` (10 s) was the bug: ~32 requests were run inside a 10 s
/// wrapper. This is the sum of the three phases that can legitimately elapse —
/// resolution (one `/api/states` round trip), the fan-out budget above, and the
/// one entity that may have started just before the deadline and must be allowed
/// to finish rather than be abandoned mid-service-call. The naive bound
/// (`REQUEST_TIMEOUT × 2 × MAX_AREA_ENTITIES` ≈ 320 s) is four times longer and
/// would make the wrapper a fiction; this stays a genuine backstop that the
/// tool's own deadline should always beat.
pub(crate) const AREA_EXECUTE_TIMEOUT: Duration = Duration::from_secs(
    HA_ROUND_TRIP.as_secs() + AREA_FANOUT_BUDGET.as_secs() + AREA_ENTITY_WORST_CASE.as_secs(),
);

/// A normalized area name — HA's `area_id` slug shape.
///
/// Both sides of the comparison are normalized so the spoken form ("Living
/// Room", "the living room") matches HA's stored `area_id` ("living_room")
/// without a lookup table. Non-ASCII letters are kept and lowercased rather than
/// transliterated: HA's own slugifier may fold them differently, and guessing
/// wrong would silently mis-resolve, so an unmatched area is reported as "no
/// lights in that area" rather than quietly matching the wrong room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AreaKey(pub(crate) String);

pub(crate) fn normalize_area(value: &str) -> Option<AreaKey> {
    let mut key = String::new();
    for ch in value.chars() {
        if ch.is_alphanumeric() {
            key.extend(ch.to_lowercase());
        } else if !key.ends_with('_') {
            key.push('_');
        }
    }
    let key = key.trim_matches('_').to_owned();
    (!key.is_empty()).then_some(AreaKey(key))
}

/// The human-facing area text: sanitized like every other HA-adjacent string,
/// with a leading article dropped so the rendered result reads "in the living
/// room" rather than "in the the living room".
pub(crate) fn area_label(value: &str) -> String {
    // `clean_text` has already collapsed runs of whitespace, so the article is
    // exactly the first space-separated word. Split on the char boundary rather
    // than byte-slicing a lowercased copy: case folding is not length-preserving
    // for every scalar, and an area name is untrusted text.
    let cleaned = clean_text(value, MAX_AREA_NAME_CHARS);
    match cleaned.split_once(' ') {
        Some((article, rest)) if article.eq_ignore_ascii_case("the") && !rest.is_empty() => {
            rest.to_owned()
        }
        _ => cleaned,
    }
}

pub(crate) fn lights_noun(count: usize) -> &'static str {
    if count == 1 { "light" } else { "lights" }
}

/// "A", "A and B", "A, B and C" — the result is spoken aloud (F5.5), so it is
/// written to be heard, not parsed.
pub(crate) fn join_labels(labels: &[String]) -> String {
    match labels {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The honesty clause for the `area_id` gap. Never omitted when it applies:
/// silently dropping lights whose area HA did not report is precisely the
/// failure ADR-018 forbids.
pub(crate) fn unknown_area_caveat(count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    let verb = if count == 1 { "has" } else { "have" };
    format!(
        " {count} allowlisted {} {verb} no known area in Home Assistant and could not be considered.",
        lights_noun(count)
    )
}

pub(crate) fn cancelled_caveat(skipped: &[String]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    let verb = if skipped.len() == 1 { "was" } else { "were" };
    format!(
        " {} {verb} not attempted because the request was cancelled.",
        join_labels(skipped)
    )
}

/// The honesty clause for the [`AREA_FANOUT_BUDGET`] deadline. Kept distinct
/// from [`cancelled_caveat`] because the two are different facts: the owner did
/// not cancel anything — Home Assistant ran out of time — and hearing "cancelled"
/// for a command nobody cancelled is its own small lie.
pub(crate) fn deadline_caveat(unreached: &[String]) -> String {
    if unreached.is_empty() {
        return String::new();
    }
    let verb = if unreached.len() == 1 { "was" } else { "were" };
    format!(
        " {} {verb} not attempted because Home Assistant was too slow to reach them all in time.",
        join_labels(unreached)
    )
}

/// What an area resolved to, plus what could not be judged.
#[derive(Debug, Default)]
pub(crate) struct AreaResolution {
    /// Allowlisted lights whose HA area matches, in entity-id order so the
    /// spoken result and the audit trail are deterministic.
    matched: Vec<EntityMetadata>,
    /// Allowlisted lights HA reported no area for (or does not know at all).
    /// Counted, never silently discarded.
    unknown_area: usize,
}

/// `home.set_area_lights` — turn every allowlisted light in one area on or off.
///
/// The **device class is fixed to `light.*` by the tool's identity**, exactly as
/// `home.set_light` pins it. That is the conservative reading of FR-28's
/// "area/device-class": a plural command for another class (switches, covers)
/// must be a tool built for that class with its own tier, not a `device_class`
/// argument this one's tier cannot see.
///
/// An area is **required**. There is deliberately no "everywhere" form: a
/// whole-house sweep is a different blast radius from a room, and it is not
/// what FR-28 asks for.
pub struct HomeSetAreaLightsTool {
    client: Arc<HomeAssistantClient>,
    allowlist: Arc<EntityAllowlist>,
}

impl HomeSetAreaLightsTool {
    pub fn new(client: Arc<HomeAssistantClient>, allowlist: Arc<EntityAllowlist>) -> Self {
        Self { client, allowlist }
    }

    pub fn id() -> ToolId {
        "home.set_area_lights"
            .parse()
            .expect("static tool id is valid")
    }

    /// Host-owned policy: **R1**, the same tier as `home.set_light`.
    ///
    /// This is the F5.4 tier decision, argued against docs/06 §3 rather than
    /// assumed from the singular case:
    ///
    /// * **R1's own row is "reversible, low impact"** and its example is
    ///   "toggle a light". Every member of the resolved set is, individually,
    ///   that exact action. The set is bounded by the owner's allowlist, pinned
    ///   to `light.*`, local to the LAN, and — see below — genuinely reversible
    ///   per entity. None of R2's row applies: nothing leaves the home, no
    ///   record is mutated, no automation is changed.
    /// * **R2 would buy friction, not containment.** The same caller that can
    ///   invoke this tool can already invoke `home.set_light` N times, each
    ///   auto-authorized at R1, and reach the identical physical effect. A
    ///   control the same actor can bypass at the same authority, by the same
    ///   code path, is theatre — and it would put an approval card in front of
    ///   the single commonest voice command in a house ("turn off the lights"),
    ///   which is how owners get trained to blanket-approve.
    /// * **What actually differs is fan-out, so fan-out is what is bounded** —
    ///   [`MAX_AREA_ENTITIES`], enforced in-executor because `policy::evaluate`
    ///   does not inspect arguments (the same constraint that forced two tools
    ///   for the M3a volume cap and F5.6's volume boost).
    /// * **If this had been R2, the resolved set would have had to reach the
    ///   card.** Approving the literal argument "living room" tells a human
    ///   nothing about which entities it expanded to, and resolution needs HA
    ///   I/O that `validate_args` (sync) cannot perform — so an honest R2 form
    ///   of this tool would need the resolved set carried in the arguments and
    ///   re-verified against HA at execution. That is a real design, and it is
    ///   the one to build if an owner wants approval here; the correct way to
    ///   get it is a policy rule through the settings flow (docs/06 §3), which
    ///   is a human-only decision, not a tier this adapter invents.
    ///
    /// `is_reversible` is claimed only because the executor proves it: every
    /// entity is pre-read, the undo is composed from those pre-reads, and an
    /// entity whose pre-read fails is not mutated at all.
    ///
    /// The `timeout` is [`AREA_EXECUTE_TIMEOUT`], not the single-request
    /// `REQUEST_TIMEOUT` the singular tools use: this one is a fan-out, and a
    /// wrapper that can fire mid-loop turns real, already-applied physical
    /// effects into an audited "nothing happened" (M5 audit S1).
    pub fn policy() -> ToolPolicy {
        ToolPolicy {
            risk: RiskLevel::R1,
            is_reversible: true,
            requires_user_presence: false,
            timeout: AREA_EXECUTE_TIMEOUT,
            required_scopes: [Scope::new(CONTROL_SCOPE).expect("static scope is valid")]
                .into_iter()
                .collect(),
            egress: DataEgress::Local,
        }
    }

    pub fn descriptor(
        client: Arc<HomeAssistantClient>,
        allowlist: Arc<EntityAllowlist>,
    ) -> ToolDescriptor {
        ToolDescriptor {
            id: Self::id(),
            version: ToolVersion::new(1, 0, 0),
            policy: Some(Self::policy()),
            executor: Arc::new(Self::new(client, allowlist)),
        }
    }

    /// Arguments are exactly `{area, state}`. The caller names a *room*, never
    /// an entity: the expansion is Jarvis's own, derived from HA metadata and
    /// bounded by the allowlist, so there is no entity list a model could lie
    /// about. Text proposes the room; it never names the targets.
    fn target(
        &self,
        arguments: &CanonicalValue,
    ) -> Result<(AreaKey, String, LightState), ToolError> {
        let values = exact_string_args(arguments, &["area", "state"])?;
        let [area, state] = values[..] else {
            return Err(ToolError::SchemaInvalid(
                "home arguments must be exactly {area, state}".to_owned(),
            ));
        };
        if area.is_empty() || area.len() > MAX_AREA_NAME_CHARS * 4 {
            return Err(ToolError::SchemaInvalid(
                "home argument `area` is out of range".to_owned(),
            ));
        }
        let label = area_label(area);
        let key = normalize_area(&label).ok_or_else(|| {
            ToolError::SchemaInvalid("home argument `area` is not a usable area name".to_owned())
        })?;
        Ok((key, label, LightState::parse(state)?))
    }

    /// Resolve an area to the concrete allowlisted entity set.
    ///
    /// The candidate set is the *allowlist* — HA metadata only filters it. This
    /// direction is the whole security property: iterating HA and then checking
    /// the allowlist would be equivalent only as long as the check is never
    /// forgotten, while iterating the allowlist makes reaching a non-allowlisted
    /// entity structurally impossible.
    async fn resolve(
        &self,
        area: &AreaKey,
        cancel: &CancellationToken,
    ) -> Result<AreaResolution, ToolError> {
        let lights: Vec<EntityId> = self.allowlist.lights().cloned().collect();
        if lights.is_empty() {
            return Ok(AreaResolution::default());
        }
        // One bounded index read, and only when the cache cannot already answer
        // for every candidate. `cached` returns `None` for stale entries too, so
        // this is also the TTL refresh.
        if lights
            .iter()
            .any(|entity| self.client.cached(entity).is_none())
        {
            self.client.refresh_metadata(cancel).await?;
        }
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let mut resolution = AreaResolution::default();
        for entity in &lights {
            let Some(metadata) = self.client.cached(entity) else {
                // HA does not know this allowlisted entity at all. Unknown, not
                // absent — counted so the caveat can say so.
                resolution.unknown_area += 1;
                continue;
            };
            match metadata.area.as_deref().and_then(normalize_area) {
                Some(found) if found == *area => resolution.matched.push(metadata),
                Some(_) => {}
                None => resolution.unknown_area += 1,
            }
        }
        tracing::debug!(
            target: "jarvis.home",
            area = %area.0,
            matched = resolution.matched.len(),
            unknown_area = resolution.unknown_area,
            "resolved a home area to allowlisted lights",
        );
        Ok(resolution)
    }
}

#[async_trait]
impl ToolExecutor for HomeSetAreaLightsTool {
    async fn execute(
        &self,
        invocation: ToolInvocation,
        _grant: Option<ExecutionGrant>, // R1: auto-authorized, never carries a grant.
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let (key, label, desired) = self.target(&invocation.arguments)?;
        let resolution = self.resolve(&key, &cancel).await?;
        let total = resolution.matched.len();

        // Both refusals happen before any mutation, and both are errors: an
        // area that drove nothing must never be reported as a success.
        if total > MAX_AREA_ENTITIES {
            return Err(ToolError::Denied(format!(
                "{total} allowlisted lights are in the {label}; a plural command may drive at most \
                 {MAX_AREA_ENTITIES}. Name the lights individually."
            )));
        }
        if total == 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "No allowlisted lights are in the {label}.{}",
                unknown_area_caveat(resolution.unknown_area)
            )));
        }

        // Per-entity execution. One failure must neither abort the rest nor be
        // swallowed, so every outcome lands in exactly one bucket and every
        // bucket reaches the text below.
        let mut succeeded: Vec<String> = Vec::new();
        let mut undos: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut unreached: Vec<String> = Vec::new();
        // The executor's own deadline (M5 audit S1). It starts here, after
        // resolution, on purpose: resolution is a single bounded read that
        // mutates nothing, while this budget exists to bound the *mutating*
        // phase — the one whose interruption leaves the house half-changed with
        // nobody told. `tokio::time::Instant` rather than `std::time::Instant`
        // so the bound is exercisable under a paused test clock.
        let deadline = tokio::time::Instant::now() + AREA_FANOUT_BUDGET;
        for metadata in &resolution.matched {
            let entity_label = metadata.label();
            if cancel.is_cancelled() {
                skipped.push(entity_label);
                continue;
            }
            if tokio::time::Instant::now() >= deadline {
                // Stop *ourselves* rather than be dropped mid-call by the host
                // wrapper: the entities already driven are real and the owner is
                // about to be told exactly which ones they were. Remaining
                // entities are collected, not silently dropped.
                unreached.push(entity_label);
                continue;
            }
            match apply_one(&self.client, &metadata.entity_id, desired, &cancel).await {
                Ok(result) => {
                    succeeded.push(entity_label);
                    // Per-entity undo, derived from that entity's own pre-read.
                    undos.extend(result.compensation);
                }
                Err(ToolError::Cancelled) => skipped.push(entity_label),
                Err(error) => {
                    // The generic error text is logged, not spoken: the caller
                    // needs to know *which* light failed, and the adapter's
                    // error strings carry no HA detail worth repeating.
                    tracing::warn!(
                        target: "jarvis.home",
                        entity = %metadata.entity_id,
                        error = %error,
                        "home area command: an entity did not respond",
                    );
                    failed.push(entity_label);
                }
            }
        }

        if succeeded.is_empty() {
            // Total failure is reported as total failure. Rounding it up to a
            // partial success would be the same lie in the other direction.
            if failed.is_empty() {
                // Nothing was even attempted. Unreachable in practice — the
                // deadline is in the future when the loop starts, so the first
                // entity is always tried — but if it ever happens it is a
                // timeout, not a cancellation, and must not be mislabelled.
                if !unreached.is_empty() {
                    return Err(ToolError::Timeout(AREA_FANOUT_BUDGET));
                }
                return Err(ToolError::Cancelled);
            }
            return Err(ToolError::ExecutionFailed(format!(
                "None of the {total} {} in the {label} responded: {}.{}{}{}",
                lights_noun(total),
                join_labels(&failed),
                cancelled_caveat(&skipped),
                deadline_caveat(&unreached),
                unknown_area_caveat(resolution.unknown_area),
            )));
        }

        let verb = desired.as_str();
        let mut content = if succeeded.len() == total {
            if total == 1 {
                format!("Turned {verb} {} in the {label}.", succeeded[0])
            } else {
                format!(
                    "Turned {verb} all {total} lights in the {label}: {}.",
                    join_labels(&succeeded)
                )
            }
        } else {
            // Never rounded up: the count leads, and the survivors are named.
            format!(
                "Turned {verb} {} of {total} {} in the {label}: {}.",
                succeeded.len(),
                lights_noun(total),
                join_labels(&succeeded)
            )
        };
        if !failed.is_empty() {
            content.push_str(&format!(" {} did not respond.", join_labels(&failed)));
        }
        content.push_str(&cancelled_caveat(&skipped));
        content.push_str(&deadline_caveat(&unreached));
        content.push_str(&unknown_area_caveat(resolution.unknown_area));

        Ok(ToolResult {
            content,
            truncated: false,
            // The undo restores each light to *its own* prior state. A blanket
            // "turn them all off" would be wrong for any light that was already
            // on, and only the entities actually mutated appear here.
            compensation: (!undos.is_empty()).then(|| undos.join(" ")),
        })
    }

    /// Shape only. Resolution needs HA I/O and this hook is synchronous, so
    /// there is no entity to check here — and nothing is lost by that: the
    /// arguments name no entity, and the executor can only ever enumerate the
    /// allowlist. The bound is structural rather than validated.
    fn validate_args(&self, arguments: &CanonicalValue) -> Result<(), ToolError> {
        self.target(arguments).map(|_| ())
    }
}
