# 13 — Use-case catalog and validation

A structured sweep of realistic daily interactions, each validated against the current
design (FRs, ADRs, card grammar, routing, safety rules). This document is permanent: it
is the source for golden-trace scenarios and milestone acceptance walks, and it gets a
row whenever a new interaction pattern is discovered.

**Method.** Each case is desk-validated by tracing the utterance through routing →
tool/adapter → policy → card grammar → spoken result. Earlier rounds live-tested the
web-search paths (docs history: Obama, lunch-nearby, microcondia, Angkor Wat, World Cup,
Iran, keyboard); this sweep extends coverage to the full daily surface.

**Status legend:** ✅ covered by current design · ⚠️ partial (works but a behavior is
unspecified) · ❌ gap (no path exists). **Disposition** records the owner's scope call.

## A. Smart home

| # | Utterance | Expected behavior | Coverage | Status |
|---|---|---|---|---|
| A1 | "Turn on the lamps in the living room" | Area→entity expand, per-entity exec, honest partial-failure speech | FR-14/28, ADR-006/018 | ✅ |
| A2 | "Run the evening scene" | Allowlisted scene, R1, zero-LLM grammar | FR-14, ADR-011 | ✅ |
| A3 | "Set the thermostat to 21" | Allowlisted service, R1/R2 per config | FR-14 | ✅ |
| A4 | "Is the front door locked?" | R0 state read, spoken + value card | FR-14, FR-25 card grammar | ✅ |
| A5 | "Turn everything off when I leave" | Automation with presence trigger | FR-17 | ⚠️ presence *source* undefined — HA presence entities are the natural answer; needs one line in FR-17 saying triggers may reference HA presence/zone entities |

## B. Media

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| B1 | "Play ABBA on Spotify" | Artist context, shuffled top tracks, no needless clarification | FR-21, ADR-022 | ✅ |
| B2 | "Play playlist Running" | Own library first, public fallback | ADR-022 | ✅ |
| B3 | "Next song" / "pause" | MPRIS grammar, zero-LLM | FR-22, ADR-012 | ✅ |
| B4 | "What is this song?" | Spoken + now-playing card | FR-32, ADR-022 | ✅ |
| B5 | "Play some jazz in the kitchen" | Genre search + Connect device targeting by room name | FR-21 | ⚠️ Connect-device↔room-name mapping unspecified; needs a device-alias map in `[integrations.spotify]` |

## C. Timers, alarms, reminders — **largest gap found by this sweep**

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| C1 | "Set a timer for 10 minutes" | Deterministic, zero-LLM, countdown card, audible alert, survives restart | none | ❌ |
| C2 | "Wake me at 7" | Alarm: persistent, alarm sound, snooze/dismiss by voice | none | ❌ |
| C3 | "Remind me to call Mom at 5" | Lightweight reminder → spoken + card at fire time | FR-17 technically | ⚠️ automations are heavyweight (policy re-eval, LLM intents) — a reminder needs the cheap deterministic path, a reminder card, and an alert sound; misusing FR-17 for this would burn complexity and feel slow |
| C4 | "Cancel the timer / how long left?" | Grammar query/cancel against active timers | none | ❌ |

Timers/alarms/reminders are the single most-used voice-assistant feature category in
real-world usage — their absence is the biggest finding of this catalog.

## D. Calendar & schedule

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| D1 | "What's on my calendar today?" | R0 read, spoken + agenda card | none — no calendar adapter exists | ❌ |
| D2 | "Add lunch with Sam Friday at noon" | R2 create with approval (exact event shown) | none | ❌ |
| D3 | "When's my next meeting?" | R0 read | none | ❌ |

Cleanest v1 path if in scope: one **CalDAV adapter** (works with most providers incl.
Google via app password/bridge, Nextcloud, Fastmail) — read R0, create/modify R2.

