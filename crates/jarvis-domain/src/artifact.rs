//! Artifact manifests: durable, versioned outputs with immutable provenance
//! (FR-08, docs/02 §6, docs/04 §4, ADR-008).
//!
//! An artifact is a blob in the content-addressed store (keyed by its
//! [`Sha256`]) plus an **immutable manifest** describing it: id, version,
//! creating run, hash, media type, renderer, sources, sensitivity, build
//! provenance, and declared capabilities. This module is pure — it defines the
//! manifest's shape and the versioning rule (a new version is a *new* manifest,
//! never a mutation). The CAS blob store, Postgres persistence, and the
//! transactional audit write live in infra (F3a.2); the wire DTO lives in
//! contracts (F3a.3).
//!
//! Immutability (docs/04 §4 "Manifests are immutable; a new version is a new
//! row + new CAS entry") is enforced structurally: a manifest exposes no
//! mutating method, and [`ArtifactManifest::next_version`] borrows `&self` and
//! returns a brand-new manifest — the prior version is never touched.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use thiserror::Error;

use crate::grants::Sha256;
use crate::ids::{ArtifactId, MessageId, RunId};
// TODO(promote): `Sensitivity` currently lives in `location` (its first user).
// It is now cross-cutting (location + artifact); when a third consumer appears,
// promote it to a neutral module (e.g. `sensitivity.rs`) rather than importing
// it from `location` here.
use crate::location::Sensitivity;

/// A monotonic artifact version, starting at 1 (docs/04 §4 `"version": 3`).
/// A `NonZeroU32` so "version 0" is unrepresentable — the first version is
/// [`ArtifactVersion::FIRST`] and each subsequent version is exactly one more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactVersion(NonZeroU32);

impl ArtifactVersion {
    /// The first version of any artifact.
    pub const FIRST: ArtifactVersion = ArtifactVersion(NonZeroU32::MIN);

    /// Construct from a raw version number; `0` is rejected (`None`).
    pub fn new(n: u32) -> Option<ArtifactVersion> {
        NonZeroU32::new(n).map(ArtifactVersion)
    }

    /// The next version. Returns `None` only on `u32` overflow — an artifact
    /// with four billion versions is not a real condition, but the domain never
    /// silently wraps.
    pub fn next(self) -> Option<ArtifactVersion> {
        self.0.checked_add(1).map(ArtifactVersion)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl fmt::Display for ArtifactVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.get())
    }
}

/// The logical kind of an artifact, which selects its renderer (docs/02 §6:
/// "Initial renderers: Markdown/HTML, code/text, images, simple charts").
/// Exhaustive on purpose — a new artifact shape adds a variant here and a
/// renderer in the web shell (F3b.3), never a free-form string that the client
/// must guess how to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// Markdown or sanitized HTML — Research Notes, reports (FR-27).
    MarkdownHtml,
    /// Plain code or text, including unified diffs / patches (golden 7, F3a.6).
    CodeText,
    /// A raster or vector image.
    Image,
    /// A simple chart rendered from structured data.
    Chart,
    /// A generated-web-app bundle. **Reserved for M6** (FR-18) — executed only
    /// in the sandbox (docs/06 §6); no renderer ships in M3.
    Bundle,
}

