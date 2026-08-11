//! The **app spec**: the "validated templates" half of FR-18 (docs/06 §6,
//! docs/08 §6, ADR-029).
//!
//! A generated web app is described by a small JSON document — a template id,
//! a title, the capabilities the app declares it needs, the data bindings it
//! wants rendered, and its build limits. That document is *model-authored*,
//! which makes it untrusted input in the strongest sense (invariant #1): it may
//! **name** a template and capabilities from the host's own closed vocabularies
//! and nothing else. Everything here is pure validation — no I/O, no build, no
//! authority.
//!
//! Validation happens **before a build is ever started**, so an invalid spec
//! fails in the domain rather than deep inside a Node worker where the failure
//! mode is a timeout and the diagnosis is a log grep. An [`AppSpec`] can only be
//! produced by [`AppSpec::validate`]; its fields are private, so "an unvalidated
//! spec" is unrepresentable downstream.
//!
//! The spec is *not* a grant. A declared capability is at most an authorization
//! to **ask** at bridge time (F6.5); the host still runs `policy::evaluate`
//! against the live registry, and R2+ still mints an `ExecutionGrant`.

use std::fmt;

use thiserror::Error;

use crate::artifact::{Capability, echo_untrusted};
use crate::policy::RiskLevel;

// --- host-owned limits ------------------------------------------------------

/// The largest app-spec document the host will even parse, in bytes. The spec
/// arrives as model output; a cap here is what keeps a runaway generation from
/// becoming a memory event before any structural check runs (NFR-15, docs/09
/// §5).
pub const MAX_APP_SPEC_BYTES: usize = 16 * 1024;

/// The most capabilities one app may declare — the size of the whole host
/// vocabulary, since duplicates are rejected. Derived rather than written down,
/// so it cannot drift below the vocabulary and make a legitimate all-capability
/// spec unrepresentable, nor above it and pretend to bound something it does
/// not (F6.1 review).
pub const MAX_CAPABILITIES: usize = Capability::ALL.len();

/// The most data bindings one app may declare.
pub const MAX_BINDINGS: usize = 32;

/// The longest app title, in characters (not bytes) — it is rendered in a
/// window/panel chrome, not stored as prose.
pub const MAX_TITLE_CHARS: usize = 80;

/// The largest built bundle the host will accept from the builder, in bytes
/// (docs/06 §6 "size/time limits"). A spec may request *less*; never more.
pub const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024;

/// The longest build the host will allow, in seconds (docs/06 §6). A spec may
/// request *less*; never more. A Node build is the heaviest thing this system
/// spawns, so the ceiling is deliberately tight for an 8 GB ultrabook.
pub const MAX_BUILD_SECONDS: u32 = 120;

// --- template ---------------------------------------------------------------

/// A host-owned app template (FR-18 "validated templates"). Closed for the same
/// reason [`Capability`] is: a template id selects the exact locked source tree
/// and lockfile the builder uses (ADR-029), so an id a model could invent is an
/// id the builder could not pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TemplateId {
    /// A read-mostly dashboard of cards bound to declared data (F6.6).
    Dashboard,
}

impl TemplateId {
    /// Every template the host ships. A test asserts this covers the enum.
    pub const ALL: [TemplateId; 1] = [TemplateId::Dashboard];

    /// The stable, versioned template id. Versioned independently of the enum
    /// so the locked template can evolve (new lockfile, new source tree) behind
    /// a new id without silently changing what an old spec meant.
    pub fn as_str(self) -> &'static str {
        match self {
            TemplateId::Dashboard => "dashboard/v1",
        }
    }
}

impl fmt::Display for TemplateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// --- bindings ---------------------------------------------------------------

/// The name a binding is referenced by inside the template
/// (`[a-z][a-z0-9_]*`). Validated so a binding name can never be interpolated
/// into generated source as something other than an identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingName(String);

impl BindingName {
    /// Longest binding name, in bytes (ASCII-only by construction).
    pub const MAX_BYTES: usize = 40;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a binding points at — an entity id, a list id, whatever the backing
/// tool addresses. **Opaque to the domain**: it is carried, length-capped and
/// control-stripped, never interpreted. The bridge re-resolves it through the
/// backing tool's own allowlist at call time (F6.5), so a target here confers
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingTarget(String);

impl BindingTarget {
    /// Longest binding target, in bytes.
    pub const MAX_BYTES: usize = 128;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One piece of live data the app renders: a name the template binds, the
/// capability that supplies it, and the resource it addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBinding {
    name: BindingName,
    capability: Capability,
    target: BindingTarget,
}

impl DataBinding {
    pub fn name(&self) -> &BindingName {
        &self.name
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn target(&self) -> &BindingTarget {
        &self.target
    }
}

// --- limits -----------------------------------------------------------------

/// Build limits in force for one app (docs/06 §6). Constructed only by
/// validation, which clamps nothing silently: a spec asking for more than the
/// host ceiling is **rejected**, not quietly reduced, so the caller learns its
/// request was impossible rather than getting a build it did not ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLimits {
    max_bundle_bytes: u64,
    max_build_seconds: u32,
}

impl AppLimits {
    /// The host ceiling — what an app gets when its spec requests nothing.
    pub const fn host_default() -> AppLimits {
        AppLimits {
            max_bundle_bytes: MAX_BUNDLE_BYTES,
            max_build_seconds: MAX_BUILD_SECONDS,
        }
    }

    pub fn max_bundle_bytes(self) -> u64 {
        self.max_bundle_bytes
    }

    pub fn max_build_seconds(self) -> u32 {
        self.max_build_seconds
    }
}

// --- the unvalidated draft --------------------------------------------------

/// The raw, unvalidated spec as it arrives from the boundary (a model's JSON,
/// converted by `jarvis-contracts`). Strings, not enums, on purpose: an unknown
/// template or capability must fail as a *typed domain rejection* naming what
/// was wrong — which the orchestrator can hand back for a replan and the audit
/// trail can record — not as an opaque deserialization error at the edge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppSpecDraft {
    pub template: String,
    pub title: String,
    pub capabilities: Vec<String>,
    pub bindings: Vec<DataBindingDraft>,
    /// Absent means "the host ceiling" ([`AppLimits::host_default`]).
    pub limits: Option<AppLimitsDraft>,
}

/// The raw form of one [`DataBinding`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataBindingDraft {
    pub name: String,
    pub capability: String,
    pub target: String,
}

