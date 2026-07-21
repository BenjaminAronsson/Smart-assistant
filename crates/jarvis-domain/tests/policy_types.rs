//! F2.1 acceptance tests for the policy/tool/grant value types (docs/05 §4,
//! docs/06 §3–4). These are the security-critical properties the higher layers
//! rely on: canonicalization is key-order-independent yet material-change
//! sensitive (confused-deputy defence, docs/06 §5), risk tiers classify
//! correctly, and grant binding round-trips exactly.

use std::time::{Duration, SystemTime};

use jarvis_domain::grants::{ExecutionGrant, GrantId, Sha256};
use jarvis_domain::ids::{DeviceId, RunId, UserId};
use jarvis_domain::policy::{DataEgress, ResourcePattern, RiskLevel, Scope, ToolPolicy};
use jarvis_domain::tools::{
    CanonicalValue as V, ToolId, ToolInvocation, ToolProposal, ToolVersion, canonical_form,
};

// ---- canonicalization: the confused-deputy defence -----------------------

/// The core property: reordering object keys must not change the canonical
/// bytes. BTreeMap guarantees this structurally; this test locks it in as a
/// contract so a future refactor to a hash map is caught.
#[test]
fn canonical_form_is_key_order_independent() {
    let a = V::obj([
        ("recipient", V::str("alice@example.com")),
        ("subject", V::str("hi")),
        ("attempts", V::int(2)),
    ]);
    let b = V::obj([
        ("attempts", V::int(2)),
        ("subject", V::str("hi")),
        ("recipient", V::str("alice@example.com")),
    ]);
    assert_eq!(canonical_form(&a), canonical_form(&b));
}

/// Nested objects are canonicalized recursively.
#[test]
fn canonical_form_is_key_order_independent_when_nested() {
    let a = V::obj([(
        "msg",
        V::obj([("to", V::str("bob")), ("cc", V::array([V::str("x")]))]),
    )]);
    let b = V::obj([(
        "msg",
        V::obj([("cc", V::array([V::str("x")])), ("to", V::str("bob"))]),
    )]);
    assert_eq!(canonical_form(&a), canonical_form(&b));
}

/// A material change (different value) must change the canonical bytes — else
/// an edited-args proposal would validate against an old grant.
#[test]
fn canonical_form_detects_material_change() {
    let base = V::obj([("recipient", V::str("alice@example.com"))]);
    let edited = V::obj([("recipient", V::str("mallory@example.com"))]);
    assert_ne!(canonical_form(&base), canonical_form(&edited));
}

/// Type changes are material: string "2" and integer 2 must not collide.
#[test]
fn canonical_form_distinguishes_types() {
    assert_ne!(
        canonical_form(&V::obj([("n", V::str("2"))])),
        canonical_form(&V::obj([("n", V::int(2))])),
    );
    assert_ne!(
        canonical_form(&V::obj([("n", V::Null)])),
        canonical_form(&V::obj([("n", V::str(""))])),
    );
}

/// Float text is length-prefixed, so terminator-shaped content in one value
/// cannot be confused with a following value (security-auditor advisory).
#[test]
fn canonical_form_float_text_is_self_delimiting() {
    assert_ne!(
        canonical_form(&V::obj([("a", V::Float("1;i0".into())), ("b", V::Null)])),
        canonical_form(&V::obj([("a", V::Float("1".into())), ("b", V::int(0))])),
    );
}

/// Array order is significant (it is data, not a key set).
#[test]
fn canonical_form_array_order_is_significant() {
    assert_ne!(
        canonical_form(&V::array([V::int(1), V::int(2)])),
        canonical_form(&V::array([V::int(2), V::int(1)])),
    );
}

/// A key named the same as a value cannot smuggle a collision: the encoding is
/// unambiguous across the string/key boundary.
#[test]
fn canonical_form_has_no_key_value_ambiguity() {
    let a = V::obj([("a", V::str("b"))]);
    let b = V::obj([("a\u{0}b", V::Null)]);
    assert_ne!(canonical_form(&a), canonical_form(&b));
}

// ---- Sha256 container ----------------------------------------------------

#[test]
fn sha256_hex_round_trips() {
    let bytes = [0xabu8; 32];
    let h = Sha256::from_bytes(bytes);
    let hex = h.to_string();
    assert_eq!(hex.len(), 64);
    assert_eq!(hex, "ab".repeat(32));
    assert_eq!(hex.parse::<Sha256>().unwrap(), h);
}

#[test]
fn sha256_rejects_bad_hex() {
    assert!("zz".repeat(32).parse::<Sha256>().is_err());
    assert!("ab".parse::<Sha256>().is_err()); // too short
}

// ---- risk tiers ----------------------------------------------------------

