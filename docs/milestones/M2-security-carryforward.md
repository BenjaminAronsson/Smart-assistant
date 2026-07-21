# M2 security-review carry-forward

Advisory findings from the security-auditor pass on F2.1+F2.2 (no BLOCKING
issues; the policy-engine core was confirmed sound). Each is dormant while
`jarvisd` wires `tools: None`, but MUST be addressed in the feature that first
activates the relevant path. Tracked here so a later session sees them.

| # | Finding | Fix in | Severity |
|---|---------|--------|----------|
| CF-1 | `Float` scalar in `canonical_form` was terminator-delimited, not length-prefixed. | **DONE in F2.2** (length-prefixed; test added). | advisory |
| CF-2 | `AuditSink::record` is fire-and-forget — cannot be written in the same transaction as the run's domain change (invariant #6, docs/06 §7). The port signature must let the audit event commit atomically with the checkpoint/outbox write, or be reworked so F2.4 can. | **F2.4** | SHOULD-FIX |
| CF-3 | Tool-result smuggling (docs/06 §5): `ToolResult.content` is folded verbatim into the next model prompt with no schema-validation, size-truncation, or control-char stripping. The `truncated` flag implies an upstream validator that does not yet exist. A result validator MUST enforce this — in the orchestrator/domain, not trusted per-executor — before a real `ToolExecutor` lands. | **F2.6** (native tools) and **F2.8** (web.fetch Z4) | SHOULD-FIX before any executor |
| CF-4 | Tool *execution* + result are not audited, only the policy *decision*. docs/06 §7 treats the side effect as the audited unit. Add an execution/result audit event when a real executor lands. | **F2.6** | advisory |
| CF-5 | `DataEgress` is defined but `evaluate` never consults it. External-egress tools must be gated before they are registered. | **F2.8/F2.9** | advisory |