impl ArtifactKind {
    /// The stable, versioned renderer id recorded in the manifest (docs/04 §4
    /// `"renderer": "sandboxed-webapp/v1"`). In v1 each kind maps to exactly one
    /// renderer; the id is versioned independently so a renderer can evolve
    /// without changing the kind.
    pub fn renderer_id(self) -> &'static str {
        match self {
            ArtifactKind::MarkdownHtml => "markdown-html/v1",
            ArtifactKind::CodeText => "code-text/v1",
            ArtifactKind::Image => "image/v1",
            ArtifactKind::Chart => "chart/v1",
            ArtifactKind::Bundle => "sandboxed-webapp/v1",
        }
    }

    /// Whether this kind may be rendered **inside the control UI's own origin**
    /// — the shell parses the bytes itself and puts the result on the page.
    ///
    /// (Was `is_renderable_in_m3` through M3–M5, when `Bundle` had no renderer
    /// at all. The milestone number was never the point: what this predicate
    /// actually decides is *which origin* is allowed to hold the bytes, and
    /// F6.4 makes the second answer real.)
    pub fn renders_inline_in_shell(self) -> bool {
        match self {
            ArtifactKind::MarkdownHtml
            | ArtifactKind::CodeText
            | ArtifactKind::Image
            | ArtifactKind::Chart => true,
            // Never. A bundle is model-influenced code; the control origin is
            // exactly where it must not run (docs/06 §6, ADR-030).
            ArtifactKind::Bundle => false,
        }
    }

    /// Whether this kind is rendered through the **generated-app sandbox** — an
    /// opaque-origin, script-sandboxed frame under a restrictive CSP (F6.4,
    /// ADR-030). Exactly the complement of [`Self::renders_inline_in_shell`],
    /// asserted by a test: every kind has exactly one render path, and a kind
    /// that fell out of both would silently become unrenderable while a kind in
    /// both would be a same-origin escape.
    pub fn renders_in_app_sandbox(self) -> bool {
        match self {
            ArtifactKind::Bundle => true,
            ArtifactKind::MarkdownHtml
            | ArtifactKind::CodeText
            | ArtifactKind::Image
            | ArtifactKind::Chart => false,
        }
    }
}

/// A validated media (MIME) type for the blob, e.g. `text/markdown` or
/// `application/vnd.jarvis.webapp+zip` (docs/04 §4 `mediaType`). Non-empty and
/// contains a `/`; the domain does not enforce the full RFC 6838 grammar — that
/// is presentation, not a security boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaType(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("media type must be non-empty and contain a '/' (got {0:?})")]
pub struct MediaTypeError(String);