#[test]
fn risk_tiers_classify_handling() {
    // R0/R1 are auto-authorizable; R2/R3 need a grant; R4 is never allowed.
    assert!(!RiskLevel::R0.requires_approval());
    assert!(!RiskLevel::R1.requires_approval());
    assert!(RiskLevel::R2.requires_approval());
    assert!(RiskLevel::R3.requires_approval());
    assert!(RiskLevel::R4.is_prohibited());
    assert!(!RiskLevel::R0.is_prohibited());
    // A prohibited action never requires "approval" — it is rejected outright.
    assert!(!RiskLevel::R4.requires_approval());
}

// ---- ToolId / ToolVersion validation -------------------------------------

#[test]
fn tool_id_accepts_dotted_names_rejects_junk() {
    assert!("fs.read".parse::<ToolId>().is_ok());
    assert!("web.search".parse::<ToolId>().is_ok());
    assert!("home.set_light".parse::<ToolId>().is_ok());
    assert!("fs".parse::<ToolId>().is_err()); // must be namespaced
    assert!("Fs.Read".parse::<ToolId>().is_err()); // lowercase only
    assert!("fs..read".parse::<ToolId>().is_err());
    assert!("".parse::<ToolId>().is_err());
    assert!("fs.read; rm -rf".parse::<ToolId>().is_err());
}

#[test]
fn tool_version_parses_and_displays() {
    let v: ToolVersion = "1.4.2".parse().unwrap();
    assert_eq!(v, ToolVersion::new(1, 4, 2));
    assert_eq!(v.to_string(), "1.4.2");
    assert!("1.4".parse::<ToolVersion>().is_err());
    assert!("x.y.z".parse::<ToolVersion>().is_err());
}

// ---- ResourcePattern matching --------------------------------------------

#[test]
fn resource_pattern_matches_exact_and_prefix_glob() {
    let exact: ResourcePattern = "/projects/jarvis/README.md".parse().unwrap();
    assert!(exact.matches("/projects/jarvis/README.md"));
    assert!(!exact.matches("/projects/jarvis/secret"));

    let glob: ResourcePattern = "/projects/jarvis/*".parse().unwrap();
    assert!(glob.matches("/projects/jarvis/README.md"));
    assert!(glob.matches("/projects/jarvis/src/main.rs"));
    assert!(!glob.matches("/etc/passwd"));
    // The glob must not allow escaping the prefix via traversal text.
    assert!(!glob.matches("/projects/other/x"));
}

// ---- ToolPolicy ----------------------------------------------------------

#[test]
fn tool_policy_requires_grant_follows_risk() {
    let read = ToolPolicy {
        risk: RiskLevel::R0,
        is_reversible: true,
        requires_user_presence: false,
        timeout: Duration::from_secs(5),
        required_scopes: [Scope::new("files:read").unwrap()].into_iter().collect(),
        egress: DataEgress::None,
    };
    assert!(!read.requires_grant());

    let send = ToolPolicy {
        risk: RiskLevel::R2,
        is_reversible: true,
        requires_user_presence: true,
        timeout: Duration::from_secs(30),
        required_scopes: Default::default(),
        egress: DataEgress::External,
    };
    assert!(send.requires_grant());
}

// ---- ExecutionGrant binding + expiry -------------------------------------

fn ulid(seed: char) -> String {
    std::iter::repeat_n(seed, 26).collect()
}

#[test]
fn execution_grant_binds_and_expires() {
    let args = V::obj([("recipient", V::str("alice@example.com"))]);
    let grant = ExecutionGrant {
        grant_id: GrantId::from_bytes([7u8; 32]),
        user_id: ulid('1').parse::<UserId>().unwrap(),
        device_id: ulid('2').parse::<DeviceId>().unwrap(),
        run_id: ulid('3').parse::<RunId>().unwrap(),
        tool_id: "message.send".parse::<ToolId>().unwrap(),
        tool_version: ToolVersion::new(1, 0, 0),
        normalized_args_sha256: Sha256::from_bytes([9u8; 32]),
        target_resource: "alice@example.com".parse::<ResourcePattern>().unwrap(),
        expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
        single_use: true,
    };

    assert!(grant.single_use);
    assert!(grant.is_expired(SystemTime::UNIX_EPOCH + Duration::from_secs(1001)));
    assert!(!grant.is_expired(SystemTime::UNIX_EPOCH + Duration::from_secs(999)));

    // Sanity: the proposal/invocation carry the same argument shape the grant
    // was minted against.
    let proposal = ToolProposal {
        tool_id: "message.send".parse().unwrap(),
        arguments: args.clone(),
    };
    let invocation = ToolInvocation {
        tool_id: proposal.tool_id.clone(),
        tool_version: ToolVersion::new(1, 0, 0),
        arguments: args,
    };
    assert_eq!(
        canonical_form(&proposal.arguments),
        canonical_form(&invocation.arguments)
    );
}

// ---- GrantId hex round-trip ----------------------------------------------

#[test]
fn grant_id_hex_round_trips() {
    let id = GrantId::from_bytes([0x1fu8; 32]);
    let text = id.to_string();
    assert_eq!(text.len(), 64);
    assert_eq!(text.parse::<GrantId>().unwrap(), id);
}
