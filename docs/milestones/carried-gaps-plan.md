# Carried gaps — plan

A living tracking document for security/product debt carried out of closed milestones
(M2–M10), consolidated here because it was scattered across gate reports and getting
stale in place — see §5. Not a milestone; these are independent items, most small,
none behaviour-blocking today. Priority order below is the recommended pickup order,
not a schedule.

~~**Do not run this alongside M9.**~~ **Lifted 2026-09-01** — M9 is signed off (tag
`m9-complete`), so the behaviour freeze that blocked every item here is over. The
reason it existed: M9 was a refactor whose entire exit evidence was "nothing changes
behaviour", and every item below changes behaviour.

---

## 1. CF-14 — timeout drop has no atomicity, and dormancy is stale (re-verified 2026-08-30)

**This is the one to read first.** The M2 carry-forward doc still says CF-14 is
"advisory (dormant: no mutating tool)". That is no longer true: `SmtpTool`,
`HomeSetLightTool`, `HomeSetAreaLightsTool` and `HomeBroadTool` are all registered
mutating executors (`crates/jarvisd/src/tools.rs`), and every one of them is wrapped
by the same `TimeoutExecutor` (`crates/jarvis-adapters/src/tools/timeout.rs`) whose
own doc comment still says, correctly: *"On elapse, the inner future is dropped
(cancelled) — the same abandon semantics the orchestrator relies on for
cancellation."*

The gap: if a real side effect completes on the far side of the wire (Home Assistant
turns on a light, SMTP hands a message to the relay) but the wrapped future is
dropped before it returns — because the deadline landed in that window, not because
the effect failed — the caller sees `ToolError::Timeout`, not a result. No
`ToolResult.compensation` is ever produced for that firing, because compensation is
built from a *completed* result. The effect happened; nothing downstream knows it did.

HA's own executor already builds `compensation: Some(...)` on success — the
machinery to undo an effect exists — it just never gets a chance to run when the
future is dropped rather than returned.

**Plan:**
1. Confirm which of the four mutating tools can leave a real effect after their own
   transport call returns but before `execute` returns to `TimeoutExecutor` — HA's
   HTTP call and SMTP's send are the two to check first.
2. Decide the fix shape with the owner (docs/11 §3 — this is a port-shape question,
   same as CF-2 below): either (a) `TimeoutExecutor` races the timeout against a
   *side-channel* "effect committed" signal the inner executor can set before its
   own await point completes, so a late-but-real effect is still recorded even
   though the result itself is abandoned; or (b) tools most likely to strand an
   effect get their own internal, shorter, tool-owned timeout that returns a real
   (if degraded) `ToolResult` before `TimeoutExecutor`'s outer deadline can fire —
   cheaper, but only closes tools that adopt it.
3. Either way: a test that starts a mutating tool's transport call, holds it just
   past `TimeoutExecutor`'s deadline, and asserts the effect is either prevented or
   recorded — never silently stranded.
4. Update this doc and `M2-security-carryforward.md`'s CF-14 row together when
   closed.

**Owner decision needed:** which fix shape (2a vs 2b), because it changes a port
signature other tools will be written against.

---

## 2. S3 — every spoken run answer is labelled `Normal` — **CLOSED 2026-09-01**

**Closed** on `fix/s3-speech-sensitivity`. What shipped, and where it differs from the
plan below:

- `SpeechSensitivity` moved to `jarvis-domain::policy` (beside `DataEgress`, which is
  the same kind of statement) and `ToolPolicy` gained a non-defaulted
  `speech_sensitivity` field. `fs.read` and both `message.send` tools declare
  `Sensitive`; the F9.6 policy snapshot in `crates/jarvisd/src/tools.rs` pins all 21
  tools' values so the field can never start being *derived* from `risk`/`egress`.
  The tiers genuinely do not line up: `fs.read` is R0/`DataEgress::None` and
  `Sensitive`.
- **The plan's step 3 was wrong about the code, and the fix is wider than it asked
  for.** There is no mail tool in the registry, and *calendar is not a tool* — an
  agenda reaches the model through `ContextAssembler` as `AssembledContext::agenda`.
  A per-tool field alone would therefore never have fired for "calendar entries",
  which is the one case ADR-033 §4 names by name. So the orchestrator escalates from
  **two** producers: `tool_step` (a tool declaring `Sensitive` returned) and
  `assemble_step` (an agenda was assembled). Any agenda escalates, ignoring each
  event's own `Sensitivity` — which today is a distinction without a difference,
  because the CalDAV adapter parses no `CLASS` property and hardcodes
  `Sensitivity::Sensitive` on every event (`caldav.rs`). The refusal to branch on it
  is aimed at the moment `CLASS` parsing lands: the flag becomes owner-authored,
  almost no real calendar sets it, and a branch would silently start reading every
  unclassified appointment to a vendor. (The first draft of this justified the
  decision by describing the flag as already owner-authored — wrong about this
  codebase, and caught by the security-auditor pass.)
