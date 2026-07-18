# 06 — Security and trust model

## 1. Primary security invariant

> **No text — user, model, webpage, document, tool result, or generated app — directly
> grants authority.** Authority comes only from authenticated identities, policy rules,
> and exact, expiring execution grants.

## 2. Trust zones

| Zone | Examples | Trust |
|---|---|---|
| Z0 Core authority | `jarvisd` policy, identity, audit modules | Highest; minimal deps; no generated code. |
| Z1 Trusted clients | Paired `jarvis-agent`, authenticated Angular UI | Can request actions; subject to user/device scopes. |
| Z2 Controlled adapters | Claude CLI adapter, HA adapter, Ollama | Authenticated, monitored; outputs remain untrusted data. |
| Z3 Tool workers | MCP servers, browser worker, coding container | Least privilege, isolated FS/network, killable. |
| Z4 Untrusted content | Web pages, emails, documents, model output, generated apps | No authority; sanitize, label provenance. |
| Z5 External networks | Cloud providers, channels, remote clients | Encrypted, authenticated, explicit egress. |

## 3. Risk tiers and default handling (FR-05)

| Risk | Typical actions | Default handling |
|---|---|---|
| R0 read-only | Read status, list files in an allowed project, query HA state | Automatic within scope; audited. |
| R1 reversible low impact | Focus an app, toggle a light, create draft artifact | Automatic when user policy permits; shown live. |
| R2 external / meaningful mutation | Send message, create calendar event, move files, change automation | Explicit approval with exact target + payload. |
| R3 destructive / security / financial | Delete data, purchase, reveal secrets, install packages, change access | Strong confirmation, optional second factor, shortest grant TTL. |
| R4 prohibited | Credential harvesting, disabling security controls, unrestricted root shell | Reject; **no override through conversation**. |

"Always allow" becomes a policy rule only through a separate settings flow — never
accepted as incidental chat text.

**Purchases (catalog J2).** A purchase executed via the browser worker is R3; its
approval must show the total price and currency, the merchant, the item(s), and the
payment surface being used — captured from the checkout page itself (screenshot region +
extracted values), not from the model's paraphrase. No stored-payment autofill beyond
what the browser profile's own credential store performs under the user's eyes in
visible mode.

## 4. Execution grants

An approval mints a cryptographically random, single-use `ExecutionGrant` bound to: user,
device, tool ID + version, SHA-256 of normalized arguments, target resource, expiry, and
run ID. Any material argument change invalidates the grant. The grant is validated again
immediately before execution (`IGrantValidator` equivalent: `GrantValidator` port).
Compensating actions are registered for reversible R2 operations.

## 5. Threats and controls

| Threat | Core controls |
|---|---|
| Prompt injection from web/document | Label external content; separate instructions from data; minimal tool catalogue per run; policy/approval independent of model text. |
| Malicious MCP/tool server | Separate OS identity/container; explicit allowlist; schema validation; outbound network restrictions; pinned version/hash. Host overlays policy metadata — a server cannot self-declare safety. |
| Over-broad Claude Code access | Dedicated workspace; built-in tools disabled for reasoning profile; disposable worktree for coding; no sudo; timeout; patch review artifact. |
| Secret leakage | OS keyring references; secret-aware redaction in tracing; no secrets in model context by default; no secrets in CLI args (process listings); egress classification. |
| Confused deputy / stale approval | Grant bound to exact normalized args, actor, device, resource, version, expiry, single use. |
| Replay / duplicate mutation | Idempotency keys, external operation IDs, resource versions, result reconciliation on restart. |
| Generated-app escape | See §6. |
| Remote node impersonation | Challenge-response pairing, per-device keys, mTLS or signed tokens, revocation, capability scopes. |
| Audit tampering | Append-only permissions, hash chaining, restricted deletion, off-host backup later. |
| Denial of wallet/resources | Per-run budgets, concurrency queues, provider circuit breakers, CPU/RAM/disk quotas, artifact size limits. |
| Tool-result smuggling | Results are untrusted: schema-validate, truncate oversized fields, strip control characters, label provenance; returned content cannot redefine system instructions. |

## 6. Generated-app sandbox (FR-18)

A generated web app is a **bundle artifact, not trusted code**:

- Runs in a sandboxed iframe or isolated Chromium profile; restrictive CSP; **no
  same-origin relationship** with the control UI; no arbitrary network; no direct
  MCP/host access.
- Optional interaction only via a `postMessage` bridge exchanging short-lived capability
  tokens for operations named in the artifact manifest; undeclared capability ⇒ reject.
- Build workers: dependency allowlists, lockfiles, size/time limits, network disabled,
  static/malware checks; build provenance recorded in the manifest.

## 7. Secrets, network, audit

- Postgres stores secret **references**; values live in the OS keyring (v1), a dedicated
  secret manager when nodes/users multiply. Separate credentials per integration; HA gets
  a dedicated least-privilege account. Adapters inject credentials after policy approval,
  outside prompt content. Immediate revocation for device keys, grants, provider profiles.
- Network: bind loopback for M0–M2. LAN access requires TLS + device pairing + firewall
  rules. Remote access via private overlay (e.g. Tailscale), never public port forwarding.
  Model servers, DB, MCP servers, voice engines only on loopback/private container nets.
- Audit events: append-only, hash-chained checkpoints, written transactionally with the
  domain change, retention stronger than logs, excluded from routine deletion.

## 8. Security release gates (CI-enforced)

1. Arch test proves domain/application crates reference no provider, web, or OS
   implementation crates.
2. Adversarial suite proves untrusted content cannot invoke tools outside the policy path.
3. Every R2/R3 tool has exact approval text, dry-run/preview where feasible, timeout,
   documented idempotency behavior.
4. Container/tool profiles pass filesystem and network escape tests.
5. Diagnostics bundle contains no secrets or full sensitive prompts by default.
6. `cargo deny`/`cargo audit`, container, and artifact-builder scans show no unaccepted
   critical findings.