/// The raw form of [`AppLimits`]. Both fields optional: a spec may tighten one
/// without naming the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppLimitsDraft {
    pub max_bundle_bytes: Option<u64>,
    pub max_build_seconds: Option<u32>,
}

// --- errors -----------------------------------------------------------------

/// Why a spec was rejected. Every variant that echoes untrusted text passes it
/// through [`echo_untrusted`] first (clamped, control- and bidi-stripped), so a
/// rejection reason is safe to log, audit and render.
/// Why a free-text field was rejected. Carried alongside the offending value so
/// a caller — a replanning model, an error body, a test — learns *which rule*
/// fired rather than only that something was wrong. Without it, five distinct
/// title rules collapse into one indistinguishable variant and a test cannot
/// tell a swapped check from a correct one (F6.1 review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextReason {
    /// Empty, or nothing but whitespace.
    Empty,
    /// Over the field's length limit.
    TooLong,
    /// Contains a control, bidi, zero-width or line-separator character.
    UnsafeText,
    /// Contains a character outside the field's permitted set.
    BadCharacter,
}

impl fmt::Display for TextReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TextReason::Empty => "empty",
            TextReason::TooLong => "too long",
            TextReason::UnsafeText => "contains invisible or control characters",
            TextReason::BadCharacter => "contains a disallowed character",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AppSpecError {
    #[error("app spec is {bytes} bytes, over the {max}-byte limit")]
    SpecTooLarge { bytes: usize, max: usize },

    #[error("unknown app template {0:?}")]
    UnknownTemplate(String),

    #[error("unknown capability {0:?}")]
    UnknownCapability(String),

    /// A *binding* named a capability outside the vocabulary. Distinct from
    /// [`AppSpecError::UnknownCapability`] so the offending binding is named —
    /// the top-level variant has no binding to report.
    #[error("binding {binding:?} names unknown capability {capability:?}")]
    UnknownBindingCapability { binding: String, capability: String },

    #[error("capability {0} is declared more than once")]
    DuplicateCapability(Capability),

    #[error("{count} capabilities declared, over the limit of {max}")]
    TooManyCapabilities { count: usize, max: usize },

    #[error("invalid app title ({reason}): {value:?}")]
    InvalidTitle { reason: TextReason, value: String },

    #[error("{count} bindings declared, over the limit of {max}")]
    TooManyBindings { count: usize, max: usize },

    #[error("invalid binding name ({reason}): {value:?}")]
    InvalidBindingName { reason: TextReason, value: String },

    #[error("binding name {0:?} is used more than once")]
    DuplicateBindingName(String),

    #[error("invalid binding target ({reason}): {value:?}")]
    InvalidBindingTarget { reason: TextReason, value: String },

    /// The load-bearing cross-check: a binding may only draw on a capability the
    /// spec **declared**. Without it, `capabilities` would describe one thing
    /// and the app would render another.
    #[error("binding {binding:?} uses capability {capability}, which the spec does not declare")]
    UndeclaredBindingCapability {
        binding: String,
        capability: Capability,
    },

    #[error("requested {field} of {requested} exceeds the host maximum of {max}")]
    LimitAboveHostMaximum {
        field: &'static str,
        requested: u64,
        max: u64,
    },

    #[error("requested {field} must be greater than zero")]
    LimitIsZero { field: &'static str },
}

// --- the validated spec -----------------------------------------------------

/// A validated app spec. Private fields with getters: the only way to hold one
/// is to have passed [`AppSpec::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSpec {
    template: TemplateId,
    title: String,
    /// Sorted and duplicate-free (duplicates are rejected, not deduped).
    capabilities: Vec<Capability>,
    bindings: Vec<DataBinding>,
    limits: AppLimits,
}

impl AppSpec {
    /// Validate a draft into an [`AppSpec`] (FR-18, docs/06 §6).
    ///
    /// `source_bytes` is the length of the document the draft was parsed from.
    /// It is a required argument rather than something the domain recomputes
    /// because the domain has no JSON library — and making it required means
    /// the size check cannot be the one a boundary forgets to call.
    /// Checks run cheapest-and-most-total first: document size, then template,
    /// then title, then the capability set as a whole, then bindings, then
    /// limits. Cardinality is checked before per-item parsing so a hostile spec
    /// cannot make the validator do 10,000 parses to learn it was too long —
    /// the same fail-fast-on-size shape as [`AppSpecError::SpecTooLarge`].
    pub fn validate(draft: AppSpecDraft, source_bytes: usize) -> Result<AppSpec, AppSpecError> {
        if source_bytes > MAX_APP_SPEC_BYTES {
            return Err(AppSpecError::SpecTooLarge {
                bytes: source_bytes,
                max: MAX_APP_SPEC_BYTES,
            });
        }

        let template = TemplateId::ALL
            .into_iter()
            .find(|t| t.as_str() == draft.template.trim())
            .ok_or_else(|| AppSpecError::UnknownTemplate(echo_untrusted(&draft.template)))?;

        let title = validate_title(&draft.title)?;
        let capabilities = validate_capabilities(&draft.capabilities)?;
        let bindings = validate_bindings(draft.bindings, &capabilities)?;
        let limits = validate_limits(draft.limits)?;

        Ok(AppSpec {
            template,
            title,
            capabilities,
            bindings,
            limits,
        })
    }

