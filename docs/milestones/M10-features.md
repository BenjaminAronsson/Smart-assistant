# M10 "Product hardening" — feature list

Status: **APPROVED — owner sign-off 2026-08-19**, list and order as written. Decomposed the same
day on Opus 5, after M8 finished in code (PRs #44–#68) and M9 was deliberately skipped.
**F10.1 is the current feature.** The owner will install and test once it is done.

Two of the three human-only questions below are still open and do not block F10.1: whether
D-M4-1 is scheduled or dropped, and whether M8's sub-gates are signed now or on the evidence
F10.1 produces.

**Ordered for one stated goal: "a working product I can test."** That reorders the roadmap row.
Signed releases and a diagnostics bundle are worth nothing to an owner who cannot yet run the
thing in their own house, so F10.1 is *use it*, and the release machinery comes after.

## Why this milestone, and why M9 was skipped

The owner's call, 2026-08-19: M9 "Load-bearing" is a pure refactor with **no user-facing
change** — 3,700-line files are untidy, not broken. A house that cannot be backed up or upgraded
is not finished. M9's feature list stays on file and unapproved; nothing here depends on it.

What M8 actually left behind is worth stating plainly, because F10.1 is shaped around it:

- **The whole system has never run together.** M8 proved its parts: the daemon runs and is
  healthy, the wake engine detects on recorded speech, a node pairs and pins, the settings
  surface renders, NFR-04 measures at 433 ms. **Nobody has said the wake word to a satellite
  and been answered aloud by it.** That is M8a's headline exit evidence and it is still open.
- Two M8 gate reports are **not signed**, and one blocked item (D-M4-1) was reopened after a
  review pass found the claim of closure was false.
- The M8 review passes left four items recorded rather than fixed (below).

## Exit evidence (proposed docs/08 §1 M10 row)

The owner installs on a machine that has never had Jarvis, talks to it hands-free, breaks it on
purpose, restores it from a backup, upgrades it, and rolls the upgrade back — following written
instructions the whole way, with no source tree and no help from the person who built it.

---

## The features

- [ ] **F10.1 — It runs, and you can talk to it (M8a/M8c exit evidence)** · *strong model*
      The one that makes this real. Bring the whole stack up on one machine — Postgres, Wyoming
      STT/TTS, `jarvisd`, the web shell, and a **paired `jarvis-agent` node with the wake engine
      compiled in** — and have a person say "hey jarvis" in a room and be answered aloud *there*.
      Then fix whatever that exposes, because nothing has ever exercised the seams between those
      five things at once.
      Known to be untested end to end today: the agent has never been paired against a live
      daemon outside the test suite; the wake engine has never scored microphone audio (only
      recorded WAVs); the shell has never been served by `jarvisd` and driven by a human; and the
      answer path (F8.5, PR #62) has never carried a real answer to a real node.
      **Produces M8's outstanding exit evidence as a by-product**, which is the point — it is
      cheaper to finish M8 by using the product than by writing more tests about it.
      Tests: a scripted end-to-end that pairs a node, fires the wake word from a recorded clip
      through a real capture path, and asserts audio comes back on that node's socket.
      Refs: `docs/milestones/M8a-gate-report.md` §1.4/§1.6, `M8c-gate-report.md` §2.
      Deps: none — everything it needs is merged.

- [ ] **F10.2 — Backup, restore, and a restore that is actually tested (FR-30, docs/09)**
      · *strong model*
      A house whose Postgres is one disk failure from gone is not a product. `pg_dump` is the
      easy half; the half that matters is **restoring into a clean database and proving the house
      still works** — timers still fire, devices are still paired, automations still hold their
      creator, artifacts still resolve to their blobs.
      The artifact CAS is the trap: blobs live on the filesystem and manifests live in Postgres,
      so a backup of one without the other restores a database full of dangling references.
      Tests: backup → drop → restore → the golden 12 assertions still pass against the restored
      database; a restore with the blob store missing **fails loudly** rather than half-working.
      Deps: F10.1.

- [ ] **F10.3 — Update and rollback, repeatably (docs/09)** · *strong model*
      Upgrading must not be an act of faith. Forward migrations already run on start; what is
      missing is the other direction and the story around it: what happens to a paired node when
      the daemon's contract version moves, what an operator does when an upgrade goes wrong, and
      how they know which state they are in.
      **Rollback is the hard part and the honest answer may be "restore from backup"** — if so,
      say that in writing rather than implying a `down` migration that does not exist.
      Tests: upgrade across a migration with live data and paired devices; the documented
      rollback path, executed.
      Deps: F10.2 (rollback leans on restore).

- [ ] **F10.4 — Diagnostics bundle (NFR-07, docs/09)** · *Sonnet*
      One command that produces something an owner can read — or send — when the house
      misbehaves: versions, migration state, adapter health, recent audit *shapes*, the last
      errors, resource figures. **Redaction is the feature, not a caveat**: no secrets, no
      message bodies, no transcripts, no tool arguments. A bundle nobody dares share is useless.
      Tests: the bundle contains the diagnostic fields; a seeded secret, transcript and message
      body appear **nowhere** in it.
      Deps: F10.1.

- [ ] **F10.5 — Policy UI: see and change what is allowed (FR-05, docs/12)** · *Sonnet*
      Risk tiers, scopes and per-tool policy are config-only today. The settings surface gained
      devices, automations and voice in M8c; this adds the one an owner most needs to *see*:
      what each tool may do, what needs approval, and what a given device class is allowed.
      **Read-first.** Changing policy from a web page is a bigger authority question than
      changing a wake word, and F8.8's consent-gate amendment is the precedent for how carefully
      that has to be argued. Propose write access in an ADR; ship the view regardless.
      Tests: the rendered policy matches `policy::evaluate`'s actual decisions for the same
      inputs — a UI that *describes* different rules than the engine enforces is worse than none.
      Deps: F10.1.

- [ ] **F10.6 — Accessibility pass (NFR-11, docs/12 §8)** · *Sonnet*
      Keyboard-first was built in from M3b and spot-checked per surface; nothing has audited it
      whole. Screen-reader labelling of the presence states, focus order across the HUD and
      settings, reduced-motion honoured everywhere, contrast in both the glass and photo
      backgrounds, and the voice surfaces' non-voice equivalents (NFR-11 requires them).
      Tests: an automated axe pass plus a keyboard-only walkthrough of every route.
      Deps: F10.5 (audit the surface once it is complete).

- [ ] **F10.7 — Signed releases and the security checklist (docs/06, NFR-14)** · *strong model*
      Reproducible release artifacts, signed; the checklist in docs/06 run and recorded; the
      `cargo deny` advisory posture made durable rather than incidental.
      **Note from M8:** the advisory check is *time-dependent* — RUSTSEC-2026-0258 turned a green
      pipeline red with no code change. A release process has to state how old an advisory scan
      may be at sign-off.
      Tests: a release build verifies its own signature; the checklist has no unchecked box.
      Deps: F10.3.

- [ ] **F10.8 — Golden 10 + M10 acceptance** · *Sonnet*
      The exit evidence, executable where it can be: install, talk, break, restore, upgrade, roll
      back. Some of it is inherently a human at a machine — golden 10 covers the parts a script
      can hold, and the acceptance document names the rest honestly.
      Deps: F10.1–F10.7.

---

## Carried in — decide where each lands

| Item | Source | Note |
|---|---|---|
| **D-M4-1** — deferrable work has no driver *and* no handler | M4 gate, reopened 2026-08-18 | M8 wrote `run_worker` and never spawned it. Needs **real deferrable work to exist** (M4's deferred summarization) before a driver means anything. Its own slice, or explicitly dropped. |
| **S3** — every spoken answer is labelled `Normal` | M8 security audit | A run that reads a message aloud can reach ElevenLabs. Needs a tool-activity signal from the orchestrator — an application-layer change with a transition-table test. |
| **M8b D1** — automations are created API-only | M8b gate | Natural fit with F10.5's policy surface. |
| **AEC active cost** ~9.3% of a core while speaking | M8 perf pass | Scalar 2048-tap NLMS. Only paid while speaking now; revisit only if a satellite proves too slow. |
| **NFR-04 / false-accept on reference hardware** | D-M5-3, ADR-032 | Harnesses exist; both are a re-run on the 8 GB machine, naturally part of F10.1. |
| Dark theme | docs/08 §6 | Still deferred; not M10. |

## Explicitly out of scope

- **Multi-user.** Still single-owner, multi-device.
- **NATS / WebRTC**, **local reasoning model** — unchanged (ADR-011, ADR-031).
- **M9's refactor.** Skipped by owner decision; the list stays on file. If a god-module actively
  obstructs an M10 feature, split *that* file in that feature's PR rather than reviving M9.

## Human-only decisions this list needs (docs/11 §3)

1. **Approve this list and its order.** In particular: F10.1 before everything, and shipping
   F10.5 read-only rather than blocking on the write-access ADR.
2. **D-M4-1: schedule or drop it.** It has been carried since M4 and nothing has ever needed it.
   Dropping it deliberately is a better outcome than carrying it a fifth time.
3. **Whether M8's three sub-gates are signed before M10 starts**, or M8a/M8c sign off on the
   evidence F10.1 produces.
