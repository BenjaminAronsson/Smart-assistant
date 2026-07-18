---
description: Full security audit of a diff, branch, or the whole tree against the six invariants
---
Invoke the security-auditor subagent on: $ARGUMENTS (default: diff against main).

Additionally run the mechanical checks yourself: `cargo xtask arch-test`; grep for
executor calls not preceded by policy::evaluate on the path; grep tracing/log statements
near secret-typed fields; `cargo deny check` and `cargo audit`.

Merge the auditor's findings with the mechanical results into one report:
BLOCKING / SHOULD-FIX / NIT with file:line and minimal fixes. If the review was triggered
by a release or gate, also walk docs/06 §8 gates explicitly and state pass/fail each.