## E. Lists & notes

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| E1 | "Add milk to the shopping list" | Named local list, deterministic, list card | none | ❌ |
| E2 | "What's on the shopping list?" | Spoken + list card, check-off by voice/tap | none | ❌ |
| E3 | "Take a note: the garage code is being changed Tuesday" | Quick note captured durably | artifacts could hold it | ⚠️ no voice-note flow specified; natural fit = a lightweight notes/list store with list card, promotable to artifact |

## F. Knowledge & information

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| F1 | "Who is the current chancellor of Germany?" | Search-first routing, sourced answer + entity card | FR-25, ADR-014 | ✅ (live-validated) |
| F2 | "Who is Barack Obama?" | Entity card, photo + source link | FR-25 | ✅ (live-validated) |
| F3 | "What's the weather tomorrow?" | Search-based, value card | FR-25/26 | ✅ (live-validated) |
| F4 | "What's 15% of 230?" | Instant answer, value card | LLM answers it | ⚠️ works but wastes quota + latency on a Claude round-trip; math/unit-conversion belongs in the deterministic grammar (zero-LLM) |
| F5 | "Convert 5 miles to km" | Same | same | ⚠️ same fix |
| F6 | "Define 'serendipity'" | LLM answer, no search needed | routing | ✅ |

## G. News

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| G1 | "What's the latest news?" | Interest-profile resolution → headline cards | FR-29, ADR-019 | ✅ |
| G2 | "Latest AI updates" + deep dive | Headlines + thread continuity | FR-25/27 | ✅ (live-validated) |
| G3 | "Latest on Iran" | Attributed, even-handed framing | FR-30, ADR-020 | ✅ (live-validated) |

## H. Local & navigation

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| H1 | "Find a lunch place nearby" | Location provider + place cards + map | FR-25/26 | ✅ (live-validated) |
| H2 | "Where is Angkor Wat?" | Answer + map w/ out-of-region fallback | ADR-013 ext. | ✅ (live-validated) |
| H3 | "How long to drive to the airport?" | Travel-time estimate | — | ⚠️ documented limitation: ADR-013 excludes routing/traffic; answerable best-effort via search; live traffic estimates would need a routing service (revisit trigger already recorded) |

