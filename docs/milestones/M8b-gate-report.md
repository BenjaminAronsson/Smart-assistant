# M8b gate report — scheduling

**Status: READY FOR SIGN-OFF, with two recorded deviations.**

Prepared 2026-08-15 against `main` (`e69bd83`), covering F8.6–F8.7 and D-M4-1.

---

## 1. Sub-gate exit evidence

> **M8b exit evidence** (docs/milestones/M8-features.md): *an automation the owner created fires
> on its own, is re-evaluated against policy at fire time, and a run missed while the daemon was
> down is announced rather than silently skipped.*

| # | Claim | Result |
|---|---|---|
| 1 | An automation the owner created fires on its own | **PASS** |
| 2 | Policy is re-evaluated **at fire time** | **PASS** |
| 3 | A missed run is announced, not silently skipped | **PASS (with D2)** |

### 1 — It fires on its own (F8.6/F8.7, PRs #51/#52)

`golden12_an_automation_fires_on_its_own_and_records_what_happened` — against real Postgres,
nobody asked: the clock moved past 07:00 and the sweep did the rest. The world is touched
exactly once and the firing is recorded durably, so *"why did the lights come on?"* is
answerable.

Supporting behaviour, all tested: a trigger fires **once and only once** (the same window again
does not re-fire it); a disabled automation does not fire and is not even recorded; a state
trigger fires only for its own entity *and* its own state; a flapping sensor cannot fire twice
inside the refire interval; a clock going backwards does not unlock a refire.

The **midnight-crossing** sweep window is handled explicitly — without it, every automation
between the last evening tick and the first morning one would be skipped every night.

### 2 — Policy re-evaluated at fire time (F8.6)

The design property, and it is **structural rather than conventional**: `Automation` stores
*who* asked (`created_by`) and has **no accessor** for what they were allowed. There is no
scopes column, no cached `PolicyDecision`, no `approved` flag. The runner is therefore *forced*
to resolve authority fresh, through `DeviceAuthority`, from the live device row on every firing.

- `an_automation_cannot_mint_authority_its_creator_lacks` — a creator holding nothing is refused
  for an action that would be allowed to an owner.
- `golden12_a_revoked_nodes_automation_stops_and_says_why` — a revoked creator's automation is
  **denied with a reason and reaches nothing**, and the refusal is durable.
- `an_action_needing_approval_is_refused_and_says_exactly_what_it_wanted` — an R2 action firing
  at 6am has nobody to ask, so it is a recorded refusal carrying the exact effect rather than a
  prompt queued forever, or silence.
- A storage failure resolving authority reads as **no authority** — failing closed, because the
  alternative is acting on a database blip.

Execution history is **append-only at the database**, enforced by trigger:
`execution_history_cannot_be_rewritten` proves Postgres refuses both `UPDATE` and `DELETE`. A
denial that could be edited into a success is not a record — and a denial is the most important
row in that table, because *"it ran and nothing happened"* and *"it was refused"* are
indistinguishable from the sofa.

### 3 — A missed run is announced (D-M4-1, PR #56)

`AutomationService::missed_since` **reports rather than fires**, and the distinction was a real
decision: a missed *timer* must still ring (the owner asked for a noise at a time, and the noise
is the point), but firing *"turn on the lights at 07:00"* at 11:00 because the daemon was off
all morning is **worse** than not firing it, because the reason has passed. Silence is the
option ruled out: an owner returning to a house that did nothing cannot tell *"the automation is
broken"* from *"the daemon was off"*. The announcement names the automation.

**D-M4-1 is closed.** M4's `DeferrableScheduler` had existed since M4 with nothing calling it —
carried through M5, M6 and M7. `jarvisd::deferred::run_worker` now turns it: single-flight,
health- and quota-gated, 120 s idle / 5 s busy so a backlog drains, the lock never held across
the sleep, and prompt shutdown (tested — it fails if the worker waits out its idle interval).

---

## 2. Measurements

| Metric | Result |
|---|---|
| Workspace tests | **1488 pass**, 0 fail |
| `cargo xtask golden` | all traces, **exit 0** (golden 12 included) |
| arch-test | 9 crates, rules hold |
| clippy `-D warnings`, fmt, codegen `--check` | clean |
| `cargo deny check` | advisories/bans/licences/sources ok |
| jarvisd idle RSS | **22.1 MB** (unchanged by the sweep driver) |

The automation sweep ticks **once a minute**, matching `daily_at` resolution — a finer tick
would burn wakeups on an 8 GB target for a trigger that cannot express anything smaller
(docs/09 §5).

---

## 3. Deviations requested

- **D1 — the automations UI is read-and-toggle only.** The settings surface (F8.8) lists
  automations, enables/disables them and shows history, but **creating** one is API-only. The
  domain models creation as an R2 action; no UI drives it yet. Not blocking: the exit evidence
  is about firing, not authoring.
- **D2 — the restart sweep reports nothing in production.** `missed_since` is wired and tested,
  but jarvisd passes `None` because **no last-seen stamp is persisted**. So evidence item 3 is
  proven at the service level and inert at the daemon level. A small persistence change; it
  belongs with the surface that would display it. **This is the one item where the test passes
  and the deployed behaviour does not yet follow.**

## 4. Open risks

- **No subagent review passes** ran on any of M8 (see the M8a report, D4). This diff adds an
  execution path that acts on the world **unattended** — the single best `security-auditor`
  candidate in the milestone.
- Automations can only propose tools that are *registered*; an unregistered tool is denied. That
  is correct, but it means an automation created against a tool that is later unregistered fails
  closed silently apart from its history row. Acceptable, worth knowing.

## 5. Recommendation

**Sign M8b**, accepting D1 and D2 as recorded. The exit evidence is demonstrated: an automation
fires on its own, policy is re-evaluated at fire time from live authority, and a missed run is
reported rather than skipped — with D2 noting the last mile of that third item is not yet wired
into the daemon.
