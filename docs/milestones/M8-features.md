# M8 "The house answers" — feature list

Status: **APPROVED — owner sign-off 2026-08-13**, including the requirement amendment and
the roadmap reshuffle. Decomposed the same day on Opus 5 after M7 shipped (tag
`m7-complete`). Split into **three sub-gates** as approved. Nothing implemented yet;
**F8.1 is the next feature loop.** Check items off as their PRs merge, and do not pull a
later sub-gate's feature forward without an approved change to this list (docs/11 §2).

**Resolved at approval:**
1. **FR-13 amended** (docs/01 §2) — hands-free wake-word invocation on the device, raised
   from Should to **Must**, with push-to-talk retained as an equal path because NFR-11
   requires non-voice alternatives.
2. **openWakeWord**, detection **on the node** — nothing streams before the word fires.
   Recorded as **ADR-032** in F8.3, with the asset licence review.
3. **Roadmap reshuffled** — this is M8; product hardening becomes M9, minus the settings
   surface and deployment story, which come forward into M8c.
4. **Three sub-gates**, so a stumble in automations cannot hold the hands-free evidence
   hostage. Each gets its own `/gate` and its own report.
5. **ElevenLabs — deferred at approval, then pulled into scope by owner direction the same
   day as F8.11 (M8c).** The conditions below are unchanged and became F8.11's acceptance
   criteria. Original text follows.
   A `SpeechSynthesizer` adapter is genuinely cheap
   (one port, two methods, streaming already matches, barge-in already threaded) — but the
   decision waits until Piper has been heard in a real kitchen after F8.9. If it lands it
   needs an ADR (new external dependency + a new egress path), opt-in config as the consent
   gate, a local fallback so alarms still ring offline, **sensitivity-aware routing** so
   message bodies and calendar entries are never spoken by a third party, and a character
   budget.

## Why this milestone exists, and what it displaces

M5 built a **voice turn**. M7 built the **transport** for satellites. Neither built the
thing that makes this a voice assistant, and an audit of the tree says so plainly:

- `jarvis-agent` has **no audio code and no audio dependency at all** — it is display-only.
- The **only** thing that captures audio in the repo is the browser (`getUserMedia`).
- There is **no wake word** anywhere — no engine, no code, no config.
- M7's `voice-node` / `room-node` classes therefore describe **a device that cannot exist**:
  the daemon will route audio to a satellite, and nothing on a satellite can open the stream.
- A fired timer plays on the **daemon host's** audio device (`jarvis-adapters::timer_alert`),
  with no device notion at all — set a timer in the kitchen, it rings at the desk.
- **FR-17 automations does not exist** in any form, while `docs/05 §1` advertises its routes.
- M4's deferrable-work scheduler is a library **nothing calls** (D-M4-1, still open).

So today "talking to Jarvis" means opening a browser tab and holding a button. This
milestone is the one that makes the product its own description.

**It displaces the current docs/08 §1 M8 row.** "Product hardening" (installer/update,
backup/restore, policy UI, accessibility, diagnostics bundle, signed releases, golden 10)
moves to **M9**, except the two parts you cannot live without to run a hands-free house at
all — a real deployment story (F8.9) and a settings surface (F8.8) — which come here.

---

## The decisions, as approved (docs/11 §3 — human only)

**1. ✅ FR-13 amended.** It read *"Push-to-talk speech input with partial transcripts,
streaming speech output, barge-in"* — an accurate description of what M5 shipped. A
hands-free assistant was **not a missing implementation; it was scope nobody had written
down.** FR-13 now requires wake-word invocation **and keeps push-to-talk as an equal path**
(NFR-11 requires non-voice alternatives, and PTT is the accessibility route), and is raised
from Should to **Must**. Wake word stops being a docs/08 §6 deferred decision and becomes a
requirement with a milestone behind it.

**2. ✅ openWakeWord**, with the asset licence reviewed in ADR-032, `"jarvis"` as the word,
and a documented swap path — it sits behind a port like every other adapter.

