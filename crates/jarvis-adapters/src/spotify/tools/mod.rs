//! One file per tool (F9.5); shared argument-parsing and grant helpers here.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use jarvis_application::policy::ToolDescriptor;
use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::media::VolumePct;
use jarvis_domain::tools::{
    CanonicalValue, MAX_RESULT_PROMPT_BYTES, ToolError, ToolInvocation, ToolResult, canonical_form,
    sanitize_result_content,
};
use sha2::{Digest, Sha256 as Sha2};

use super::*;

mod play;
mod play_playlist;
mod queue_add;
mod search;
mod volume;
mod volume_boost;

pub use play::*;
pub use play_playlist::*;
pub use queue_add::*;
pub use search::*;
pub use volume::*;
pub use volume_boost::*;

// ---------------------------------------------------------------------------
// Argument parsing shared by the tools
// ---------------------------------------------------------------------------

pub(crate) fn object(
    arguments: &CanonicalValue,
) -> Result<&BTreeMap<String, CanonicalValue>, ToolError> {
    match arguments {
        CanonicalValue::Object(map) => Ok(map),
        _ => Err(ToolError::SchemaInvalid(
            "arguments must be an object".to_owned(),
        )),
    }
}

pub(crate) fn optional_text(
    map: &BTreeMap<String, CanonicalValue>,
    key: &str,
) -> Result<Option<String>, ToolError> {
    match map.get(key) {
        Some(CanonicalValue::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if trimmed.len() > MAX_QUERY_BYTES || trimmed.chars().any(char::is_control) {
                return Err(ToolError::SchemaInvalid(format!(
                    "argument `{key}` is malformed or too long"
                )));
            }
            Ok(Some(trimmed.to_owned()))
        }
        Some(CanonicalValue::Null) | None => Ok(None),
        Some(_) => Err(ToolError::SchemaInvalid(format!(
            "argument `{key}` must be a string"
        ))),
    }
}

pub(crate) fn required_text(
    map: &BTreeMap<String, CanonicalValue>,
    key: &str,
) -> Result<String, ToolError> {
    optional_text(map, key)?
        .ok_or_else(|| ToolError::SchemaInvalid(format!("missing required argument `{key}`")))
}

pub(crate) fn optional_int(
    map: &BTreeMap<String, CanonicalValue>,
    key: &str,
) -> Result<Option<i64>, ToolError> {
    match map.get(key) {
        Some(CanonicalValue::Int(n)) => Ok(Some(*n)),
        Some(CanonicalValue::Null) | None => Ok(None),
        Some(_) => Err(ToolError::SchemaInvalid(format!(
            "argument `{key}` must be an integer"
        ))),
    }
}

/// `uri` or `query`, exactly one, plus an optional Connect `device`.
pub(crate) struct TargetArgs {
    pub(crate) uri: Option<String>,
    pub(crate) query: Option<String>,
    pub(crate) device: Option<String>,
}

impl TargetArgs {
    pub(crate) fn parse(arguments: &CanonicalValue) -> Result<Self, ToolError> {
        let map = object(arguments)?;
        let uri = optional_text(map, "uri")?;
        let query = optional_text(map, "query")?;
        let device = optional_text(map, "device")?;
        match (&uri, &query) {
            (Some(_), Some(_)) => Err(ToolError::SchemaInvalid(
                "pass either `uri` or `query`, not both".to_owned(),
            )),
            (None, None) => Err(ToolError::SchemaInvalid(
                "one of `uri` or `query` is required".to_owned(),
            )),
            _ => {
                if let Some(raw) = &uri
                    && parse_uri(raw).is_none()
                {
                    return Err(ToolError::SchemaInvalid(
                        "`uri` must be a spotify:track|album|artist|playlist:<id> URI".to_owned(),
                    ));
                }
                // `device` may be a room alias, a Connect device name, or an id,
                // so the charset stays open (a speaker really is called
                // "Kitchen Sonos"); it is bounded, and `resolve_device` is what
                // decides whether it names anything real.
                if let Some(name) = &device
                    && name.len() > MAX_DEVICE_ID_BYTES
                {
                    return Err(ToolError::SchemaInvalid(
                        "`device` is too long to be a device name or id".to_owned(),
                    ));
                }
                Ok(Self { uri, query, device })
            }
        }
    }
}