- **Step 1's warning was weighed and not followed.** It anticipated a new `RunUpdate`
  variant would repeat CF-8's mistake. It does not: `run.speech_sensitive` is a
  transient Session event carrying *only* a run id, and it is scoped by exactly the
  machinery F7.4 built — `SPOKEN_RUN_EVENTS` plus per-socket run ownership. It rides
  beside `text.delta`, which is already sending that same socket the entire answer, so
  it reveals strictly less than its neighbour. It also had to ride that stream:
  ordering is the protection, and only one ordered transport can guarantee the label
  arrives before the clause it labels.
- Escalation is monotonic — the socket's `AtomicBool` is only ever set, and there is no
  de-escalation event on the wire — and it is read **per clause** rather than captured
  when the utterance starts, because the label is a consequence of the run and so
  necessarily arrives mid-flight.
- Step 4's mutation check ran, twice: dropping the socket's store, and disabling the
  orchestrator's tool-path emission. Two tests failed each time.
- No transition-table change: this adds a `RunUpdate`, not a `RunState`/`RunEvent`.
- Found and fixed alongside: the web shell's `TRANSIENT_WS_TYPES` sets, which decide
  what advances `lastSeq`. A transient event missing from them reads as a gap in the
  durable sequence and triggers a needless timeline reload on every occurrence. Now
  one exported constant (`web/src/app/ws-events.ts`) rather than two copies that had
  to be edited in lockstep.
- Reviewed by security-auditor, rust-reviewer and contract-keeper; no BLOCKING
  findings. Their SHOULD-FIXes were applied: a test that drives the **real** producer
  (`RunEventSink::emit` → `WsHub`) rather than a hand-built envelope, which is this
  repo's most-repeated bug class; a corrected agenda rationale (see above); the
  re-export dropped so one routing rule does not have two import paths inside one
  crate; a truthful memory-ordering comment (the mpsc send→recv edge is what orders
  the flag, not `Acquire`/`Release`); `speech_sensitivity` surfaced in the F10.5
  policy view; and a registry-coverage assertion so a newly registered tool cannot
  join unpinned — the unpinned direction being `Normal`, i.e. spoken by a vendor.

**Two escalation gaps deliberately left open** (both out of S3's scope, recorded so
they are decisions rather than oversights):

1. **Cross-turn.** Escalation is per-run. If turn 1 uses `fs.read` and turn 2's
   assembled context includes history or a summary quoting that content, turn 2 is
   not escalated and its answer can go out in the vendor voice.
2. **Resume.** `assemble_step` and `tool_step` are the only producers, so a run
   resumed from a checkpoint past both re-emits nothing. Harmless today only because
   a resumed run has no in-flight utterance on a fresh socket — this entry is the
   record that the design leans on that assumption.

**Two owner decisions left open**, both surfaced by the security-auditor:

- **`home.get_state` stays `Normal`.** It returns household state — lock state,
  occupancy, presence. "The back door is unlocked and nobody's home" read out by a
  vendor voice is arguably closer to ADR-033 §4's category than to a weather answer.
  One line plus a snapshot expectation to flip.
- **Memory retrieval is a third unlabelled producer** (`jarvisd/src/orchestrator_ports.rs`).
  Retrieved memories are folded into the prompt and can be quoted verbatim, with no
  escalation. The memory path *does* rely on a per-item `Sensitivity` flag — the kind
  of flag the agenda decision declines to trust — but there the label is owner-authored
  at write time and the retrieval path already drops `Sensitive` items, which is a real
  distinction from calendar's. Escalating on any retrieval hit would fire on most runs
  and hollow the label out. The asymmetry needs a sentence in ADR-033 §4 stating it is
  a decision.

Original entry follows.

---

Open since M8a, restated at every gate since (M8c, M10). `SpeechSensitivity::Sensitive`
is real routing machinery (ADR-033 §4) — content marked sensitive never reaches
ElevenLabs, whatever the config says. Nothing marks anything sensitive. A run that
reads a message or calendar entry aloud is spoken by the same path as an ordinary
answer.