**3. ✅ Detection runs on the node.** The satellite streams nothing until the word fires.
That is a privacy property, not an optimisation: an always-on microphone that ships every
sound to a server is a different product from one that listens locally and speaks only when
addressed. It also keeps the daemon inside its CPU budget (docs/01 §4.1, 8 GB profile).

**4. ✅ Roadmap reshuffled** — this milestone is M8; hardening is M9. Its exit evidence and
golden 10 move with it.

**5. ✅ Three sub-gates**, as laid out below.

---

## Exit evidence (proposed docs/08 §1 M8 row)

With **no browser open anywhere**: say the wake word in the kitchen and ask a question — it
is answered aloud **in the kitchen**; set a timer by voice and it rings **in the room where
it was set**; an automation fires on its own and reports honestly; every device is paired,
listed and revocable from the UI; and the whole thing starts from a documented install on a
fresh machine.

---

## M8a — hands-free core (F8.1–F8.5)

**Sub-gate exit evidence:** say the wake word at a satellite and be answered aloud *by that
satellite*, with no browser involved and nothing streamed before the word fired.

- [x] **F8.1 — `jarvis-agent --node`: pair, pin, connect (FR-19)** ✅ PR #44 · *strong model*
      The client M7's protocol never got. Generates an Ed25519 keypair, pairs through the
      real `/api/v1/devices/pair` route with a code the owner reads out, **pins the
      `serverFingerprint`** and refuses anything else afterwards, stores its token in the
      OS keyring, reconnects with backoff, and exits clean on revocation. Deletes the
      `JARVIS_AGENT_TOKEN` env-var path — a bearer token in an environment variable was a
      stopgap, and it is the last place a node credential sits in the clear.
      Tests: pairing against a real TLS listener; a changed fingerprint is refused (the
      whole point of pinning); revocation ends the process; the keyring round-trips.
      Refs: ADR-031, docs/05 §6.5, M7's `golden11_node.rs` (the shape to follow).
      Deps: none.

- [ ] **F8.2 — Node audio: capture and playback (FR-13)** · *Sonnet*
      Makes `voice-node` / `room-node` describe something real. `cpal`-backed capture at
      16 kHz mono 16-bit — the format docs/05 §1 already fixes — streamed as binary WS
      frames, and TTS frames played back on the node's output device. Config names the
      input/output devices; a **hardware mute switch is honoured and visible** (a satellite
      whose mic state you cannot see is not one people accept in a kitchen).
      Tests: format negotiation; device absent → the node still runs and says so; mute
      stops frames at the source, not at the server; oversized/malformed frames rejected.
      Refs: docs/05 §1 (binary frames), F7.6's socket-side routing. Deps: F8.1.

- [ ] **F8.3 — Wake word on the node (FR-13 amended, ADR-032)** · *strong model*
      The feature that changes what this product *is*. The engine runs **on the node**;
      audio never leaves the device until the word fires, and the daemon cannot be asked to
      stream continuously. VAD gates end-of-turn as it does today. Includes a visible
      listening state, a configurable sensitivity, and a false-accept budget measured rather
      than asserted. Draft **ADR-032**: engine choice, asset licence review, why detection is
      local, and the swap path.
      Tests: a recorded clip fires once and only once; silence and household noise do not;
      nothing is streamed before detection (asserted at the socket, not in the client);
      detection while speaking triggers barge-in rather than a second turn.
      Refs: docs/02 §9, docs/08 §6. Deps: F8.2.

- [ ] **F8.4 — Echo cancellation and on-device barge-in (FR-13)** · *strong model*
      A satellite with a speaker beside its microphone hears itself; server-side barge-in
      cannot fix that, because the interruption *is* the assistant's own voice. AEC on the
      node, ducking while speaking, and a barge-in path that survives the speaker being
      loud. docs/01 §4 already calls the whole-house profile "room satellites with
      echo-controlled audio" — this is that clause.
      Tests: playback does not self-trigger the wake word; a real interruption during TTS
      still cancels within NFR-04; AEC absent → the node degrades to push-to-talk rather
      than looping. Deps: F8.3.

