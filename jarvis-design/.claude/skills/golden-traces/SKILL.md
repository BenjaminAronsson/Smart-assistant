---
name: golden-traces
description: Writing and running golden trace scenarios - executable end-to-end acceptance specs with scripted fakes. Use for /golden, at milestone gates, and when adding scenarios.
---

# Golden traces

Spec: docs/07 §2 - ten scenarios, milestone-mapped, run by
`cargo xtask golden [--scenario N]` against the compose test env (real Postgres,
FakeModel, fake MCP server, fake HA).

1. Structure per scenario (tests/golden/): Arrange (seed config/profiles/fixtures) ->
   Act (drive the PUBLIC API only - REST + WS, never internal calls) -> Assert (WS event
   sequence, DB end-state, audit chain intact, and the trace: span tree contains the
   expected spans).
2. Fakes are scripted per scenario: FakeModel takes a scripted event list (deltas, tool
   proposals, errors, delays); fake MCP a scripted result/malformed result; fake HA a
   scripted state map. Scripts live beside the scenario and read as its storyboard.
3. Determinism: injected Clock port, seeded ULIDs, no sleeps - await expected events
   with bounded-timeout helpers. A flaky golden is a harness bug; fix it before
   anything else.
4. Failure output must be diagnosable: on assertion failure dump the received event
   sequence and the run timeline.
5. Scenario 3 (quota exhaustion -> queue -> recovery) and 10 (restart mid-tool ->
   reconcile, no duplicate mutation) are the hardest and most valuable. Restart is
   simulated by dropping and rebuilding app state against the same DB in-process.
6. Never weaken an assertion to make a scenario pass. If behavior legitimately changed,
   the spec changes first (drift loop), then the scenario.