impl MediaType {
    /// `text/markdown` — the media type of every promoted document (a Research
    /// Notes thread, FR-27/ADR-017; a promoted list, FR-34/ADR-024).
    ///
    /// A named constructor rather than a `&str` each caller re-parses: parsing a
    /// literal that is known-good forces every promotion path to carry an
    /// `.expect()` for a case that cannot occur, and `.expect()` in a library
    /// crate is a lint the project does not want to keep explaining. The value
    /// is host-authored and never derived from anything untrusted, which is the
    /// property that actually matters here.
    ///
    /// `MediaType` wraps a `String`, so this cannot be a `const`; the round-trip
    /// against [`FromStr`] is asserted by a test instead.
    pub fn markdown() -> MediaType {
        MediaType("text/markdown".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for MediaType {
    type Err = MediaTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        // Must be non-empty, contain a subtype separator, and hold no control
        // characters — a media type becomes an HTTP `Content-Type` header value
        // at the artifact-blob boundary (F3a.3), and a control char there would
        // fail header construction (defence in depth, docs/06 §2 strip-controls).
        if trimmed.is_empty() || !trimmed.contains('/') || trimmed.chars().any(|c| c.is_control()) {
            return Err(MediaTypeError(s.to_owned()));
        }
        Ok(MediaType(trimmed.to_owned()))
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an artifact's content came from (docs/04 §4 `sources`). Typed rather
/// than a free `{type, id}` map so provenance is checkable: a Research Notes
/// artifact (FR-27) cites the messages and web pages it was built from, and a
/// promotion never loses which run or page a fact came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactSource {
    /// A conversation message that contributed content.
    Message(MessageId),
    /// A run whose output contributed content.
    Run(RunId),
    /// A fetched web page (F2.8 `source_url`) — carried so an image or fact
    /// keeps its attribution link (FR-25/ADR-014).
    Web { url: String },
}

/// How the artifact was built (docs/04 §4 `build`). For a coding-worker patch
/// (F3a.6) or a generated-app bundle (M6) this pins the worker image, the
/// dependency lockfile hash, and the network policy in force during the build,
/// so a build is reproducible and auditable. Trivially-produced artifacts
/// (Research Notes, plain text) use [`BuildProvenance::none`].
///
/// Never carries secrets (invariant 5) — only hashes and image references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildProvenance {
    /// The builder image reference, e.g. `jarvis-web-builder@sha256:…`.
    pub worker_image: Option<String>,
    /// Hash of the dependency lockfile the build resolved against.
    pub lockfile_hash: Option<Sha256>,
    /// The network policy enforced during the build.
    pub network: BuildNetwork,
}

impl BuildProvenance {
    /// Provenance for an artifact produced without an isolated builder (e.g. a
    /// Research Notes markdown document): no worker image, no lockfile, network
    /// disabled.
    pub fn none() -> BuildProvenance {
        BuildProvenance {
            worker_image: None,
            lockfile_hash: None,
            network: BuildNetwork::Disabled,
        }
    }
}

/// The network policy under which an artifact was built (docs/04 §4
/// `build.network`). Exhaustive; the default and safest is
/// [`BuildNetwork::Disabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildNetwork {
    /// No network access during the build (the default for sandboxed builders).
    Disabled,
    /// Network access was available during the build.
    Enabled,
}

/// A capability an artifact declares it needs to run (docs/04 §4
/// `capabilities`). For a generated app (FR-18) this is the *only* vocabulary
/// the F6.5 bridge will honour: a `postMessage` from a bundle may name one of
/// these and nothing else.
///
/// **Closed on purpose (F6.1).** Through M3–M5 this was a free-form `String`
/// newtype, which was harmless while it was pure provenance metadata. It is not
/// harmless once a bridge enforces it: a bridge that enforces free-form strings
/// enforces nothing, because "undeclared capability ⇒ reject" (docs/06 §6) is
/// only a decidable question against a host-defined, exhaustive set. Adding a
/// capability is therefore a deliberate host change — a new variant here, a
/// registered tool to back it, and a risk tier — never a string a model can
/// invent.
///
/// Each variant names an **already-registered** tool. Naming it grants nothing
/// (invariant #1): the bridge still runs `policy::evaluate` against the live
/// registry at call time, and [`Capability::risk`] is only the *declared* tier
/// used for spec-time preview and approval text. The authoritative tier is the
/// registered [`crate::policy::ToolPolicy`]; a test in `jarvisd` asserts the two
/// never diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// Read Home Assistant entity state — backed by `home.get_state` (R0).
    HomeReadState,
    /// Set a single allowlisted light — backed by `home.set_light` (R1).
    HomeSetLight,
    /// Activate an allowlisted scene — backed by `home.execute_scene` (R2, so
    /// it takes the full approval + `ExecutionGrant` path).
    HomeExecuteScene,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown capability {0:?} (the host vocabulary is closed; see Capability::ALL)")]
pub struct CapabilityError(String);

impl CapabilityError {
    /// The rejected name, already length-clamped and control-stripped by
    /// [`FromStr`] — this string is model-authored and travels into error
    /// bodies, spans and audit reasons, so it never carries raw untrusted text
    /// (docs/06 §5).
    pub fn rejected(&self) -> &str {
        &self.0
    }
}

impl Capability {
    /// Every capability the host defines.
    ///
    /// What actually keeps this honest is the `match` in [`Capability::as_str`]
    /// and its siblings: adding a variant is a compile error until every one of
    /// them is visited, and this list sits directly beside them. That is
    /// weaker than a proof — a new variant could be given an `as_str` and left
    /// out of `ALL`, and it would then be silently unnameable in a spec (fail
    /// closed, but silent). The F6.1 review named this precisely; the real fix
    /// is generating enum + list + tables from one source, which is a bigger
    /// change than this milestone should make. Until then: **add the variant
    /// here in the same edit.**
    pub const ALL: [Capability; 3] = [
        Capability::HomeReadState,
        Capability::HomeSetLight,
        Capability::HomeExecuteScene,
    ];