- [ ] **F8.5 — Room attribution: answer, and ring, where I spoke (FR-13/FR-33)**
      · *strong model*
      The run learns its **origin node**, and everything that speaks goes back there: the
      answer, the clarifying question, the timer that fires, the alarm that was missed while
      the daemon was down. This closes the bug M7 made visible — `timer_alert` plays on the
      daemon host with no device notion, so a timer set in the kitchen rings at the desk.
      Also maps HA areas to room aliases so "turn on the lights" means *this* room.
      Tests: two nodes, each gets only its own answers; a timer set on one rings on it after
      a restart; an unattributed timer (set from the shell) still rings somewhere sensible;
      a revoked node's pending announcements do not resurrect it.
      Refs: FR-33/ADR-023, F7.6, `[display].node_aliases`. Deps: F8.2.

---

## M8b — scheduling (F8.6–F8.7)

**Sub-gate exit evidence:** an automation the owner created fires on its own, is
re-evaluated against policy at fire time, and a run missed while the daemon was down is
announced rather than silently skipped.

- [ ] **F8.6 — Automations: entity, triggers, policy at fire time (FR-17)** · *strong model*
      The requirement docs/05 §1 has advertised routes for since M0 and that has been parked
      twice. `Automation` + `trigger` + `execution` persistence (the `automation` schema is
      already reserved in docs/04 §3), time and HA presence/zone triggers, `GET/POST/PATCH/
      DELETE /api/v1/automations` with creation as an R2 action, and execution history with
      the policy decision recorded. **Policy is re-evaluated at fire time, never cached at
      creation** — an automation is a stored intention, not a stored authorization.
      Tests: a trigger fires once and only once; a disabled automation does not; policy
      denial at fire time is recorded and visible; an automation cannot mint authority its
      creator did not have.
      Refs: docs/02 §11, docs/04 §3, docs/05 §1. Deps: none (assembly over `timers.rs`).

- [ ] **F8.7 — The daemon actually schedules (FR-17, closes D-M4-1)** · *Sonnet*
      M4's `DeferrableScheduler` is a library nothing calls; the only thing that runs on a
      schedule today is the timer sweep. Wire it: quota-window awareness, attempts and
      `not_before`, health gating, and **missed-run announcements on restart** in the same
      shape timers already use. Closes the M4 deviation.
      Tests: work deferred while the provider is unhealthy runs when it recovers; a missed
      window is announced, not silently skipped; restart does not double-run.
      Deps: F8.6.

---

## M8c — the seam that makes it seamless (F8.8–F8.10)

**Sub-gate exit evidence:** a fresh machine reaches a working, hands-free house from a
documented install, administrable from the UI by the person who lives in it — and golden 12
proves it end to end.

- [ ] **F8.8 — Settings surface: devices, automations, voice (FR-19/FR-17)** · *Sonnet*
      Today device management is API-only and pairing a node means curl. The shell gains:
      pair a node (show the code, watch it appear), list devices with class/last-seen/revoke,
      review automations and their history, choose the wake word and audio devices. This is
      also the first slice of M9's policy UI, and it is what makes the house administrable
      by the person who lives in it.
      Tests: the M7 device DTOs render; revoke asks once and takes effect visibly; a node
      appearing mid-pairing updates without a reload; keyboard-first per NFR-11.
      Refs: docs/12, M7's `/api/v1/devices`. Deps: F8.1, F8.6.

