//! Spoken light target → allowlisted entity id (F5.5, FR-13/FR-14).
//!
//! `jarvis_application::home::LightTargetResolver` is the seam the deterministic
//! grammar uses to turn "turn on the desk lamp" into a `home.set_light`
//! proposal. The application layer deliberately cannot know the allowlist, and
//! the trait forbids I/O on that path — it runs on the quota-free deterministic
//! route, whose whole point is answering without waiting on anything. So the
//! mapping is built once at startup from `[integrations.home_assistant]` and
//! then answered from memory. Being immutable also satisfies the trait's other
//! requirement — the answer must be stable for the lifetime of a run, or the
//! deterministic provider could end up looking at the result of a tool it never
//! proposed (M5 audit S4).
//!
//! **The security property is that this can only ever resolve *downward*, into
//! entities the owner explicitly allowlisted.** It never constructs an entity
//! id from the utterance: an unknown target returns `None`, the utterance is
//! not recognized, and it goes to the reasoning provider instead. That is why
//! a slugified guess is not acceptable here — `light.living_room_lights` might
//! name a real, non-allowlisted device, and inventing it would route a spoken
//! phrase at hardware the owner never authorized. Text proposes; the allowlist
//! decides what is even nameable.

use std::collections::BTreeMap;

use jarvis_application::home::LightTargetResolver;

/// Resolves spoken phrases against the configured light allowlist.
#[derive(Debug, Default)]
pub struct ConfiguredLightTargets {
    /// Normalized spoken form → entity id. `BTreeMap` keeps construction
    /// deterministic, which matters for the collision rule below.
    by_spoken: BTreeMap<String, String>,
}

impl ConfiguredLightTargets {
    /// Build from the `[integrations.home_assistant].lights` allowlist.
    ///
    /// Each `light.desk_lamp` becomes the spoken form `"desk lamp"`. This is the
    /// *inverse* of a guess: the entity id is the input, never the output.
    ///
    /// A spoken form produced by two different entities is dropped entirely
    /// rather than resolved to whichever sorted first — ambiguity must reach a
    /// human (or the provider), not be silently decided. That cannot happen for
    /// distinct ids under this derivation today, but the allowlist is
    /// owner-edited config and the rule should not depend on that.
    pub fn from_allowlist(lights: &[String]) -> Self {
        let mut by_spoken: BTreeMap<String, Option<String>> = BTreeMap::new();
        for entity in lights {
            let Some(spoken) = spoken_form(entity) else {
                continue;
            };
            by_spoken
                .entry(spoken)
                // Second distinct claimant ⇒ poison the entry.
                .and_modify(|slot| {
                    if slot.as_deref() != Some(entity.as_str()) {
                        *slot = None;
                    }
                })
                .or_insert_with(|| Some(entity.clone()));
        }
        Self {
            by_spoken: by_spoken
                .into_iter()
                .filter_map(|(spoken, entity)| entity.map(|e| (spoken, e)))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_spoken.is_empty()
    }
}

impl LightTargetResolver for ConfiguredLightTargets {
    fn resolve_light(&self, spoken_target: &str) -> Option<String> {
        self.by_spoken.get(&normalize(spoken_target)).cloned()
    }
}

/// `light.desk_lamp` → `"desk lamp"`. Anything that is not a `light.*` entity
/// yields `None`: the allowlist constructor already rejects those, and this
/// keeps the derivation from inventing a spoken form for something the
/// `home.set_light` tool would refuse anyway.
fn spoken_form(entity_id: &str) -> Option<String> {
    let object_id = entity_id.strip_prefix("light.")?;
    let spoken = normalize(&object_id.replace('_', " "));
    (!spoken.is_empty()).then_some(spoken)
}

/// Lowercase, collapse whitespace, and drop a leading article so "the desk
/// lamp" and "desk lamp" are the same target. Deliberately conservative: no
/// stemming, no fuzzy matching, no plural folding — a near miss must fall
/// through to the provider rather than actuate the closest-looking device.
fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut words: Vec<&str> = lowered.split_whitespace().collect();
    if matches!(words.first(), Some(&"the") | Some(&"my")) {
        words.remove(0);
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(lights: &[&str]) -> ConfiguredLightTargets {
        let owned: Vec<String> = lights.iter().map(|s| (*s).to_owned()).collect();
        ConfiguredLightTargets::from_allowlist(&owned)
    }

    #[test]
    fn an_allowlisted_light_resolves_from_its_spoken_form() {
        let targets = targets(&["light.desk_lamp", "light.kitchen_ceiling"]);
        assert_eq!(
            targets.resolve_light("desk lamp"),
            Some("light.desk_lamp".to_owned())
        );
        // Articles and casing are noise, not meaning.
        assert_eq!(
            targets.resolve_light("The  Desk   Lamp"),
            Some("light.desk_lamp".to_owned())
        );
        assert_eq!(
            targets.resolve_light("my kitchen ceiling"),
            Some("light.kitchen_ceiling".to_owned())
        );
    }

    // The property the whole module exists for: a target the owner never
    // allowlisted is unresolvable, so the grammar does not recognize the
    // utterance and it reaches the provider instead of actuating anything.
    #[test]
    fn a_target_outside_the_allowlist_never_resolves() {
        let targets = targets(&["light.desk_lamp"]);
        assert_eq!(targets.resolve_light("greenhouse sprinklers"), None);
        // Nor can a caller name an entity id directly to get it back.
        assert_eq!(targets.resolve_light("light.front_door"), None);
        // A near miss is a miss — no fuzzy matching, no plural folding.
        assert_eq!(targets.resolve_light("desk lamps"), None);
        assert_eq!(targets.resolve_light("desk"), None);
    }

    // Only `light.*` is nameable here; `home.set_light` would refuse the rest,
    // so deriving a spoken form for them would only create dead ends.
    #[test]
    fn non_light_entities_contribute_no_spoken_form() {
        let targets = targets(&["switch.pump", "scene.movie_night", "light.desk_lamp"]);
        assert_eq!(targets.resolve_light("pump"), None);
        assert_eq!(targets.resolve_light("movie night"), None);
        assert!(targets.resolve_light("desk lamp").is_some());
    }

    #[test]
    fn an_empty_allowlist_resolves_nothing() {
        assert!(targets(&[]).is_empty());
        assert_eq!(targets(&[]).resolve_light("desk lamp"), None);
    }
}