    /// The stable wire name (docs/04 §4 `capabilities`).
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::HomeReadState => "home.read_state",
            Capability::HomeSetLight => "home.set_light",
            Capability::HomeExecuteScene => "home.execute_scene",
        }
    }

    /// The registered tool that backs this capability. The bridge resolves the
    /// capability to exactly this id and never to anything a message supplies.
    pub fn tool_id(self) -> crate::tools::ToolId {
        match self {
            Capability::HomeReadState => crate::tools::ToolId::home_get_state(),
            Capability::HomeSetLight => crate::tools::ToolId::home_set_light(),
            Capability::HomeExecuteScene => crate::tools::ToolId::home_execute_scene(),
        }
    }

    /// The **declared** risk tier, used for spec-time preview and approval text.
    /// Never a substitute for `policy::evaluate` on the live registry.
    pub fn risk(self) -> crate::policy::RiskLevel {
        match self {
            Capability::HomeReadState => crate::policy::RiskLevel::R0,
            Capability::HomeSetLight => crate::policy::RiskLevel::R1,
            Capability::HomeExecuteScene => crate::policy::RiskLevel::R2,
        }
    }
}

/// The longest rejected-name echoed back in an error.
const MAX_ECHOED_NAME_BYTES: usize = 64;

/// Clamp and strip an untrusted **name** before it is echoed in an error.
/// Model- or app-authored text reaches logs, problem bodies and audit reasons,
/// so every character [`is_unsafe_in_single_line`](crate::tools) covers is
/// removed and the length is capped: a rejection can be used neither to smuggle
/// nor to flood.
///
/// Deliberately **not** [`crate::tools::sanitize_result_content`], which keeps
/// `\n` and `\t` because it was written for tool-result *prose*. A capability or
/// template name has no legitimate newline, and this value is exposed raw
/// (`CapabilityError::rejected`, the payload of several `AppSpecError`
/// variants), so a preserved newline would let a rejected name forge a second
/// log or audit line. The F6.1 security review found exactly that gap.
///
/// Stripping happens before the byte count, so the result is `≤
/// MAX_ECHOED_NAME_BYTES` however much invisible padding the input carried.
pub(crate) fn echo_untrusted(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if crate::tools::is_unsafe_in_single_line(ch) {
            continue;
        }
        if out.len() + ch.len_utf8() > MAX_ECHOED_NAME_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

impl FromStr for Capability {
    type Err = CapabilityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        Capability::ALL
            .into_iter()
            .find(|c| c.as_str() == trimmed)
            .ok_or_else(|| CapabilityError(echo_untrusted(s)))
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The content of one artifact version, independent of its version number.
/// Bundling these lets [`ArtifactManifest::next_version`] take exactly the
/// fields that change between versions while the id stays fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContent {
    pub sha256: Sha256,
    pub media_type: MediaType,
    pub kind: ArtifactKind,
    pub sources: Vec<ArtifactSource>,
    pub sensitivity: Sensitivity,
    pub build: BuildProvenance,
    pub capabilities: Vec<Capability>,
}

/// The immutable manifest of one artifact version (docs/02 §6, docs/04 §4,
/// FR-08). Every field is provenance: once created a manifest is never mutated
/// — a new version is a whole new manifest (see [`ArtifactManifest::next_version`]),
/// which is why persistence is append-only (F3a.2) and the audit trail is
/// intact (invariant 6).
///
/// Fields are private with getters so "immutable once created" is enforced by
/// the type system, not merely by convention — there is no way to reassign a
/// manifest's version, hash, or sources after construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactManifest {
    id: ArtifactId,
    version: ArtifactVersion,
    created_by_run: RunId,
    content: ArtifactContent,
}

impl ArtifactManifest {
    /// Create the first version (v1) of a new artifact.
    pub fn initial(
        id: ArtifactId,
        created_by_run: RunId,
        content: ArtifactContent,
    ) -> ArtifactManifest {
        ArtifactManifest {
            id,
            version: ArtifactVersion::FIRST,
            created_by_run,
            content,
        }
    }

    /// Reconstruct a manifest from persisted parts (used by infra when loading
    /// a stored version, F3a.2). The domain does not police whether this exact
    /// (id, version) was previously stored — that is the store's uniqueness
    /// invariant; here it only carries the value.
    pub fn from_parts(
        id: ArtifactId,
        version: ArtifactVersion,
        created_by_run: RunId,
        content: ArtifactContent,
    ) -> ArtifactManifest {
        ArtifactManifest {
            id,
            version,
            created_by_run,
            content,
        }
    }