- [ ] **F8.9 — It starts on a fresh machine (docs/09)** · *Sonnet*
      Nothing is switched on by default today: the dev config enables server, database, maps
      and observability, and that is all — no voice, no web search, no display profile, no
      integrations. This feature is the deployment story: systemd units for `jarvisd` and
      `jarvis-agent`, Wyoming STT/TTS in compose with the **STT model size finally chosen**
      (open since M5), a real annotated `jarvisd.toml`, TLS certificate generation, and a
      first-run path that ends with a paired shell and a working microphone.
      Tests: a scripted fresh install reaches a healthy daemon and a paired device;
      config validation refuses the half-configured states people actually hit.
      Refs: docs/09, docs/08 §6 (STT size). Deps: F8.2.

- [ ] **F8.11 — ElevenLabs speech synthesis, opt-in (ADR-033)** · *strong model*
      **Added to the list by owner direction 2026-08-13**, superseding "deferred, not
      rejected" in decision 5 above. The deferral's *conditions* stand in full and are this
      feature's acceptance criteria — the owner pulled the timing forward, not the
      safeguards. `SpeechSynthesizer` is a two-method port (`id`, `synthesize`) that only
      `wyoming.rs` implements, so this is an added adapter behind the existing seam, not a
      change to the voice path.
      Must have, all of them: **ADR-033** (new external dependency + a new egress path);
      **opt-in config** as the consent gate, off by default; a **local fallback** so alarms
      and timers still ring with the network down or the quota spent; **sensitivity-aware
      routing** so message bodies and calendar entries are never spoken by a third party
      (`Sensitivity` + `DataEgress::External` already exist — this is routing, not new
      machinery); and a **character budget** with the spend observable.
      Excluded, and these are not owner-timing calls but invariant ones: **not** the wake
      word (must be local and offline — F8.3), **not** STT (voice is the most sensitive
      stream; the zero-LLM paths must work offline), and **never** their Agents platform,
      which takes over the loop and breaks invariants 1–2.
      Tests: opt-in off → Wyoming, no egress; sensitive text → local synthesis even when
      opt-in is on; the adapter unreachable → the alarm still rings locally; budget
      exhausted → falls back rather than failing the turn; the API key is a keyring
      reference and never appears in a prompt, log, or CLI arg (invariant 5).
      Refs: docs/06 §5, ADR-021's spirit, `wyoming.rs` as the shape to follow.
      Deps: F8.2 (node playback), F8.9 (Piper heard first, so the comparison is real).

- [ ] **F8.10 — Golden 12 + M8 acceptance: the house answers** · *Sonnet*
      The exit evidence, executable. **With no browser open:** the wake word fires in the
      kitchen, a question is answered aloud there, a timer set by voice rings in that room,
      an automation fires on its own, and a revoked node goes quiet mid-sentence. Plus the
      **first real NFR-04 measurement** (D-M5-3, open since M5) on the reference hardware,
      because until now nobody has measured the round trip end to end.
      Refs: docs/07 §2, docs/01 §4.1. Deps: F8.1–F8.9, F8.11.

---

## Explicitly out of scope

- **Backup/restore, signed releases, diagnostics bundle, accessibility audit, golden 10** —
  these are M9 (the old M8 row), except the settings surface and deployment story above.
- **Multi-user** — M7 was multi-*device*, single-owner; unchanged.
- **NATS / WebRTC** — still gated on a second *machine* and on networks the owner does not
  control (docs/08 §6, ADR-031).
- **Local reasoning model** — ADR-011 stands.
- **CF-2 audit atomicity** — open since M2; needs a port-signature change (human decision).

## Carried in from earlier gates

| Item | Source | Lands in |
|---|---|---|
| **D-M4-1** — the deferrable-work scheduler has no daemon driver | M4 gate | **F8.7** |
| **D-M5-3** — NFR-04 latency never measured on real hardware | M5 gate | **F8.10** |
| **STT model size undecided** (`base` vs `small` int8) | M5, docs/08 §6 | **F8.9** |
| **Timer alerts have no device notion** — they play on the daemon host | found post-M7 | **F8.5** |
| **`voice-node`/`room-node` describe a device that cannot exist** | found post-M7 | **F8.2** |
| Wake-word engine + licence | docs/08 §6 deferred decision | **F8.3 / ADR-032** |
