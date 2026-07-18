---
name: state-machine
description: How to safely modify the Jarvis run orchestrator - RunState transitions, budgets, checkpoints, cancellation. Use whenever touching jarvis-application::orchestrator or the RunState enum.
---

# Modifying the run state machine

Spec: docs/02 §4, ADR-003. The loop is code; the model only proposes.

## Rules
1. `RunState` lives in `jarvis-domain`; the transition function is a pure
   `fn next(state, event) -> Result<RunState, TransitionError>` with an exhaustive match.
   Side effects live in step executors in `jarvis-application`, never in the transition fn.
2. **Every change starts in the transition-table test** (`jarvis-domain/tests/transitions.rs`):
   the table enumerates every (state, event) pair as Allowed(next) or Rejected. Add rows
   first, watch them fail, then change the enum/fn.
3. New states need: entry action, exit condition, checkpoint decision (safe boundary per
   NFR-05 - checkpoint wherever restart would otherwise lose a committed external
   effect), and a WS event classification (persisted domain event vs transient).
4. Budgets are checked at the top of the loop, never inside steps. New budget dimensions
   extend `RunBudget` + the budget test table.
5. Cancellation: `cancel.check()?` at loop top; every step executor takes the token and
   must abort promptly - process-backed steps (CLI, tools) kill + reap; streams drop.
   Add a cancellation test per new step: cancel mid-step, assert terminal `Cancelled`,
   assert no orphan process (see provider-adapter skill).
6. Terminal states are idempotent: re-entering commit logic is a no-op (guard with the
   checkpoint's recorded outcome).
7. Recovery: on restart, runs load from the last checkpoint; a step with an external
   idempotency key reconciles (query external state) instead of re-executing blindly.
   Every new side-effecting step documents its reconcile behavior.

## Definition of done for a state-machine change
Transition table green with new rows · cancellation test · restart-recovery test if a
checkpoint moved · docs/02 §4 table updated (doc-syncer checks) · no `_` arm anywhere.