    /// Produce the next version of this artifact. Borrows `&self` (never `&mut`)
    /// and returns a *new* manifest with the same [`ArtifactId`], the version
    /// incremented by one, and the supplied new content — the prior manifest is
    /// left untouched (docs/04 §4). Returns `None` only on version overflow.
    pub fn next_version(
        &self,
        created_by_run: RunId,
        content: ArtifactContent,
    ) -> Option<ArtifactManifest> {
        Some(ArtifactManifest {
            id: self.id.clone(),
            version: self.version.next()?,
            created_by_run,
            content,
        })
    }

    pub fn id(&self) -> &ArtifactId {
        &self.id
    }

    pub fn version(&self) -> ArtifactVersion {
        self.version
    }

    /// The run that created *this version* (not necessarily the run that created
    /// version 1).
    pub fn created_by_run(&self) -> &RunId {
        &self.created_by_run
    }

    pub fn sha256(&self) -> &Sha256 {
        &self.content.sha256
    }

    pub fn media_type(&self) -> &MediaType {
        &self.content.media_type
    }

    pub fn kind(&self) -> ArtifactKind {
        self.content.kind
    }

    /// The versioned renderer id for this artifact's kind (docs/04 §4).
    pub fn renderer_id(&self) -> &'static str {
        self.content.kind.renderer_id()
    }

    pub fn sources(&self) -> &[ArtifactSource] {
        &self.content.sources
    }

    pub fn sensitivity(&self) -> Sensitivity {
        self.content.sensitivity
    }

    pub fn build(&self) -> &BuildProvenance {
        &self.content.build
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.content.capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_id() -> RunId {
        "01J8Z000000000000000000000".parse().unwrap()
    }

    fn other_run_id() -> RunId {
        "01J8Z0000000000000000000ZZ".parse().unwrap()
    }

    fn artifact_id() -> ArtifactId {
        "01J8ZARTFACT00000000000000".parse().unwrap()
    }

    fn sha(byte: u8) -> Sha256 {
        Sha256::from_bytes([byte; 32])
    }

    fn markdown_content(sha_byte: u8) -> ArtifactContent {
        ArtifactContent {
            sha256: sha(sha_byte),
            media_type: "text/markdown".parse().unwrap(),
            kind: ArtifactKind::MarkdownHtml,
            sources: vec![ArtifactSource::Run(run_id())],
            sensitivity: Sensitivity::Normal,
            build: BuildProvenance::none(),
            capabilities: vec![],
        }
    }

    // --- ArtifactVersion --------------------------------------------------

    #[test]
    fn first_version_is_one_and_zero_is_unrepresentable() {
        assert_eq!(ArtifactVersion::FIRST.get(), 1);
        assert_eq!(ArtifactVersion::new(0), None);
        assert_eq!(ArtifactVersion::new(3).unwrap().get(), 3);
    }

    #[test]
    fn version_increments_by_exactly_one() {
        let v1 = ArtifactVersion::FIRST;
        let v2 = v1.next().unwrap();
        let v3 = v2.next().unwrap();
        assert_eq!(v2.get(), 2);
        assert_eq!(v3.get(), 3);
        assert!(v1 < v2 && v2 < v3);
    }

    #[test]
    fn version_overflow_is_none_not_wrap() {
        let max = ArtifactVersion::new(u32::MAX).unwrap();
        assert_eq!(max.next(), None);
    }

    // --- immutability: the headline property (docs/04 §4) -----------------

    #[test]
    fn next_version_produces_a_new_manifest_and_leaves_the_old_untouched() {
        let v1 = ArtifactManifest::initial(artifact_id(), run_id(), markdown_content(0xAA));
        let v1_snapshot = v1.clone();

        let v2 = v1
            .next_version(other_run_id(), markdown_content(0xBB))
            .unwrap();

        // Same artifact identity, incremented version.
        assert_eq!(v2.id(), v1.id());
        assert_eq!(v1.version().get(), 1);
        assert_eq!(v2.version().get(), 2);

        // New content on v2; v1 is byte-for-byte what it was before.
        assert_eq!(v2.sha256(), &sha(0xBB));
        assert_eq!(v2.created_by_run(), &other_run_id());
        assert_eq!(
            v1, v1_snapshot,
            "next_version must not mutate the prior manifest"
        );
        assert_eq!(v1.sha256(), &sha(0xAA));
    }

    #[test]
    fn next_version_reports_overflow_rather_than_wrapping() {
        let maxed = ArtifactManifest::from_parts(
            artifact_id(),
            ArtifactVersion::new(u32::MAX).unwrap(),
            run_id(),
            markdown_content(0x01),
        );
        assert!(
            maxed
                .next_version(run_id(), markdown_content(0x02))
                .is_none()
        );
    }

    // --- kind / renderer mapping (docs/02 §6, docs/04 §4) -----------------

    #[test]
    fn renderer_ids_are_stable_and_versioned() {
        assert_eq!(ArtifactKind::MarkdownHtml.renderer_id(), "markdown-html/v1");
        assert_eq!(ArtifactKind::CodeText.renderer_id(), "code-text/v1");
        assert_eq!(ArtifactKind::Image.renderer_id(), "image/v1");
        assert_eq!(ArtifactKind::Chart.renderer_id(), "chart/v1");
        assert_eq!(ArtifactKind::Bundle.renderer_id(), "sandboxed-webapp/v1");
    }

    /// A bundle never renders in the control origin, and everything else never
    /// renders through the app sandbox — every kind has **exactly one** render
    /// path (F6.4, ADR-030). A kind in neither would be silently unrenderable;
    /// a kind in both would be a same-origin escape.
    #[test]
    fn every_kind_has_exactly_one_render_path() {
        assert!(!ArtifactKind::Bundle.renders_inline_in_shell());
        assert!(ArtifactKind::Bundle.renders_in_app_sandbox());
        for kind in [
            ArtifactKind::MarkdownHtml,
            ArtifactKind::CodeText,
            ArtifactKind::Image,
            ArtifactKind::Chart,
            ArtifactKind::Bundle,
        ] {
            assert_ne!(
                kind.renders_inline_in_shell(),
                kind.renders_in_app_sandbox(),
                "{kind:?} must have exactly one render path"
            );
        }
    }

    // --- MediaType validation ---------------------------------------------

    #[test]
    fn media_type_requires_a_slash_and_rejects_empty() {
        assert!("text/markdown".parse::<MediaType>().is_ok());
        assert_eq!(
            "application/vnd.jarvis.webapp+zip"
                .parse::<MediaType>()
                .unwrap()
                .as_str(),
            "application/vnd.jarvis.webapp+zip"
        );
        assert!("".parse::<MediaType>().is_err());
        assert!("notamediatype".parse::<MediaType>().is_err());
        // Control characters are rejected (they would break a Content-Type header).
        assert!("text/pl\nain".parse::<MediaType>().is_err());
        assert!("text/plain\r\nX-Evil: 1".parse::<MediaType>().is_err());
        // Trimmed on the way in.
        assert_eq!(
            "  text/plain  ".parse::<MediaType>().unwrap().as_str(),
            "text/plain"
        );
    }

    #[test]
    fn the_markdown_constant_is_exactly_what_parsing_the_literal_yields() {
        // This is what lets every promotion path drop its `.expect()`: the
        // named constructor and the validated parse agree, asserted once here
        // rather than re-proved by a panic at each call site.
        assert_eq!(MediaType::markdown(), "text/markdown".parse().unwrap());
        assert_eq!(MediaType::markdown().as_str(), "text/markdown");
    }

    // --- Capability validation (closed enum, F6.1) -------------------------
    //
    // Through M3-M5 `Capability` was a free-form `String` newtype; F6.1 closed
    // it to a fixed vocabulary (docs/06 §6 "undeclared capability ⇒ reject" is
    // only decidable against a closed set). These tests replace the old
    // `capability_rejects_empty` free-form test.

    // A new variant added to the enum without extending this match fails to
    // *compile* — that is the exhaustiveness guarantee, independent of (and
    // stronger than) counting `Capability::ALL`'s length.
    #[test]
    fn capability_all_covers_every_variant_exhaustive() {
        fn assert_listed_in_all(c: Capability) {
            assert!(
                Capability::ALL.contains(&c),
                "{c:?} is a real variant but missing from Capability::ALL"
            );
        }
        for c in Capability::ALL {
            match c {
                Capability::HomeReadState => assert_listed_in_all(Capability::HomeReadState),
                Capability::HomeSetLight => assert_listed_in_all(Capability::HomeSetLight),
                Capability::HomeExecuteScene => assert_listed_in_all(Capability::HomeExecuteScene),
            }
        }
        assert_eq!(
            Capability::ALL.len(),
            3,
            "ALL must list exactly the enum's variants"
        );
    }

    #[test]
    fn capability_as_str_round_trips_through_from_str_for_every_variant() {
        for c in Capability::ALL {
            let s = c.as_str();
            let parsed: Capability = s.parse().expect("known capability string parses");
            assert_eq!(parsed, c);
        }
    }

    #[test]
    fn capability_from_str_rejects_unknown() {
        assert!("not.a.capability".parse::<Capability>().is_err());
        // The pre-F6.1 free-form vocabulary is no longer valid: closing the
        // enum must reject strings that used to be accepted.
        assert!(
            "artifact.read-own-data".parse::<Capability>().is_err(),
            "the old free-form vocabulary must now be rejected"
        );
    }

    #[test]
    fn capability_from_str_rejects_empty_and_whitespace() {
        assert!("".parse::<Capability>().is_err());
        assert!("   ".parse::<Capability>().is_err());
    }

    #[test]
    fn capability_tool_id_matches_the_registered_backing_tool() {
        assert_eq!(
            Capability::HomeReadState.tool_id(),
            "home.get_state".parse().unwrap()
        );
        assert_eq!(
            Capability::HomeSetLight.tool_id(),
            "home.set_light".parse().unwrap()
        );
        assert_eq!(
            Capability::HomeExecuteScene.tool_id(),
            "home.execute_scene".parse().unwrap()
        );
    }

    #[test]
    fn capability_risk_matches_the_documented_tier() {
        assert_eq!(
            Capability::HomeReadState.risk(),
            crate::policy::RiskLevel::R0
        );
        assert_eq!(
            Capability::HomeSetLight.risk(),
            crate::policy::RiskLevel::R1
        );
        assert_eq!(
            Capability::HomeExecuteScene.risk(),
            crate::policy::RiskLevel::R2
        );
    }

    // --- getters carry every manifest field (FR-08 provenance) ------------

    #[test]
    fn manifest_carries_full_provenance() {
        let content = ArtifactContent {
            sha256: sha(0x11),
            media_type: "text/markdown".parse().unwrap(),
            kind: ArtifactKind::MarkdownHtml,
            sources: vec![
                ArtifactSource::Message("01J8ZMSG00000000000000000A".parse().unwrap()),
                ArtifactSource::Web {
                    url: "https://en.wikipedia.org/wiki/Mitochondrion".to_owned(),
                },
            ],
            sensitivity: Sensitivity::Sensitive,
            build: BuildProvenance {
                worker_image: Some("jarvis-web-builder@sha256:abcd".to_owned()),
                lockfile_hash: Some(sha(0x22)),
                network: BuildNetwork::Disabled,
            },
            capabilities: vec!["home.read_state".parse().unwrap()],
        };
        let m = ArtifactManifest::initial(artifact_id(), run_id(), content);

        assert_eq!(m.id(), &artifact_id());
        assert_eq!(m.version(), ArtifactVersion::FIRST);
        assert_eq!(m.created_by_run(), &run_id());
        assert_eq!(m.sha256(), &sha(0x11));
        assert_eq!(m.media_type().as_str(), "text/markdown");
        assert_eq!(m.kind(), ArtifactKind::MarkdownHtml);
        assert_eq!(m.renderer_id(), "markdown-html/v1");
        assert_eq!(m.sources().len(), 2);
        assert_eq!(m.sensitivity(), Sensitivity::Sensitive);
        assert_eq!(m.build().network, BuildNetwork::Disabled);
        assert_eq!(m.capabilities().len(), 1);
    }
}
