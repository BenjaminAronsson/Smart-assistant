//! F0.3: ULID newtype identity contract.
//!
//! docs/04 §2: "All IDs are ULIDs exposed as opaque strings; database sequences never
//! leak (contract convention, `05` §5)."
//!
//! `SessionId`, `DeviceId`, `UserId`, and `RunId` are expected to share identical
//! behavior: `FromStr`/`Display`/`as_str` plus serde round-trip through a bare JSON
//! string, rejecting anything that is not a canonical 26-char uppercase
//! Crockford-base32 ULID.

use jarvis_domain::ids::{DeviceId, IdParseError, RunId, SessionId, UserId};

/// Canonical example ULID (docs/05 §3 style), 26 chars, uppercase Crockford-base32.
const VALID_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// Generates the identical test suite for one ULID newtype so all four ID types are
/// held to the same contract (docs/04 §2).
macro_rules! ulid_newtype_tests {
    ($mod_name:ident, $ty:ty) => {
        mod $mod_name {
            use super::*;
            use std::str::FromStr;

            #[test]
            fn from_str_accepts_canonical_ulid() {
                // FR-contracts: FromStr accepts a canonical 26-char uppercase
                // Crockford-base32 ULID.
                let id = <$ty>::from_str(VALID_ULID).expect("valid ULID must parse");
                assert_eq!(id.as_str(), VALID_ULID);
            }

            #[test]
            fn from_str_rejects_too_short() {
                // 25 chars: wrong length.
                let too_short = &VALID_ULID[..25];
                assert!(
                    <$ty>::from_str(too_short).is_err(),
                    "25-char string must be rejected"
                );
            }

            #[test]
            fn from_str_rejects_too_long() {
                // 27 chars: wrong length.
                let too_long = format!("{VALID_ULID}A");
                assert!(
                    <$ty>::from_str(&too_long).is_err(),
                    "27-char string must be rejected"
                );
            }

            #[test]
            fn from_str_rejects_lowercase() {
                let lower = VALID_ULID.to_lowercase();
                assert!(
                    <$ty>::from_str(&lower).is_err(),
                    "lowercase ULID must be rejected (canonical form is uppercase)"
                );
            }

            #[test]
            fn from_str_rejects_excluded_alphabet_chars() {
                // Crockford base32 excludes I, L, O, U.
                for bad_char in ['I', 'L', 'O', 'U'] {
                    let mut chars: Vec<char> = VALID_ULID.chars().collect();
                    chars[0] = bad_char;
                    let candidate: String = chars.into_iter().collect();
                    assert!(
                        <$ty>::from_str(&candidate).is_err(),
                        "expected rejection of excluded char '{bad_char}' in '{candidate}'"
                    );
                }
            }

            #[test]
            fn from_str_rejects_empty() {
                assert!(
                    <$ty>::from_str("").is_err(),
                    "empty string must be rejected"
                );
            }

            #[test]
            fn parse_error_implements_std_error_and_display() {
                let err: IdParseError = <$ty>::from_str("").unwrap_err();
                // Compile-time assertion: IdParseError implements std::error::Error.
                let as_std_error: &dyn std::error::Error = &err;
                assert!(
                    !as_std_error.to_string().is_empty(),
                    "IdParseError must have a non-empty Display message"
                );
            }

            #[test]
            fn display_returns_original_string() {
                let id = <$ty>::from_str(VALID_ULID).unwrap();
                assert_eq!(id.to_string(), VALID_ULID);
            }

            #[test]
            fn as_str_returns_original_string() {
                let id = <$ty>::from_str(VALID_ULID).unwrap();
                assert_eq!(id.as_str(), VALID_ULID);
            }

            #[test]
            fn serde_serializes_to_bare_json_string() {
                // Contract: IDs are "opaque strings" on the wire, not `{ "id": "..." }`.
                let id = <$ty>::from_str(VALID_ULID).unwrap();
                let json = serde_json::to_string(&id).unwrap();
                assert_eq!(json, format!("\"{VALID_ULID}\""));
            }

            #[test]
            fn serde_deserializes_valid_bare_string() {
                let json = format!("\"{VALID_ULID}\"");
                let id: $ty = serde_json::from_str(&json).unwrap();
                assert_eq!(id.as_str(), VALID_ULID);
            }

            #[test]
            fn serde_deserializing_invalid_string_fails() {
                let json = "\"not-a-ulid\"";
                let result: Result<$ty, _> = serde_json::from_str(json);
                assert!(
                    result.is_err(),
                    "deserializing an invalid ULID string must fail"
                );
            }

            #[test]
            fn serde_deserializing_lowercase_fails() {
                let json = format!("\"{}\"", VALID_ULID.to_lowercase());
                let result: Result<$ty, _> = serde_json::from_str(&json);
                assert!(result.is_err());
            }

            #[test]
            fn serde_deserializing_wrong_json_type_fails() {
                // IDs must not accept a bare number or object in place of a string.
                let result: Result<$ty, _> = serde_json::from_str("4182");
                assert!(result.is_err());
            }
        }
    };
}

ulid_newtype_tests!(session_id, SessionId);
ulid_newtype_tests!(device_id, DeviceId);
ulid_newtype_tests!(user_id, UserId);
ulid_newtype_tests!(run_id, RunId);
