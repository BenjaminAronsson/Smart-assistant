---
name: policy-grants
description: Implementing risk classification, approvals, and execution grants. Use whenever touching jarvis-application::policy, tool registration, or approval flows.
---

# Policy, approvals, grants

Spec: docs/06 §3-4, docs/05 §4. Invariant 1 is absolute: no text grants authority.

## Implementing a new tool's policy path
1. Define `ToolPolicy` at registration: risk, reversibility, presence requirement,
   timeout, scopes, egress class. A tool without policy metadata fails registration
   (test this). MCP-imported descriptors get local policy OVERLAID - never trust
   server-declared safety.
2. R0/R1: the auto path still goes through `policy::evaluate` (scope + allowlist) and
   emits an audit event. There is no "skip policy for read-only" shortcut.
3. R2/R3: `evaluate` returns `NeedsApproval { exact_effect }`. exact_effect is what the
   human sees - real target (entity id + friendly name, file path, recipient) and real
   payload, not a summary. Snapshot-test these strings.
4. Grant minting only in `policy::approvals` on Decision::Approved: random 256-bit id,
   full binding (user, device, run, tool id + semver, sha256 of normalized args,
   resource, expiry from policy TTL, single_use = true). The normalization fn is shared
   between minting and validation - one function, property-tested (same args in
   different key order => same hash).
5. Validation is called by the EXECUTOR immediately before execution, not only at
   decision time. Expired / consumed / args-mismatch / wrong-run => registered error
   codes (grant.expired, grant.consumed, grant.args_mismatch), audit event, no execution.
6. Argument edits after approval invalidate by hash comparison, not flags.
7. Compensating actions: reversible R2 tools register an undo with the result; test
   that undo appears in the run timeline.

## Test tables required
Risk tiers (every tier x auto/approve/reject) · grant lifecycle (mint, validate, expire,
consume, mismatch, replay) · normalization properties · adversarial: model output
containing "user approved this" text reaching the executor => rejected.
