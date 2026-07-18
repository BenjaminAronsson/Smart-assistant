---
description: Run golden trace scenarios and fix failures without weakening assertions
---
Run `cargo xtask golden $ARGUMENTS` (all scenarios if no argument).

For each failure: dump and read the received event sequence + run timeline; classify the
cause — (a) implementation bug: fix via /feature-style inner loop; (b) harness
flakiness: fix the harness (clock/seq/timeout helpers), flakiness is a bug;
(c) legitimate behavior change: STOP — the spec section changes first via /sync-docs or
/adr, then the scenario, with human approval.

Never weaken an assertion to get green. Report: scenarios run, pass/fail, causes, fixes
applied. (golden-traces skill applies.)
