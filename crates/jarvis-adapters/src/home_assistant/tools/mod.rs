//! One file per tool (F9.5); shared plumbing here.

use std::collections::BTreeSet;
use std::time::SystemTime;

use jarvis_domain::grants::ExecutionGrant;
use jarvis_domain::tools::{CanonicalValue, ToolError, ToolInvocation, canonical_form};
use sha2::{Digest, Sha256 as Sha2};
use tokio_util::sync::CancellationToken;

use super::*;

mod execute_scene;
mod get_state;
mod set_area_lights;
mod set_light;

pub use execute_scene::*;
pub use get_state::*;
pub use set_area_lights::*;
pub use set_light::*;

// ---------------------------------------------------------------------------
// Shared tool plumbing
// ---------------------------------------------------------------------------

pub(crate) fn arguments_fingerprint(arguments: &CanonicalValue) -> jarvis_domain::grants::Sha256 {
    let mut hasher = Sha2::new();
    hasher.update(canonical_form(arguments));
    jarvis_domain::grants::Sha256::from_bytes(hasher.finalize().into())
}

/// Read the exact set of string keys an argument object must carry — extra or
/// missing keys are a schema violation, so an argument the executor would ignore
/// can never ride along inside a grant's hash.
fn exact_string_args<'a>(
    arguments: &'a CanonicalValue,
    keys: &[&str],
) -> Result<Vec<&'a str>, ToolError> {
    let CanonicalValue::Object(map) = arguments else {
        return Err(ToolError::SchemaInvalid(
            "home arguments must be an object".to_owned(),
        ));
    };
    let expected: BTreeSet<&str> = keys.iter().copied().collect();
    let actual: BTreeSet<&str> = map.keys().map(String::as_str).collect();
    if actual != expected {
        return Err(ToolError::SchemaInvalid(format!(
            "home arguments must be exactly {{{}}}",
            keys.join(", ")
        )));
    }
    let mut values = Vec::with_capacity(keys.len());
    for key in keys {
        match map.get(*key) {
            Some(CanonicalValue::Str(value)) => values.push(value.as_str()),
            _ => {
                return Err(ToolError::SchemaInvalid(format!(
                    "home argument `{key}` must be a string"
                )));
            }
        }
    }
    Ok(values)
}

fn parse_entity(value: &str) -> Result<EntityId, ToolError> {
    value
        .parse()
        .map_err(|_| ToolError::SchemaInvalid("invalid home entity id".to_owned()))
}

/// The denial a non-allowlisted target produces. The entity id is echoed on
/// purpose — it is owner-visible configuration, not a secret, and naming it is
/// what makes the denial actionable.
fn not_allowlisted(entity: &EntityId) -> ToolError {
    ToolError::Denied(format!("{entity} is not on the home control allowlist"))
}

/// Re-validate a grant at the executor, immediately before the effect
/// (docs/06 §4, policy-grants skill step 5). The orchestrator's `GrantValidator`
/// is the primary gate; this is the tool's own fail-closed check so a direct
/// invocation of the executor cannot bypass it. It therefore re-checks
/// *everything* that matters, expiry included. Kept symmetric with
/// [`crate::spotify`]'s `check_grant`.
///
/// The target entity is bound through `normalized_args_sha256` rather than
/// through the resource pattern — see [`grant_target_resource`] for why.
fn check_grant(
    grant: Option<&ExecutionGrant>,
    invocation: &ToolInvocation,
    now: SystemTime,
) -> Result<(), ToolError> {
    let Some(grant) = grant else {
        return Err(ToolError::Denied(format!(
            "{} requires an execution grant",
            invocation.tool_id
        )));
    };
    let fingerprint = arguments_fingerprint(&invocation.arguments);
    if grant.tool_id != invocation.tool_id
        || grant.tool_version != invocation.tool_version
        || !grant.single_use
        || grant.normalized_args_sha256 != fingerprint
        || !grant
            .target_resource
            .matches(&grant_target_resource(&invocation.tool_id))
        || grant.expires_at <= now
    {
        return Err(ToolError::Denied(format!(
            "execution grant does not match {}",
            invocation.tool_id
        )));
    }
    Ok(())
}

/// The friendly-name argument the approval card renders is checked against HA's
/// own metadata before the effect happens. Text never grants authority: a
/// proposal that claims `script.disarm_alarm` is "Kitchen timer" is refused,
/// rather than trusted, so the name a human approved is the name HA holds.
async fn verify_label(
    client: &HomeAssistantClient,
    entity: &EntityId,
    claimed: &str,
    cancel: &CancellationToken,
) -> Result<EntityMetadata, ToolError> {
    let metadata = client.metadata(entity, cancel).await?;
    if metadata.friendly_name != clean_text(claimed, MAX_FRIENDLY_NAME_CHARS) {
        return Err(ToolError::Denied(format!(
            "the approved name does not match Home Assistant's name for {entity}"
        )));
    }
    Ok(metadata)
}
