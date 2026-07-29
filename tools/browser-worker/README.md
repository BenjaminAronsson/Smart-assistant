# jarvis-browser-worker

Out-of-process **Playwright** browser worker (F3a.5, docs/02 §8, docs/06 §5, ADR-027).

The Rust host `jarvis-adapters::browser` launches this worker and drives it with a
line-delimited JSON protocol over stdio. **The host owns the tool catalogue and every
`ToolPolicy`; this worker only executes the one typed action it is told to and reports
what happened.** It declares no tools and no safety — the host ignores any field it does
not model, so nothing this process emits can introduce a tool call (invariant #1). Every
page-derived string is treated as untrusted (Z4) by the host and sanitized before it
reaches a log, span, or the model.

## Protocol

One request line in, one response line out.

```
host → worker:  {"step": 7, "action": "navigate", "url": "https://example.org"}
worker → host:  {"ok": true, "content": "navigated", "final_url": "https://example.org/", "error": null}
```

Actions: `navigate` (needs `url`), `extract`, `click` (needs `selector`), `download`
(needs `url`), `screenshot`. The host validates argument shape and rejects non-`http(s)`
URLs **before** sending, so `javascript:`/`file:`/`data:` never reach here.

## Isolation (ADR-027)

- **Production:** run inside a per-trust-domain container (read-only mounts default,
  CPU/mem/time/net limits — docs/02 §12). The container is host/ops configuration.
- **Dev/CI:** run as a plain process with an isolated Playwright profile directory
  (`JARVIS_BROWSER_PROFILE_DIR`). CI uses a **fake** worker (no browser binaries); real
  Playwright here is manual-verify.

Both honour the same stdio protocol, so the host code path is identical.

## Config (environment, host-set — never argv)

| Var | Meaning |
|-----|---------|
| `JARVIS_BROWSER_PROFILE_DIR` | Isolated user-data-dir for this trust domain |
| `JARVIS_BROWSER_HEADLESS` | `0` for visible mode (consequential ops), else headless |
| `JARVIS_BROWSER_NAV_TIMEOUT_MS` | Navigation timeout (default 15000) |

**Credentials** arrive as host-injected environment variables (resolved from the secret
store at the jarvisd boundary). This worker never prompts for them and never logs them
(invariant #5).

## Run

```bash
npm ci
npx playwright install chromium   # once, on a real (non-CI) host
JARVIS_BROWSER_PROFILE_DIR=/tmp/jarvis-profile-default node src/index.mjs
```

Then feed it JSON lines on stdin. This package is **not** built or tested in CI (no
browser binaries); the host's Rust tests exercise the protocol and every security
property against a fake worker.