/// The one place the configured volume cap is applied. Called before any
/// transport work happens, by every path that can carry a level — so a denied
/// level produces **zero** Spotify calls, and `policy::evaluate`'s
/// argument-blindness (docs/06 §3) is compensated inside the executor.
pub(crate) fn enforce_cap(requested: VolumePct, cap: VolumePct) -> Result<(), ToolError> {
    if requested.within_cap(cap) {
        return Ok(());
    }
    Err(ToolError::Denied(format!(
        "{requested} is above the {cap} Spotify volume cap; propose spotify.volume_boost \
         (needs approval) instead"
    )))
}

pub(crate) fn volume_arg(map: &BTreeMap<String, CanonicalValue>) -> Result<VolumePct, ToolError> {
    let raw = optional_int(map, "volume_pct")?.ok_or_else(|| {
        ToolError::SchemaInvalid("missing required argument `volume_pct`".to_owned())
    })?;
    VolumePct::from_i64(raw).map_err(|e| ToolError::SchemaInvalid(short(&e.to_string())))
}

pub(crate) fn ok(content: String, compensation: Option<String>) -> Result<ToolResult, ToolError> {
    let capped = sanitize_result_content(&content, MAX_RESULT_PROMPT_BYTES);
    Ok(ToolResult {
        content: capped.text,
        truncated: capped.truncated,
        compensation,
    })
}

/// The resource string a grant for `spotify.volume_boost` must cover. Exported
/// so a minting site and this executor's validation use one function rather
/// than two string literals that can drift apart (docs/06 §4).
///
/// It is the tool id, not a device-scoped string, because that is what a real
/// grant covers: the orchestrator mints `GrantBinding::target_resource` from
/// the proposal's tool id (`jarvis-application/src/orchestrator.rs`, the
/// `WaitingApproval` arm). Checking a device-scoped string here would deny
/// every grant the validator actually issues — a silent break of an approved
/// action, which is the worse failure. The **target device is still bound**:
/// `device` is a required argument of this tool, so it is inside
/// `normalized_args_sha256`, and a grant minted for another device fails the
/// fingerprint check in [`check_grant`].
pub fn boost_target_resource() -> String {
    SpotifyVolumeBoostTool::id().as_str().to_owned()
}

/// Re-validate a grant at the executor, immediately before the effect
/// (docs/06 §4, policy-grants skill step 5). The orchestrator's `GrantValidator`
/// is the primary gate — it checks actor, run, resource and expiry under
/// `FOR UPDATE` and consumes the grant — but this is the tool's own fail-closed
/// check, so a direct invocation of the executor cannot bypass it. It therefore
/// has to re-check *everything* that matters, expiry included: an expired grant
/// presented directly to `execute` must not act. Kept symmetric with
/// [`crate::home_assistant`]'s `check_grant`.
pub(crate) fn check_grant(
    grant: Option<&ExecutionGrant>,
    invocation: &ToolInvocation,
    now: SystemTime,
) -> Result<(), ToolError> {
    let Some(grant) = grant else {
        return Err(ToolError::Denied(
            "spotify.volume_boost requires an execution grant".to_owned(),
        ));
    };
    // The grant must bind *these* arguments: a re-hashed proposal, a reused
    // multi-use grant, an expired grant, a grant for another resource, or a
    // different tool/version is not authority here (invariant #1).
    if grant.tool_id != invocation.tool_id
        || grant.tool_version != invocation.tool_version
        || !grant.single_use
        || grant.normalized_args_sha256 != arguments_fingerprint(&invocation.arguments)
        || !grant.target_resource.matches(&boost_target_resource())
        || grant.expires_at <= now
    {
        return Err(ToolError::Denied(
            "execution grant does not match spotify.volume_boost".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn arguments_fingerprint(arguments: &CanonicalValue) -> jarvis_domain::grants::Sha256 {
    let mut hasher = Sha2::new();
    hasher.update(canonical_form(arguments));
    jarvis_domain::grants::Sha256::from_bytes(hasher.finalize().into())
}

/// Every Spotify tool descriptor, in registration order. Host wiring is one
/// call: build the client once, register these.
pub fn descriptors(client: Arc<SpotifyClient>) -> Vec<ToolDescriptor> {
    vec![
        SpotifySearchTool::descriptor(client.clone()),
        SpotifyPlayTool::descriptor(client.clone()),
        SpotifyPlayPlaylistTool::descriptor(client.clone()),
        SpotifyQueueAddTool::descriptor(client.clone()),
        SpotifyVolumeTool::descriptor(client.clone()),
        SpotifyVolumeBoostTool::descriptor(client),
    ]
}
