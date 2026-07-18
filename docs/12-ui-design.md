# 12 — UI design: voice-first HUD (normative)

Supersedes the earlier "operator console" draft. The working reference implementation is
`design-refs/jarvis-hud-final.html` — where prose and reference disagree, this document
wins, but the reference defines the intended *feel*. Earlier iterations
(`hud-directions`, `hud-states`, `hud-live`) are kept for design history.

## 1. Design thesis

**Voice is the primary channel; the screen is a HUD that materializes evidence.** Jarvis
speaks its answer; the display simultaneously renders the structured proof — a person
card for "who is this", restaurant cards + a real map + a menu for "find a ramen place",
a large numeric readout for "what's the temperature". There is **no chat transcript on
the front face**. The interface has two layers:

- **HUD face (default):** presence orb, spoken caption, materialization canvas.
- **Ops layer (one keystroke, `Ctrl+.` or click the orb):** Run Spine, run timeline,
  audit detail, diagnostics, provider health — the full operator console from the safety
  model. Approval cards do **not** wait in the ops layer: R2/R3 approvals interrupt onto
  the HUD face as cards (amber, urgency pulse) because they are the human's job.

## 2. Anatomy of the HUD face

```
┌────────────────────────────────────────────────────────── bg switch ─┐
│ (orb)  "Kome Ramen is 8 minutes away, rated 4.7…"       [wallpaper]  │  presence + caption
│         Speaking · claude ok                                          │
│  [Earlier: Weather ×] [Who is this ×]                    Clear all ×  │  shelf
│ ┌──────────┐ ┌──────────┐ ┌──────────┐                                │
│ │ photo    │ │ photo    │ │ photo    │   ← result cards, scan-reveal  │
│ │ Top pick │ │ Nearby   │ │ Nearby   │                                │
│ └──────────┘ └──────────┘ └──────────┘                                │
│ ┌───────────────┐ ┌──────────────────────────┐                        │
│ │ REAL MAP      │ │ MENU (photo grid+prices) │                        │  canvas rows
│ │ route + pin   │ │                          │                        │
│ └───────────────┘ └──────────────────────────┘                        │
└──────────────────────────────────────────────────────────────────────┘
```

### 2.1 Presence orb
Small (clamp 46px–7vmin–64px), persistent top-left; concentric rings + frosted core.
State language (color + motion, both required — color alone fails accessibility):

| State | Hue token | Motion signature |
|---|---|---|
| Idle | `--c-idle` blue | slow ring spin, 3s core breathe |
| Listening | `--c-listen` teal | waveform bars inside core |
| Thinking/Speaking | `--c-speak` violet | two counter-orbiting sparks |
| Tool running | blue | progress arc sweep on outer ring |
| Waiting on you | amber (exclusive to approvals) | ring urgency pulse |
| Done | green | single 700ms bloom, then still |
| Error | red | one 480ms shake, then static red ring |
| Degraded | gray | rings slow to 60s, desaturate, dashed |

Amber exclusivity survives the HUD pivot: it appears only when a human decision is
wanted.

### 2.2 Caption (the voice made visible)
One utterance at a time next to the orb, `clamp(16px, 2.6vmin, 25px)`, words entering
with a 70ms-stagger blur-in synchronized with TTS onset (real implementation: reveal
words as TTS timing marks fire, or per sentence if the engine lacks marks — never a
fake typewriter slower than the audio). `aria-live="polite"`. The last utterance
persists until the next; full transcript lives in the ops layer (NFR-11 requires a
readable history — the HUD not showing it is a layout choice, not an omission).
Over wallpaper, the caption block gains its own glass chip (see §5).

### 2.3 Materialization canvas — card grammar
Results are typed **HUD cards** in responsive grid rows
(`repeat(auto-fit, minmax(min(100%,200px),1fr))`). Card types v1: entity/person
(photo, confidence, facts), place (photo, rating/distance/price pills, `pick` variant
with hue ring), **map** (§3), media/menu grid (photo + name + price), value readout
(hero number `clamp(40px,8vmin,64px)` tabular-nums with count-up ≤700ms, mini-stats
landing staggered), **headlines/digest** (found in review — "latest about the World
Cup": 3–5 short items, each a one-line title + one-line summary + relative time +
source link; no photos required, thumbnail optional; distinct from a single entity card
because the point is several current items, not one fact), **sources** (a compact list
of pages consulted — title + domain + link each — for "show me the references";
FR-27/ADR-017), **gallery** (a small image grid capped at 6–8, *each tile individually
source-badged* because images may come from different pages — one shared source link is
not acceptable when provenance differs; FR-27/ADR-017), **product** (name, price, a few key specs, one-line "why", source/retailer link — for "recommend a X"; ranked only by fit and source quality, **never monetized** — the retailer link is a plain reference, no affiliate/sponsored placement, ever; FR-31/ADR-021), **now-playing** (title/artist/album, art when the active player exposes it, source app noted — answers "what's playing" as a first-class query, not just the passive media bar; FR-32/ADR-022), **timer/reminder** (live countdown or fire-time, named, dismiss/snooze affordances; FR-33/ADR-023), **list** (named list items with voice/tap check-off; FR-34/ADR-024), **agenda** (today/next events, time + title + location; FR-35/ADR-025), approval (from the safety
spec: verbatim mono payload, Approve/Deny, countdown), status/queued, error. Any card
image sourced from the web (FR-25/ADR-014 — person, place, weather, menu photos absent
a dedicated integration) carries a small visible source-link chip (domain + link, e.g.
"wikipedia.org ↗") — never presented as Jarvis's own content. A card with no
extractable image renders text-only. New result shapes must extend this grammar — the
model proposes *content*, the client renders only registered card types (this is also a
security property: no model-authored layout/HTML on the HUD face; generated apps stay
in the FR-18 sandbox).