    pub fn template(&self) -> TemplateId {
        self.template
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Declared capabilities, sorted and duplicate-free. May be empty: an app
    /// that renders only its own static content needs no authority, and that is
    /// the best possible spec — least privilege is the default, not an error.
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    pub fn bindings(&self) -> &[DataBinding] {
        &self.bindings
    }

    pub fn limits(&self) -> AppLimits {
        self.limits
    }

    /// The highest **declared** risk tier across the spec's capabilities, or
    /// `None` for a capability-free app. Drives the approval preview for
    /// `app.generate` (F6.6).
    ///
    /// Returns a [`DeclaredRisk`], not a bare [`RiskLevel`], so that
    /// substituting this preview for a policy decision does not typecheck —
    /// `if spec.max_declared_risk() <= Some(RiskLevel::R1) { skip_approval() }`
    /// would have compiled, and a doc comment saying "never a policy decision"
    /// is not a mechanism (F6.1 review).
    ///
    /// `None` does **not** mean "no approval needed": generating an app spawns
    /// a build worker, and that action's tier comes from `app.generate`'s own
    /// registered `ToolPolicy`, never from this.
    pub fn max_declared_risk(&self) -> Option<DeclaredRisk> {
        self.capabilities
            .iter()
            .map(|c| c.risk())
            .max()
            .map(DeclaredRisk)
    }
}

/// A risk tier a *spec declared*, as opposed to one `policy::evaluate` decided.
/// Deliberately not comparable with [`RiskLevel`]: reading the tier for an
/// approval preview is fine, and it is the only thing this type makes easy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredRisk(RiskLevel);

impl DeclaredRisk {
    /// The tier, for display in approval text. Naming the method after what it
    /// is for makes a call site that uses it as a decision read wrong.
    pub fn for_display(self) -> RiskLevel {
        self.0
    }
}

impl fmt::Display for DeclaredRisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

// --- validation helpers -----------------------------------------------------

/// Text that may never appear in a spec's human-visible or interpolated fields:
/// C0/C1 controls (including `\n`/`\t` — a *title* is a single line, unlike tool
/// result prose) and Unicode bidi/zero-width format characters (CF-13). Both
/// classes exist here only to spoof what a human reads or to splice hidden text
/// through a renderer, so they are rejected rather than stripped: silently
/// altering a spec would mean building an app the caller did not describe.
fn has_unsafe_text(s: &str) -> bool {
    s.chars().any(crate::tools::is_unsafe_in_single_line)
}

/// ASCII characters a title may not contain because they are *syntax* in the
/// contexts a title is carried into: an HTML text node and attribute, and a JS
/// string/template literal in the generated bundle (F6.2).
///
/// The builder must escape regardless — this is defence in depth, not a licence
/// to skip escaping. But `BindingName` is already locked to an identifier
/// charset *because* it is interpolated, and a title that reaches the same
/// generated source with no equivalent guarantee is the asymmetry the F6.1
/// review flagged. Ordinary titles ("Kitchen Dashboard", "Lights & Heating")
/// are unaffected.
const TITLE_FORBIDDEN: [char; 9] = ['<', '>', '"', '\'', '`', '\\', '{', '}', '$'];

fn validate_title(raw: &str) -> Result<String, AppSpecError> {
    let title = raw.trim();
    let invalid = |reason| AppSpecError::InvalidTitle {
        reason,
        value: echo_untrusted(raw),
    };
    if title.is_empty() {
        return Err(invalid(TextReason::Empty));
    }
    // Length before content (the module's own cheapest-check-first discipline):
    // a huge title is rejected without scanning all of it. Characters, not
    // bytes — the cap exists so the title fits a window chrome, and "é"
    // occupies one column whatever its UTF-8 length.
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(invalid(TextReason::TooLong));
    }
    if has_unsafe_text(title) {
        return Err(invalid(TextReason::UnsafeText));
    }
    if title.chars().any(|c| TITLE_FORBIDDEN.contains(&c)) {
        return Err(invalid(TextReason::BadCharacter));
    }
    Ok(title.to_owned())
}

/// Parse the declared capability set. Cardinality first, then per-entry parse,
/// then duplicate detection **in declaration order** so the reported duplicate
/// is the one a reader would point at. Duplicates are an error, never a silent
/// dedupe: a spec that names the same authority twice is a spec whose author
/// did not mean what it says, and quietly fixing it hides that.
fn validate_capabilities(raw: &[String]) -> Result<Vec<Capability>, AppSpecError> {
    if raw.len() > MAX_CAPABILITIES {
        return Err(AppSpecError::TooManyCapabilities {
            count: raw.len(),
            max: MAX_CAPABILITIES,
        });
    }
    let mut declared: Vec<Capability> = Vec::with_capacity(raw.len());
    for name in raw {
        let capability = name
            .parse::<Capability>()
            .map_err(|_| AppSpecError::UnknownCapability(echo_untrusted(name)))?;
        if declared.contains(&capability) {
            return Err(AppSpecError::DuplicateCapability(capability));
        }
        declared.push(capability);
    }
    // Sorted so two specs declaring the same authorities in different orders
    // are the same spec — the manifest, the approval text and any future
    // capability-set comparison all read one canonical order.
    declared.sort_unstable();
    Ok(declared)
}

fn validate_bindings(
    raw: Vec<DataBindingDraft>,
    declared: &[Capability],
) -> Result<Vec<DataBinding>, AppSpecError> {
    if raw.len() > MAX_BINDINGS {
        return Err(AppSpecError::TooManyBindings {
            count: raw.len(),
            max: MAX_BINDINGS,
        });
    }
    let mut bindings: Vec<DataBinding> = Vec::with_capacity(raw.len());
    for binding in raw {
        let name = validate_binding_name(&binding.name)?;
        if bindings.iter().any(|b| b.name == name) {
            return Err(AppSpecError::DuplicateBindingName(name.0));
        }
        let capability = binding.capability.parse::<Capability>().map_err(|_| {
            AppSpecError::UnknownBindingCapability {
                binding: name.0.clone(),
                capability: echo_untrusted(&binding.capability),
            }
        })?;
        // docs/06 §6's "undeclared capability ⇒ reject", enforced at spec time:
        // the capability list is what the manifest records and what the bridge
        // will honour, so a binding drawing on anything outside it would be a
        // manifest that under-describes the app.
        if !declared.contains(&capability) {
            return Err(AppSpecError::UndeclaredBindingCapability {
                binding: name.0,
                capability,
            });
        }
        let target = validate_binding_target(&binding.target)?;
        bindings.push(DataBinding {
            name,
            capability,
            target,
        });
    }
    Ok(bindings)
}

fn validate_binding_name(raw: &str) -> Result<BindingName, AppSpecError> {
    let invalid = |reason| AppSpecError::InvalidBindingName {
        reason,
        value: echo_untrusted(raw),
    };
    if raw.is_empty() {
        return Err(invalid(TextReason::Empty));
    }
    if raw.len() > BindingName::MAX_BYTES {
        return Err(invalid(TextReason::TooLong));
    }
    let mut chars = raw.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if first_ok && rest_ok {
        Ok(BindingName(raw.to_owned()))
    } else {
        Err(invalid(TextReason::BadCharacter))
    }
}