**Why it's still open:** the label has to be set by the *producer* of the spoken
text (ADR-033 §4 — inferring sensitivity from content fails open, silently, for
whoever's messages happen not to look private), and the socket contract has no
signal to carry it. `RunUpdate` (`crates/jarvis-contracts`, consumed by
`jarvisd/src/runs.rs` and `jarvisd/src/ws.rs`) carries no tool-activity variant —
there's no way for the orchestrator to say "this answer used mail.read" by the time
it reaches the synthesizer.

**Plan:**
1. Add a tool-activity signal to the orchestrator's answer path — likely a field on
   whatever produces the spoken text, not a new `RunUpdate` variant broadcast to
   every socket (that would be a second copy of CF-8's mistake: information that
   should be scoped, sent unscoped). Needs a transition-table test per
   `state-machine` skill conventions.
2. A per-tool `speech_sensitivity` classification, alongside `risk`/`egress` at each
   tool's declaration site (same place F9.6's `declare_tool!` macro is explicitly
   forbidden from defaulting risk — this is the same kind of field, same rule:
   written out, not inherited).
3. Wire: mail/calendar-reading tools (and anything else that surfaces third-party
   content) mark `Sensitive`; the answer synthesis path reads it and routes
   accordingly.
4. Test: a run using a mail tool never reaches the ElevenLabs adapter, mutation-
   checked (remove the routing check, confirm the test fails).

**Mitigation already in place:** ElevenLabs is opt-in and off by default (ADR-033
§2), so this is not exposed unless the owner has explicitly turned it on.

**Owner decision needed:** none structurally blocking — this is an application-layer
change with a clear shape. Land after F9.6 if done alongside M9 (same tool-metadata
surface), otherwise any time.

---

## 3. CF-2 — audit-sink atomicity, the last quarter

Grant lifecycle and durability are done (`PgAuditSink`, hash-chain persisted, wired
into the live `ToolPlane` since F2.6 Slice 3b). What's left: `AuditSink::record` is
still `record(&self, event) -> ()` — no transaction handle, no error channel — so a
crash between a tool's side effect and the sink's own commit can still leave an
unaudited effect (invariant 6, partially held).

**Plan:** thread the caller's transaction through the port — `record(&self, event,
tx: &mut Transaction) -> Result<(), AuditError>` or equivalent. This is explicitly
flagged in the carry-forward doc as "a domain/application port change, human-decision
territory" — every port consumer (grant path, R0/R1 tool path, orchestrator-emitted
events) needs to acquire a transaction it doesn't hold today at several call sites.

**Owner decision needed:** yes — this is a port signature change touching
`jarvis-application::ports`, which CLAUDE.md's human-only list names explicitly ("new
domain/application dependencies" is adjacent; a port signature change of this shape
should get the same review). Scope it as its own feature with a spec before touching
code.

---

## 4. M8b D1 — automations are created API-only

Recorded as non-blocking at the M8b gate ("the exit evidence is about firing, not
authoring"). The settings surface (F8.8) lists, enables/disables and shows history;
creating one is still API-only. F10.5 built the policy view (read-only, matches
`policy::evaluate`) — the natural next surface for automation authoring, since both
are "show what the system is allowed to do and let the owner change it."

**Plan:** a feature-sized UI addition on top of F10.5's surface. Not urgent — no
security or correctness gap, purely an authoring-ergonomics gap. Pick up whenever
the settings surface gets its next pass.

**Owner decision needed:** none. Ordinary feature work.

---

## 5. Housekeeping already done this pass (2026-08-30)

- **CF-8 doc row fixed.** Closed in F7.4 (PR #33, M7) with a class × event table
  and a mutation-checked test; the `M2-security-carryforward.md` row still read
  "SHOULD-FIX (dormant: single-user)" a milestone later. Fixed in this same change.
- **Two items from M8a's gate report were re-verified and found already closed —
  removed from tracking, not carried into this doc:**
  - `jarvis-agent` SIGTERM handling: real handler exists
    (`crates/jarvis-agent/src/main.rs`), with a documented fallback to Ctrl-C-only
    when the signal can't be installed. The code's own comment credits the M8
    rust-reviewer pass with finding the gap between an old comment's claim and
    what the code did.
  - Automations executing with an uncancellable token: `RegistryExecutor::execute`
    now takes and honours a real `CancellationToken` threaded from `main.rs`'s
    `serve_shutdown`, bounded by `FIRE_TIMEOUT`
    (`crates/jarvisd/src/automations.rs`, `crates/jarvis-application/src/automations.rs`).

  Lesson for next time this doc is updated: **grep the code before carrying an item
  forward.** Both of these were fixed without their originating gate report, or
  anything else, being told.

## 6. Not re-opening — genuinely dormant or deliberately deferred

- **CF-10** (`fs.read` TOCTOU/hardlink holes) — re-checked: still no `fs.write` tool
  registered anywhere in the tree. Dormancy condition still holds. Re-check at the
  first fs-write tool, as originally scheduled.
- **AEC active cost** (~9.3–5% of a core while speaking, depending on measurement
  pass) — not a defect, a cost. Revisit only if a satellite proves too slow.
- **Dark theme** — deferred by design (`docs/08` §6), not a gap.