**Reveal animation (the signature):** 620ms clip-path wipe left→right with 6px rise,
staggered ~120ms per card; corner brackets (top-left/bottom-right, hue-colored) lock on
at +420ms; images get one light-sweep scan. Dismissal: 260ms shrink-fade. Nothing loops
after arrival except the map pin pulse and orb.

### 2.4 Clarification (found in review — "show me microcondia")
When a query is genuinely ambiguous between distinct real interpretations — not merely
low-confidence, but two different things it could mean — Jarvis asks **one fluent
spoken/caption question in its own conversational voice** and waits for the next
utterance to resolve it: *"Did you mean the cell organelle, or the fungal spore term?"*
— not a multiple-choice picker. Button/option pickers are a text-chat-interface
convention; this is a voice-first HUD (§1), so disambiguation is a turn of dialogue, not
a form. The caption/orb behave exactly as any other spoken turn (§2.1–2.2) — no special
"question mode" UI is needed, which is the point: asking is as ordinary as answering.
Source-quality weighting (ADR-016) reduces how often clarification is even needed by
keeping low-authority content from contaminating the answer in the first place.

**Conversational turns are caption-only (catalog M3/M4).** Small talk, thanks,
acknowledgements, and personal/emotional turns ("I had a rough day") render as spoken
caption only — **no cards materialize**. Forcing a data panel onto a human moment is a
category error. Emotional/personal content is additionally never mined into memory
outside the standard explicit-confirmation path (FR-16) — Jarvis responds warmly and
moves on; it does not harvest.

**Medical and similar personal-stakes questions (catalog O3)** get factual, sourced
information plus a plain suggestion to consult a professional — never a diagnosis, and
never a card that dresses uncertainty up as data.

### 2.5 Deep dive — continuity, navigation, keeping the result (FR-27, ADR-017)
A deep dive is a *thread*, not a series of unrelated queries.

- **Follow-ups extend, they don't shelve.** The router classifies each query as
  continuation or new-topic (same routing-signal mechanism as location/ambiguity).
  Continuations ("tell me more", "what about Y", "compare that", pronoun/topical
  back-reference) append new cards to the live canvas and leave prior cards in place;
  only a genuine topic change shelves the canvas (FR-24). Misclassification is cheap to
  correct — "new topic" / "go back to X" by voice — and shelving is reversible via
  Restore, so a boundary error costs one utterance, not lost work.
- **Reading a source is a browser handoff.** "Open that / let me read it" routes to the
  browser worker (FR-15): the real page opens, visibly, in a Chromium window on a chosen
  display. The HUD never re-renders full page content — a scope boundary and a copyright
  boundary both.
- **References and galleries are their own cards** (§2.3): "show me the references" →
  sources card; "show me pictures of X" → gallery card, each image individually
  attributed. A gallery is N search+fetch calls and visibly costs more of the run's
  tool-call budget than a single fact — capped at 6–8 images, and a real latency/quota
  tradeoff on the Claude-CLI-only setup (ADR-011), not a free UI flourish.
- **Keeping the result.** Past `[ui] deepdive_promote_after` follow-ups (default 3) on one
  thread, Jarvis offers — in its normal spoken voice, not a dialog box — to save the
  thread as a **Research Notes artifact** (FR-08): a versioned markdown document with the
  accumulated (paraphrased) facts, every source consulted, and referenced images;
  reopenable after restart. The canvas keeps showing only the current conversation; the
  artifact is the durable bibliography and history.
- **Full history is in the ops layer.** Every follow-up is a normal run on the Run Spine,
  so the complete thread — every card, source, and turn — is one keystroke away
  (`Ctrl+.`) without crowding the HUD face.

## 3. Maps are real maps (ADR-013)

Stylized radar is rejected as the primary map — it carries no information. The map card
embeds a real interactive map:

- **Production stack: MapLibre GL JS + PMTiles** region extract served locally by
  `jarvisd` — a real street map that works **offline** (NFR-06) with no API key, no
  tracking, and no per-load cost. Fits local-first exactly.