fn validate_binding_target(raw: &str) -> Result<BindingTarget, AppSpecError> {
    let target = raw.trim();
    let invalid = |reason| AppSpecError::InvalidBindingTarget {
        reason,
        value: echo_untrusted(raw),
    };
    if target.is_empty() {
        return Err(invalid(TextReason::Empty));
    }
    if target.len() > BindingTarget::MAX_BYTES {
        return Err(invalid(TextReason::TooLong));
    }
    if has_unsafe_text(target) {
        return Err(invalid(TextReason::UnsafeText));
    }
    // A **positive** charset, wide enough for every address the vocabulary can
    // reach (Home Assistant entity/scene ids, ULIDs) and narrow enough that a
    // target cannot carry markup, quotes or path traversal into the generated
    // bundle. The bridge still re-resolves the target through the backing tool's
    // own allowlist at call time (F6.5) — that is what decides *authority*; this
    // is about what can be interpolated into source (F6.1 review).
    if !target
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
    {
        return Err(invalid(TextReason::BadCharacter));
    }
    Ok(BindingTarget(target.to_owned()))
}

/// Absent limits mean the host ceiling. A *lower* request is honoured verbatim
/// (an app may bind itself more tightly than the host does); a higher one is
/// rejected rather than clamped, so a caller never receives a build it did not
/// ask for under a limit it did not choose.
fn validate_limits(raw: Option<AppLimitsDraft>) -> Result<AppLimits, AppSpecError> {
    let host = AppLimits::host_default();
    let Some(draft) = raw else {
        return Ok(host);
    };
    let max_bundle_bytes = match draft.max_bundle_bytes {
        None => host.max_bundle_bytes,
        Some(requested) => check_limit("maxBundleBytes", requested, MAX_BUNDLE_BYTES)?,
    };
    let max_build_seconds = match draft.max_build_seconds {
        None => host.max_build_seconds,
        // Checked in `u32` throughout: converting to `u64` and back would need a
        // fallback, and the only natural-looking fallback (`unwrap_or(host
        // ceiling)`) *raises* an out-of-range request to the maximum — exactly
        // the clamp-instead-of-reject behaviour this function exists to prevent
        // (F6.1 review). No conversion, no fallback, no way to get it wrong.
        Some(requested) => check_limit_u32("maxBuildSeconds", requested, MAX_BUILD_SECONDS)?,
    };
    Ok(AppLimits {
        max_bundle_bytes,
        max_build_seconds,
    })
}

fn check_limit(field: &'static str, requested: u64, max: u64) -> Result<u64, AppSpecError> {
    if requested == 0 {
        return Err(AppSpecError::LimitIsZero { field });
    }
    if requested > max {
        return Err(AppSpecError::LimitAboveHostMaximum {
            field,
            requested,
            max,
        });
    }
    Ok(requested)
}

/// [`check_limit`] for a `u32`-valued limit. The error still reports `u64`, so
/// both limits read the same in a message.
fn check_limit_u32(field: &'static str, requested: u32, max: u32) -> Result<u32, AppSpecError> {
    check_limit(field, u64::from(requested), u64::from(max))?;
    Ok(requested)
}