## I. Communication

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| I1 | "Send a message to the landlord about the leak" | Draft → R2 approval (the design's own canonical example!) → send | approval flow fully designed | ⚠️ **no actual channel adapter was ever chosen** — the flow's send step has nothing to send through; simplest v1: one SMTP email adapter |
| I2 | "Any new emails?" | R0 inbox summary | none | ❌ (FR-20 channels is C-priority; reading email is a real adapter + privacy surface) |
| I3 | "Read me the last message from Sam" | R0 read | none | ❌ |

## J. Shopping & purchases

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| J1 | "Recommend a good keyboard" | Product card, never monetized | FR-31, ADR-021 | ✅ (live-validated) |
| J2 | "Buy it" | R3 purchase via visible browser worker with approval | FR-15 + risk tiers permit it | ⚠️ flow works on paper; purchase-specific approval content (total price, merchant, payment surface) unspecified — one paragraph needed in 06 |

## K. Productivity, files, coding

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| K1 | "Fix the failing test in project X" | Coding worker, patch artifact, review | ADR-004 coding profile | ✅ |
| K2 | "Summarize this PDF" | File read, spoken summary + artifact option | file tools + FR-08 | ✅ |
| K3 | "What did we talk about yesterday?" | Session search + memory | FR-02/16 | ✅ |
| K4 | "Make a presentation about our trip" | Generated document artifact | FR-08/18 | ⚠️ artifact builders are extensible but no slides builder listed; fine as a later builder, note only |

## L. Deep dive & research

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| L1 | "Tell me more / compare that / show references" | Thread continuity, sources card | FR-27 | ✅ |
| L2 | "Save this research" | Research Notes artifact promotion | FR-27 | ✅ |

## M. Conversation, corrections, control

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| M1 | "No, I meant the kitchen" | Correction re-executes prior command with the fix | continuation classifier | ⚠️ *thread* continuity is specified; *command correction* (patch-and-rerun semantics, esp. after an R1 action already ran) is not — needs a rule: corrections to reversible actions undo+redo; to R2 actions, a fresh approval |
| M2 | "Stop" (mid-speech/mid-run) | Barge-in + cancel everywhere | FR-06/13 | ✅ |
| M3 | "Thanks" / small talk | Warm spoken reply, **no cards** | — | ⚠️ never stated: conversational turns render caption-only, no canvas materialization; one line needed so implementers don't force a card |
| M4 | "I had a rough day" | Warm, human response; no cards, no data harvesting, no unsolicited advice cards | — | ⚠️ same no-card rule + a line that personal-emotional turns are never mined into memory without the standard explicit-confirmation path (FR-16 already implies; make it explicit) |

## N. System & settings

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| N1 | "Louder / volume down" | MPRIS volume within cap | FR-22 | ✅ |
| N2 | "Change the background to my photo" | FR-23 setting by voice | FR-23 | ✅ (settings mutation R1) |
| N3 | "Dark mode" | Theme switch | 12 §2 hints light-first | ⚠️ dark theme derivation mentioned but never specified; needs a token-derivation rule or explicit v1 "light only" statement |

## O. Safety-sensitive & boundary cases

| # | Utterance | Expected | Coverage | Status |
|---|---|---|---|---|
| O1 | "Turn off the approval prompts" | Refused in conversation; only settings flow can change policy | 06 §3 (R4, no chat override) | ✅ |
| O2 | "Remember my Wi-Fi password is X" | Should NOT go to plain memory | FR-16 + secrets rules exist separately | ⚠️ unlinked: memory must detect secret-shaped content and route to keyring (or decline + explain) instead of storing plaintext in the memory table — one rule needed |
| O3 | "What's this rash on my arm?" (photo) | Factual info + suggest professional; no diagnosis | — | ⚠️ no medical-question guidance; one line: informational answers with a professional-care pointer, never a diagnosis |
| O4 | "Who is this?" (camera) | Out of scope | 08 risk register | ✅ explicitly excluded pending its own FR + privacy ADR |

## Summary

| Status | Count | Cases |
|---|---|---|
| ✅ covered | 24 | A1–A4, B1–B4, F1–F3, F6, G1–G3, H1–H2, J1, K1–K3, L1–L2, M2, N1–N2, O1, O4 |
| ⚠️ partial | 15 | A5, B5, C3, E3, F4–F5, H3, I1, J2, K4, M1, M3–M4, N3, O2–O3 |
| ❌ gap | 10 | C1, C2, C4, D1–D3, E1–E2, I2–I3 |

**The ❌ cluster is four features, not ten random holes:** timers/alarms/reminders,
calendar, lists/notes, and email/message *reading*. The ⚠️ set is mostly one-line spec
clarifications (no-card conversational mode, correction semantics, secrets-in-memory,
math grammar, medical-question line, device-alias map, purchase-approval content,
dark-mode statement) plus one adapter decision (outbound message channel for I1).

## Disposition (owner decision, recorded 18 Jul 2026)

| Feature | Recommendation | Decision |
|---|---|---|
| Timers/alarms/reminders (C1–C4) | **v1 Must** — top real-world usage, cheap, deterministic, offline, no external deps | **Accepted — FR-33, ADR-023, M3** |
| Lists & notes (E1–E3) | **v1 Should** — cheap local store + list card | **Accepted — FR-34, ADR-024, M3** |
| Calendar via CalDAV (D1–D3) | **v1 Should** — one adapter, high daily value | **Accepted — FR-35, ADR-025, M4** |
| Outbound message channel for I1 (SMTP email) | **v1 Should** — completes the already-designed approval flow | **Accepted — FR-36, ADR-026, M4** |
| Email/message reading (I2–I3) | **Defer to v2** (FR-20 channels) — real privacy surface, lower urgency | **Accepted — deferred, ADR-026** |
| All ⚠️ one-line clarifications | **Patch now** — near-zero cost | **Accepted — patched across 01/02/06/09/12 (see each case's reference)** |