- **Dev/bootstrap:** Leaflet + OpenStreetMap raster tiles (as in the reference file) is
  acceptable until the PMTiles pipeline exists; OSM attribution is mandatory and never
  hidden.
- Card contents: destination pin (hue-colored, pulsing), current-location dot, route
  polyline, coordinates + distance + walk time beneath in tabular-nums. Interactions:
  pan/zoom inline; a "open large" affordance expands to a full-canvas map card.
- Place *data* (search results, ratings, hours) comes from the tools layer with its own
  provider attribution rules; the map renders geometry, it is not the data source.
- **Coverage fallback (found in review — "where is Angkor Wat").** The local PMTiles
  extract covers the owner's home region only. For a query outside its bounding box, the
  card falls back to online OSM raster tiles when network is available, or — offline —
  renders a coordinates-only card (lat/long, distance and bearing from home, no
  interactive map) instead of a blank or wrong-region map. Never silently show the
  wrong place.

## 4. Panel lifecycle (decided)

- A new query **shelves** the current canvas: panels collapse into a labeled chip in the
  shelf row ("Ramen places · Restore · ×"). Shelf holds max 4; oldest drops.
- **Restore** swaps a shelved set back onto the canvas (shelving what was there).
- **Dismiss** = hover/focus `×` per card, `×` per shelf chip, and a "Clear all" action.
- **TTL: 2 hours.** Shelved and displayed panels self-expire at 2h (config
  `[ui] panel_ttl_hours`, default 2). Expiry is silent — no animation, no notification.
- Approval cards are exempt from shelving/TTL: they persist until decided or their own
  grant expiry, and a new query does not shelve a pending approval.

## 5. Background feature (FR-23) and glass contrast system

User-selectable background: none (light gradient), abstract (generated gradient mesh),
photo (user-supplied image). The glass system adapts as a unit via tokens — components
never hand-tune:

| Token | No background | Wallpaper active |
|---|---|---|
| `--glass-alpha` | .55 | .68 |
| `--glass-blur` | 1.4vmin | 2.4vmin |
| `--glass-border` | white/.8 | white/.55 |
| `--glass-shadow` | soft, warm-neutral | deeper, cool-tinted |
| scrim | none | full-viewport light gradient scrim (white .28–.5) |

Plus: orb gains a drop shadow; caption/status gain a glass chip. Rule: any text over an
unpredictable background must sit on glass or scrim — never raw. Acceptance includes a
contrast audit on both bundled worst-case wallpapers (≥4.5:1 for body text, ≥3:1 for
large caption text).

**Theme (catalog N3).** v1 ships the light-glass theme only. A dark theme is a planned
token-set swap (the entire visual system already routes through `--glass-*`/hue tokens
precisely so this is a derivation, not a redesign) — recorded as a deferred decision in
`08` §6, not silently absent.

## 6. Motion & power policy

- All ambient motion (particles, ring spin, breathe) **stops** when: window hidden or
  unfocused, `prefers-reduced-motion`, or the low-power profile flags battery-saver.
  Event animations (reveal, bloom, shake) reduce to ≤1ms under reduced-motion.
- Particle field: max ~25 live motes, CSS-transform only, spawn paused when hidden.
- Animation budget: HUD idle must add ~0% CPU with window unfocused and ≤ a few percent
  of one core focused-idle on the reference ultrabook; measured at `/gate` with the perf
  scenarios (extends `01` §4.1 — the GPU/compositor cost of glass blur is part of the
  budget, and blur radius is the first knob to turn down on 8 GB hosts).

## 7. Scaling (resolution independence)

No fixed pixels anywhere in layout: orb and rings in `vmin`/%; type in
`clamp(min, vmin, max)`; canvas `min(96vw, 920px)`; grids `auto-fit/minmax`; map and
photos by aspect-ratio. The same layout must hold from a 1080p laptop to 4K TV to a
portrait mobile viewport (cards stack single-column via the same `auto-fit` rule).
Snapshot tests at 1366×768, 1920×1080, 3840×2160, and 390×844.

## 8. Accessibility

Caption `aria-live`; every spoken utterance also visible; transcript in ops layer;
state changes announced (state name, not just color/motion); all controls keyboard
reachable with visible `:focus-visible` ring; dismiss/restore/clear operable without
hover (focus-within reveals `×`); reduced-motion fully honored; captions persist for
muted/silent use.

## 9. Acceptance for HUD work

Keyboard-only walkthrough · amber-exclusivity grep · card grammar only (no free-form
model HTML) · lifecycle behaviors (shelve/restore/dismiss/2h TTL/approval exemption)
tested · both wallpapers pass contrast audit · reduced-motion + hidden-window CPU checks
pass · every web-sourced image on a card shows its source link · map renders offline from local PMTiles in the M-gate demo · screenshot set (idle,
listening, speaking+canvas, approval interrupt, degraded, each background) attached to
the PR for owner review.