// =============================================================================
// F6.1 spec-validation table (docs/06 §6, the M6 feature list, invariant #1).
// Assertions are on specific `AppSpecError` variants and their `reason`, never
// bare `is_err()`, per docs/07 §3 — so swapping two checks fails a test rather
// than passing one.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// One perturbation point per test: every field here is independently
    /// valid, so an error in a single-field test can only be attributed to the
    /// one field that test changed.
    fn valid_draft() -> AppSpecDraft {
        AppSpecDraft {
            template: TemplateId::Dashboard.as_str().to_owned(),
            title: "Kitchen Dashboard".to_owned(),
            capabilities: vec![cap(Capability::HomeReadState)],
            bindings: vec![DataBindingDraft {
                name: "kitchen_temp".to_owned(),
                capability: cap(Capability::HomeReadState),
                target: "sensor.kitchen_temperature".to_owned(),
            }],
            limits: None,
        }
    }

    fn cap(c: Capability) -> String {
        c.as_str().to_owned()
    }

    fn draft_with_binding_name(name: &str) -> AppSpecDraft {
        let mut draft = valid_draft();
        draft.bindings[0].name = name.to_owned();
        draft
    }

    fn draft_with_binding_target(target: &str) -> AppSpecDraft {
        let mut draft = valid_draft();
        draft.bindings[0].target = target.to_owned();
        draft
    }

    fn binding_for(idx: usize) -> DataBindingDraft {
        DataBindingDraft {
            name: format!("b{idx}"),
            capability: cap(Capability::HomeReadState),
            target: format!("sensor.b{idx}"),
        }
    }

    // --- 1. happy path -------------------------------------------------

    // FR-18 / docs/06 §6: a valid spec validates; capabilities come back
    // sorted and duplicate-free; every field is reachable through a getter.
    #[test]
    fn happy_path_validates_capabilities_sorted_and_getters_carry_every_field() {
        let draft = AppSpecDraft {
            template: TemplateId::Dashboard.as_str().to_owned(),
            title: "Kitchen Dashboard".to_owned(),
            // Declared out of enum order to prove the validated spec sorts.
            capabilities: vec![
                cap(Capability::HomeExecuteScene),
                cap(Capability::HomeReadState),
            ],
            bindings: vec![
                DataBindingDraft {
                    name: "scene_binding".to_owned(),
                    capability: cap(Capability::HomeExecuteScene),
                    target: "scene.movie_night".to_owned(),
                },
                DataBindingDraft {
                    name: "state_binding".to_owned(),
                    capability: cap(Capability::HomeReadState),
                    target: "sensor.kitchen_temperature".to_owned(),
                },
            ],
            limits: Some(AppLimitsDraft {
                max_bundle_bytes: Some(1024),
                max_build_seconds: Some(10),
            }),
        };

        let spec = AppSpec::validate(draft, 200).expect("valid draft should validate");

        assert_eq!(spec.template(), TemplateId::Dashboard);
        assert_eq!(spec.title(), "Kitchen Dashboard");
        assert_eq!(
            spec.capabilities(),
            &[Capability::HomeReadState, Capability::HomeExecuteScene],
            "capabilities must come back sorted, duplicate-free"
        );
        assert_eq!(spec.bindings().len(), 2);
        let state_binding = spec
            .bindings()
            .iter()
            .find(|b| b.name().as_str() == "state_binding")
            .expect("state_binding present");
        assert_eq!(state_binding.capability(), Capability::HomeReadState);
        assert_eq!(
            state_binding.target().as_str(),
            "sensor.kitchen_temperature"
        );
        assert_eq!(spec.limits().max_bundle_bytes(), 1024);
        assert_eq!(spec.limits().max_build_seconds(), 10);
    }

    // --- 2. empty capability set is valid (least privilege default) ----

    #[test]
    fn empty_capability_set_is_valid_least_privilege_default() {
        let mut draft = valid_draft();
        draft.capabilities = vec![];
        draft.bindings = vec![];
        let spec = AppSpec::validate(draft, 50).expect("empty capability set is valid");
        assert!(spec.capabilities().is_empty());
        assert_eq!(spec.max_declared_risk(), None);
    }

    // --- 3. unknown template --------------------------------------------

    #[test]
    fn unknown_template_is_rejected() {
        let mut draft = valid_draft();
        draft.template = "not-a-real-template/v1".to_owned();
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(matches!(err, AppSpecError::UnknownTemplate(_)));
    }

    // --- 4. unknown capability -------------------------------------------

    #[test]
    fn unknown_capability_is_rejected() {
        let mut draft = valid_draft();
        draft.capabilities = vec!["not.a.real.capability".to_owned()];
        draft.bindings = vec![];
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(matches!(err, AppSpecError::UnknownCapability(_)));
    }

    // --- 5. oversized spec -------------------------------------------------

    // NFR-15 / docs/09 §5: the size gate must fire even when every other field
    // is otherwise perfectly valid.
    #[test]
    fn spec_over_the_byte_limit_is_rejected_even_when_otherwise_valid() {
        let draft = valid_draft();
        let err = AppSpec::validate(draft, MAX_APP_SPEC_BYTES + 1).unwrap_err();
        match err {
            AppSpecError::SpecTooLarge { bytes, max } => {
                assert_eq!(bytes, MAX_APP_SPEC_BYTES + 1);
                assert_eq!(max, MAX_APP_SPEC_BYTES);
            }
            other => panic!("expected SpecTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn spec_at_exactly_the_byte_limit_is_accepted() {
        let draft = valid_draft();
        assert!(AppSpec::validate(draft, MAX_APP_SPEC_BYTES).is_ok());
    }

    // --- 6. duplicate capability -------------------------------------------

    #[test]
    fn duplicate_capability_is_rejected_not_deduped() {
        let mut draft = valid_draft();
        draft.capabilities = vec![
            cap(Capability::HomeReadState),
            cap(Capability::HomeReadState),
        ];
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(matches!(
            err,
            AppSpecError::DuplicateCapability(Capability::HomeReadState)
        ));
    }

    // --- 7. TooManyCapabilities / TooManyBindings boundaries ---------------

    // Decided (F6.1 review): raw cardinality is checked BEFORE per-entry
    // parsing and duplicate detection, so an over-long list is refused without
    // parsing all of it — the same fail-fast-on-size shape as `SpecTooLarge`.
    // This test therefore expects `TooManyCapabilities`, not `Duplicate…`,
    // even though the over-long input is necessarily duplicate-laden (the
    // vocabulary has exactly `MAX_CAPABILITIES` members).
    #[test]
    fn too_many_capabilities_over_the_limit_is_rejected() {
        let mut draft = valid_draft();
        draft.capabilities = vec![cap(Capability::HomeReadState); MAX_CAPABILITIES + 1];
        let err = AppSpec::validate(draft, 500).unwrap_err();
        match err {
            AppSpecError::TooManyCapabilities { count, max } => {
                assert_eq!(count, MAX_CAPABILITIES + 1);
                assert_eq!(max, MAX_CAPABILITIES);
            }
            other => {
                panic!("expected TooManyCapabilities (see ambiguity note above), got {other:?}")
            }
        }
    }

    #[test]
    fn all_known_capabilities_declared_at_once_is_within_the_limit() {
        // Documents that today's full host vocabulary (3) sits well under
        // MAX_CAPABILITIES (8) — see the ambiguity note on the previous test.
        let mut draft = valid_draft();
        draft.capabilities = Capability::ALL.iter().map(|c| cap(*c)).collect();
        draft.bindings = vec![];
        let spec = AppSpec::validate(draft, 500)
            .expect("3 distinct capabilities is within the limit of 8");
        assert_eq!(spec.capabilities().len(), Capability::ALL.len());
    }

    #[test]
    fn bindings_at_exactly_the_limit_are_accepted() {
        let mut draft = valid_draft();
        draft.bindings = (0..MAX_BINDINGS).map(binding_for).collect();
        let spec = AppSpec::validate(draft, 8000).expect("exactly MAX_BINDINGS bindings is valid");
        assert_eq!(spec.bindings().len(), MAX_BINDINGS);
    }

    #[test]
    fn bindings_over_the_limit_is_rejected() {
        let mut draft = valid_draft();
        draft.bindings = (0..MAX_BINDINGS + 1).map(binding_for).collect();
        let err = AppSpec::validate(draft, 8000).unwrap_err();
        match err {
            AppSpecError::TooManyBindings { count, max } => {
                assert_eq!(count, MAX_BINDINGS + 1);
                assert_eq!(max, MAX_BINDINGS);
            }
            other => panic!("expected TooManyBindings, got {other:?}"),
        }
    }

    // --- 8. title ------------------------------------------------------

    #[test]
    fn title_empty_is_rejected() {
        let mut draft = valid_draft();
        draft.title = "".to_owned();
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidTitle {
                    reason: TextReason::Empty,
                    ..
                }
            ),
            "expected InvalidTitle/Empty, got {err:?}"
        );
    }

    #[test]
    fn title_whitespace_only_is_rejected() {
        let mut draft = valid_draft();
        draft.title = "   \t  ".to_owned();
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidTitle {
                    reason: TextReason::Empty,
                    ..
                }
            ),
            "expected InvalidTitle/Empty, got {err:?}"
        );
    }

    #[test]
    fn title_over_max_chars_is_rejected() {
        let mut draft = valid_draft();
        draft.title = "a".repeat(MAX_TITLE_CHARS + 1);
        let err = AppSpec::validate(draft, 200).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidTitle {
                    reason: TextReason::TooLong,
                    ..
                }
            ),
            "expected InvalidTitle/TooLong, got {err:?}"
        );
    }

    // MAX_TITLE_CHARS is documented as characters, not bytes — prove it with a
    // multi-byte character whose byte length would otherwise trip a byte cap.
    #[test]
    fn title_at_exactly_max_chars_multibyte_is_accepted_proving_char_not_byte_count() {
        let mut draft = valid_draft();
        draft.title = "é".repeat(MAX_TITLE_CHARS);
        assert_eq!(draft.title.chars().count(), MAX_TITLE_CHARS);
        assert!(
            draft.title.len() > MAX_TITLE_CHARS,
            "sanity check: 'é' is multi-byte, so byte length must exceed char count"
        );
        let spec = AppSpec::validate(draft, 1000)
            .expect("exactly MAX_TITLE_CHARS multi-byte characters is valid");
        assert_eq!(spec.title().chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn title_over_max_chars_multibyte_is_rejected_proving_char_not_byte_count() {
        let mut draft = valid_draft();
        draft.title = "é".repeat(MAX_TITLE_CHARS + 1);
        let err = AppSpec::validate(draft, 1000).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidTitle {
                    reason: TextReason::TooLong,
                    ..
                }
            ),
            "expected InvalidTitle/TooLong, got {err:?}"
        );
    }

    #[test]
    fn title_with_control_character_is_rejected() {
        let mut draft = valid_draft();
        draft.title = "Kitchen\u{0007}Dashboard".to_owned();
        let err = AppSpec::validate(draft, 200).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidTitle {
                    reason: TextReason::UnsafeText,
                    ..
                }
            ),
            "expected InvalidTitle/UnsafeText, got {err:?}"
        );
    }

    #[test]
    fn title_with_bidi_override_is_rejected() {
        let mut draft = valid_draft();
        draft.title = "Kitchen\u{202E}Dashboard".to_owned();
        let err = AppSpec::validate(draft, 200).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidTitle {
                    reason: TextReason::UnsafeText,
                    ..
                }
            ),
            "expected InvalidTitle/UnsafeText, got {err:?}"
        );
    }

    #[test]
    fn title_normal_is_accepted() {
        let draft = valid_draft();
        let spec = AppSpec::validate(draft, 200).expect("normal title is valid");
        assert_eq!(spec.title(), "Kitchen Dashboard");
    }

    // --- 9. binding names ------------------------------------------------

    #[test]
    fn binding_name_rejects_uppercase() {
        let err = AppSpec::validate(draft_with_binding_name("Kitchen"), 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingName {
                    reason: TextReason::BadCharacter,
                    ..
                }
            ),
            "expected InvalidBindingName/BadCharacter, got {err:?}"
        );
    }

    #[test]
    fn binding_name_rejects_leading_digit() {
        let err = AppSpec::validate(draft_with_binding_name("1kitchen"), 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingName {
                    reason: TextReason::BadCharacter,
                    ..
                }
            ),
            "expected InvalidBindingName/BadCharacter, got {err:?}"
        );
    }

    #[test]
    fn binding_name_rejects_empty() {
        let err = AppSpec::validate(draft_with_binding_name(""), 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingName {
                    reason: TextReason::Empty,
                    ..
                }
            ),
            "expected InvalidBindingName/Empty, got {err:?}"
        );
    }

    #[test]
    fn binding_name_rejects_hyphen() {
        let err = AppSpec::validate(draft_with_binding_name("kitchen-temp"), 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingName {
                    reason: TextReason::BadCharacter,
                    ..
                }
            ),
            "expected InvalidBindingName/BadCharacter, got {err:?}"
        );
    }

    #[test]
    fn binding_name_rejects_dot() {
        let err = AppSpec::validate(draft_with_binding_name("kitchen.temp"), 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingName {
                    reason: TextReason::BadCharacter,
                    ..
                }
            ),
            "expected InvalidBindingName/BadCharacter, got {err:?}"
        );
    }

    #[test]
    fn binding_name_rejects_over_max_bytes() {
        let name = "a".repeat(BindingName::MAX_BYTES + 1);
        let err = AppSpec::validate(draft_with_binding_name(&name), 200).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingName {
                    reason: TextReason::TooLong,
                    ..
                }
            ),
            "expected InvalidBindingName/TooLong, got {err:?}"
        );
    }

    #[test]
    fn binding_name_accepts_at_exactly_max_bytes() {
        let name = "a".repeat(BindingName::MAX_BYTES);
        let spec = AppSpec::validate(draft_with_binding_name(&name), 200)
            .expect("exactly MAX_BYTES binding name is valid");
        assert_eq!(spec.bindings()[0].name().as_str(), name);
    }

    #[test]
    fn duplicate_binding_name_is_rejected() {
        let mut draft = valid_draft();
        let first_name = draft.bindings[0].name.clone();
        draft.bindings.push(DataBindingDraft {
            name: first_name,
            capability: cap(Capability::HomeReadState),
            target: "sensor.other".to_owned(),
        });
        let err = AppSpec::validate(draft, 200).unwrap_err();
        assert!(matches!(err, AppSpecError::DuplicateBindingName(_)));
    }

    // --- 10. binding targets -----------------------------------------------

    #[test]
    fn binding_target_rejects_empty() {
        let err = AppSpec::validate(draft_with_binding_target(""), 100).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingTarget {
                    reason: TextReason::Empty,
                    ..
                }
            ),
            "expected InvalidBindingTarget/Empty, got {err:?}"
        );
    }

    #[test]
    fn binding_target_rejects_over_max_bytes() {
        let target = "a".repeat(BindingTarget::MAX_BYTES + 1);
        let err = AppSpec::validate(draft_with_binding_target(&target), 300).unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingTarget {
                    reason: TextReason::TooLong,
                    ..
                }
            ),
            "expected InvalidBindingTarget/TooLong, got {err:?}"
        );
    }

    #[test]
    fn binding_target_accepts_at_exactly_max_bytes() {
        let target = "a".repeat(BindingTarget::MAX_BYTES);
        let spec = AppSpec::validate(draft_with_binding_target(&target), 300)
            .expect("exactly MAX_BYTES binding target is valid");
        assert_eq!(spec.bindings()[0].target().as_str(), target);
    }

    #[test]
    fn binding_target_rejects_control_characters() {
        let err = AppSpec::validate(draft_with_binding_target("sensor.kit\u{0007}chen"), 100)
            .unwrap_err();
        assert!(
            matches!(
                err,
                AppSpecError::InvalidBindingTarget {
                    reason: TextReason::UnsafeText,
                    ..
                }
            ),
            "expected InvalidBindingTarget/UnsafeText, got {err:?}"
        );
    }

    #[test]
    fn binding_target_accepts_normal_entity_id() {
        let spec = AppSpec::validate(draft_with_binding_target("light.kitchen_lamp"), 100)
            .expect("normal entity id is valid");
        assert_eq!(spec.bindings()[0].target().as_str(), "light.kitchen_lamp");
    }

    // --- 11. UndeclaredBindingCapability -------------------------------

    // docs/06 §6: "undeclared capability ⇒ reject". The spec declares only
    // HomeReadState; a binding naming HomeSetLight (a real, but *undeclared*,
    // capability) must not slip through — this is the domain-level echo of
    // that bridge-time rule.
    #[test]
    fn binding_naming_an_undeclared_capability_is_rejected_per_docs06_undeclared_capability_rejects()
     {
        let mut draft = valid_draft();
        draft.bindings[0].capability = cap(Capability::HomeSetLight);
        let err = AppSpec::validate(draft, 100).unwrap_err();
        match err {
            AppSpecError::UndeclaredBindingCapability {
                binding,
                capability,
            } => {
                assert_eq!(binding, "kitchen_temp");
                assert_eq!(capability, Capability::HomeSetLight);
            }
            other => panic!("expected UndeclaredBindingCapability, got {other:?}"),
        }
    }

    // --- 12. limits ------------------------------------------------------

    #[test]
    fn limits_absent_uses_host_default() {
        let mut draft = valid_draft();
        draft.limits = None;
        let spec = AppSpec::validate(draft, 100).expect("valid");
        assert_eq!(spec.limits(), AppLimits::host_default());
    }

    #[test]
    fn limits_below_ceiling_are_accepted_verbatim() {
        let mut draft = valid_draft();
        draft.limits = Some(AppLimitsDraft {
            max_bundle_bytes: Some(1024),
            max_build_seconds: Some(5),
        });
        let spec = AppSpec::validate(draft, 100).expect("below ceiling is valid");
        assert_eq!(spec.limits().max_bundle_bytes(), 1024);
        assert_eq!(spec.limits().max_build_seconds(), 5);
    }

    #[test]
    fn limits_at_exactly_the_ceiling_are_accepted() {
        let mut draft = valid_draft();
        draft.limits = Some(AppLimitsDraft {
            max_bundle_bytes: Some(MAX_BUNDLE_BYTES),
            max_build_seconds: Some(MAX_BUILD_SECONDS),
        });
        let spec = AppSpec::validate(draft, 100).expect("at the ceiling is valid");
        assert_eq!(spec.limits().max_bundle_bytes(), MAX_BUNDLE_BYTES);
        assert_eq!(spec.limits().max_build_seconds(), MAX_BUILD_SECONDS);
    }

    #[test]
    fn limits_bundle_bytes_above_host_maximum_is_rejected_not_clamped() {
        let mut draft = valid_draft();
        draft.limits = Some(AppLimitsDraft {
            max_bundle_bytes: Some(MAX_BUNDLE_BYTES + 1),
            max_build_seconds: None,
        });
        let err = AppSpec::validate(draft, 100).unwrap_err();
        match err {
            AppSpecError::LimitAboveHostMaximum { requested, max, .. } => {
                assert_eq!(requested, MAX_BUNDLE_BYTES + 1);
                assert_eq!(max, MAX_BUNDLE_BYTES);
            }
            other => panic!("expected LimitAboveHostMaximum, got {other:?}"),
        }
    }

    #[test]
    fn limits_build_seconds_above_host_maximum_is_rejected_not_clamped() {
        let mut draft = valid_draft();
        draft.limits = Some(AppLimitsDraft {
            max_bundle_bytes: None,
            max_build_seconds: Some(MAX_BUILD_SECONDS + 1),
        });
        let err = AppSpec::validate(draft, 100).unwrap_err();
        match err {
            AppSpecError::LimitAboveHostMaximum { requested, max, .. } => {
                assert_eq!(requested, u64::from(MAX_BUILD_SECONDS + 1));
                assert_eq!(max, u64::from(MAX_BUILD_SECONDS));
            }
            other => panic!("expected LimitAboveHostMaximum, got {other:?}"),
        }
    }

    #[test]
    fn limits_bundle_bytes_zero_is_rejected() {
        let mut draft = valid_draft();
        draft.limits = Some(AppLimitsDraft {
            max_bundle_bytes: Some(0),
            max_build_seconds: None,
        });
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(matches!(err, AppSpecError::LimitIsZero { .. }));
    }

    #[test]
    fn limits_build_seconds_zero_is_rejected() {
        let mut draft = valid_draft();
        draft.limits = Some(AppLimitsDraft {
            max_bundle_bytes: None,
            max_build_seconds: Some(0),
        });
        let err = AppSpec::validate(draft, 100).unwrap_err();
        assert!(matches!(err, AppSpecError::LimitIsZero { .. }));
    }

    // --- 13. max_declared_risk -----------------------------------------

    #[test]
    fn max_declared_risk_is_the_highest_tier_among_declared_capabilities() {
        let mut draft = valid_draft();
        draft.capabilities = vec![
            cap(Capability::HomeReadState),
            cap(Capability::HomeExecuteScene),
        ];
        draft.bindings = vec![];
        let spec = AppSpec::validate(draft, 200).expect("valid");
        assert_eq!(
            spec.max_declared_risk().map(DeclaredRisk::for_display),
            Some(RiskLevel::R2)
        );
    }

    // --- 15. F6.1 review additions -------------------------------------

    #[test]
    fn template_all_covers_every_variant() {
        // The `match` below is the enforcement: a new variant fails to compile
        // until it is visited, and the assertion then fails until it is also
        // listed in `ALL`. `TemplateId::ALL` previously claimed a test it did
        // not have (F6.1 review S1).
        for template in TemplateId::ALL {
            match template {
                TemplateId::Dashboard => {
                    assert!(TemplateId::ALL.contains(&TemplateId::Dashboard));
                }
            }
        }
        assert_eq!(TemplateId::ALL.len(), 1);
        // Every listed template resolves through the same lookup `validate` uses.
        for template in TemplateId::ALL {
            let mut draft = valid_draft();
            draft.template = template.as_str().to_owned();
            assert_eq!(
                AppSpec::validate(draft, 200)
                    .expect("listed template validates")
                    .template(),
                template
            );
        }
    }

    /// U+2028 was accepted before the F6.1 review: `has_unsafe_text` covered a
    /// subset of category `Cf` while the markdown escaper — facing the same
    /// hostile source — folded these too. Both now share one predicate.
    #[test]
    fn title_rejects_line_separators_and_invisible_cf_carriers() {
        for evil in [
            "Kitchen\u{2028}Dashboard",
            "Kitchen\u{2029}Dashboard",
            // Unicode tag block: a full hidden ASCII alphabet (ASCII smuggling).
            "Kitchen\u{E0041}\u{E0042}",
            "Kit\u{00AD}chen",
            "Kit\u{FFF9}chen",
        ] {
            let mut draft = valid_draft();
            draft.title = evil.to_owned();
            assert!(
                matches!(
                    AppSpec::validate(draft, 200),
                    Err(AppSpecError::InvalidTitle {
                        reason: TextReason::UnsafeText,
                        ..
                    })
                ),
                "invisible character must not survive into a title: {evil:?}"
            );
        }
    }

    /// A title reaches generated HTML/JS (F6.2). The builder escapes, but the
    /// type refuses syntax outright — `BindingName` already does, and a title
    /// with no equivalent guarantee was the asymmetry the review flagged.
    #[test]
    fn title_rejects_html_and_js_syntax_characters() {
        for evil in [
            "<img src=x onerror=alert(1)>",
            "`${process.env.SECRET}`",
            "a\\\";alert(1);//",
            "{{constructor}}",
        ] {
            let mut draft = valid_draft();
            draft.title = evil.to_owned();
            assert!(
                matches!(
                    AppSpec::validate(draft, 200),
                    Err(AppSpecError::InvalidTitle {
                        reason: TextReason::BadCharacter,
                        ..
                    })
                ),
                "syntax character must not survive into a title: {evil:?}"
            );
        }
        // Ordinary titles are untouched.
        for ok in ["Kitchen Dashboard", "Lights & Heating", "Café (2nd floor)"] {
            let mut draft = valid_draft();
            draft.title = ok.to_owned();
            assert_eq!(
                AppSpec::validate(draft, 200)
                    .expect("ordinary title")
                    .title(),
                ok
            );
        }
    }

    /// A binding target is interpolated into generated source too, so it has a
    /// positive charset wide enough for every address the vocabulary can reach
    /// and narrow enough to carry no markup, quotes or traversal.
    #[test]
    fn binding_target_rejects_markup_quotes_and_traversal() {
        for evil in [
            "</script><script>alert(1)</script>",
            "\" onload=\"x",
            "../../etc/passwd",
            "a b c",
        ] {
            assert!(
                matches!(
                    AppSpec::validate(draft_with_binding_target(evil), 300),
                    Err(AppSpecError::InvalidBindingTarget {
                        reason: TextReason::BadCharacter,
                        ..
                    })
                ),
                "target must not carry syntax: {evil:?}"
            );
        }
        // Everything the vocabulary actually addresses still passes.
        for ok in [
            "sensor.kitchen_temperature",
            "light.kitchen_lamp",
            "scene.movie_night",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "media_player.living-room",
        ] {
            assert_eq!(
                AppSpec::validate(draft_with_binding_target(ok), 300)
                    .expect("addressable target")
                    .bindings()[0]
                    .target()
                    .as_str(),
                ok
            );
        }
    }

    /// Previously untested arm: a binding naming a capability outside the
    /// vocabulary. It gets its own variant so the offending binding is named —
    /// the top-level `UnknownCapability` has no binding to report.
    #[test]
    fn binding_naming_a_capability_outside_the_vocabulary_names_the_binding() {
        let mut draft = valid_draft();
        draft.bindings[0].capability = "shell.exec".to_owned();
        match AppSpec::validate(draft, 200).unwrap_err() {
            AppSpecError::UnknownBindingCapability {
                binding,
                capability,
            } => {
                assert_eq!(binding, "kitchen_temp");
                assert_eq!(capability, "shell.exec");
            }
            other => panic!("expected UnknownBindingCapability, got {other:?}"),
        }
    }

    /// The echoed value is exposed **raw** (it is the payload of these
    /// variants), so a preserved newline would let a rejected name forge a
    /// second log or audit line. Asserting on `to_string()` alone could not see
    /// this: `{0:?}` escapes newlines on the way out.
    #[test]
    fn echoed_untrusted_text_carries_no_newline_or_tab_in_the_raw_field() {
        let mut draft = valid_draft();
        draft.title = "Kitchen\nFORGED: policy=allow\tx".to_owned();
        match AppSpec::validate(draft, 200).unwrap_err() {
            AppSpecError::InvalidTitle { value, .. } => {
                assert!(!value.contains('\n'), "raw newline leaked: {value:?}");
                assert!(!value.contains('\t'), "raw tab leaked: {value:?}");
            }
            other => panic!("expected InvalidTitle, got {other:?}"),
        }

        let err = "home.read_state\nFORGED"
            .parse::<Capability>()
            .expect_err("a name with a newline is not a capability");
        assert!(
            !err.rejected().contains('\n'),
            "raw newline leaked from CapabilityError"
        );
    }

    // --- 14. untrusted echo hygiene (invariant #5 / docs/06 §5) ---------

    #[test]
    fn rejected_unknown_capability_echoes_untrusted_text_clamped_and_stripped() {
        let evil = format!("\u{202E}evil\u{0007}{}", "x".repeat(500));
        let mut draft = valid_draft();
        draft.capabilities = vec![evil];
        draft.bindings = vec![];
        let err = AppSpec::validate(draft, 700).unwrap_err();
        assert!(matches!(err, AppSpecError::UnknownCapability(_)));
        let rendered = err.to_string();
        assert!(
            !rendered.chars().any(|c| c.is_control()),
            "control character leaked into error text: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{202E}'),
            "bidi override leaked into error text: {rendered:?}"
        );
        assert!(
            rendered.len() < 150,
            "500 bytes of padding must be clamped, got {} bytes: {rendered:?}",
            rendered.len()
        );
    }

    #[test]
    fn rejected_unknown_template_echoes_untrusted_text_clamped_and_stripped() {
        let evil = format!("\u{202E}evil\u{0007}{}", "x".repeat(500));
        let mut draft = valid_draft();
        draft.template = evil;
        let err = AppSpec::validate(draft, 700).unwrap_err();
        assert!(matches!(err, AppSpecError::UnknownTemplate(_)));
        let rendered = err.to_string();
        assert!(
            !rendered.chars().any(|c| c.is_control()),
            "control character leaked into error text: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{202E}'),
            "bidi override leaked into error text: {rendered:?}"
        );
        assert!(
            rendered.len() < 150,
            "500 bytes of padding must be clamped, got {} bytes: {rendered:?}",
            rendered.len()
        );
    }
}
