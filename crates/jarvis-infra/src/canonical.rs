//! JSON projection for [`CanonicalValue`].
//!
//! These two conversions live in this crate rather than in `jarvis-domain`
//! because the domain has `serde_json` only as a dev-dependency, and adding a
//! real dependency to `jarvis-domain` is a human-only decision (docs/11 §3).
//! They are here rather than duplicated in each host because more than one
//! place needs them — the approval card projects arguments for a human to read
//! (F2.5), and automation storage round-trips them through a column (F8.6) —
//! and the two must not drift: a value that canonicalises differently binds a
//! different hash (docs/06 §4/§5).

use jarvis_domain::tools::CanonicalValue;

/// Project a [`CanonicalValue`] to JSON.
///
/// Display and storage only. The arguments that actually *bind* are always the
/// stored `CanonicalValue`, never a value re-derived from this projection.
pub fn canonical_to_json(value: &CanonicalValue) -> serde_json::Value {
    use serde_json::Value;
    match value {
        CanonicalValue::Null => Value::Null,
        CanonicalValue::Bool(b) => Value::Bool(*b),
        CanonicalValue::Int(n) => Value::Number((*n).into()),
        CanonicalValue::Float(text) => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        CanonicalValue::Str(s) => Value::String(s.clone()),
        CanonicalValue::Array(items) => Value::Array(items.iter().map(canonical_to_json).collect()),
        CanonicalValue::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), canonical_to_json(v)))
                .collect(),
        ),
    }
}

/// Lift JSON back into a [`CanonicalValue`].
///
/// Object keys sort (via `CanonicalValue::Object`'s `BTreeMap`), so the same
/// value in any key order yields the same canonical form and the same hash
/// (docs/06 §4/§5). An integer that does not fit `i64` degrades to a `Float`
/// string — still deterministic, so re-validating the same input binds.
pub fn json_to_canonical(value: serde_json::Value) -> CanonicalValue {
    use serde_json::Value;
    match value {
        Value::Null => CanonicalValue::Null,
        Value::Bool(b) => CanonicalValue::Bool(b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => CanonicalValue::Int(i),
            None => CanonicalValue::Float(n.to_string()),
        },
        Value::String(s) => CanonicalValue::Str(s),
        Value::Array(items) => {
            CanonicalValue::Array(items.into_iter().map(json_to_canonical).collect())
        }
        Value::Object(map) => CanonicalValue::Object(
            map.into_iter()
                .map(|(k, v)| (k, json_to_canonical(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_domain::tools::canonical_form;

    #[test]
    fn a_value_round_trips_through_json() {
        let original = serde_json::json!({
            "entity_id": "light.kitchen",
            "brightness": 200,
            "on": true,
            "scenes": ["evening", "dim"],
            "nothing": null
        });
        let canonical = json_to_canonical(original.clone());
        assert_eq!(canonical_to_json(&canonical), original);
    }

    /// Key order must not change the canonical form, or the same approved edit
    /// would bind a different hash depending on how it was typed.
    #[test]
    fn key_order_does_not_change_the_canonical_form() {
        let a = json_to_canonical(serde_json::json!({"b": 1, "a": 2}));
        let b = json_to_canonical(serde_json::json!({"a": 2, "b": 1}));
        assert_eq!(a, b);
        assert_eq!(canonical_form(&a), canonical_form(&b));
    }

    /// A number outside `i64` becomes a `Float` string rather than being lost,
    /// and does so the same way every time — otherwise the same approved edit
    /// would bind a different hash on a retry.
    #[test]
    fn a_number_too_large_for_i64_degrades_deterministically() {
        let big = serde_json::json!({ "n": 9_223_372_036_854_775_808_u64 });
        assert_eq!(
            json_to_canonical(big.clone()),
            json_to_canonical(big.clone())
        );
        let CanonicalValue::Object(map) = json_to_canonical(big) else {
            panic!("expected an object");
        };
        assert!(
            matches!(map.get("n"), Some(CanonicalValue::Float(_))),
            "a u64 beyond i64 must degrade to a deterministic Float string"
        );
    }
}
