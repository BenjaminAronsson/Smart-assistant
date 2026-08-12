// The app's half of the capability bridge (F6.5, docs/06 §6, ADR-030).
//
// The app has NO credential and NO network: its CSP is `default-src 'none'`
// with `connect-src 'none'`, and its origin is opaque. The only thing it can do
// is ask its parent — and asking is all this file does. Every decision is the
// host's: jarvisd re-checks this app's manifest, runs `policy::evaluate`, asks
// the human where the tier demands it, and mints a grant for R2+. A capability
// this app declared is an authorization to ASK, never to act.

const REQUEST = "jarvis.capability.request";
const RESULT = "jarvis.capability.result";

/** How long to wait for a reply before giving up on one request. */
const REPLY_TIMEOUT_MS = 15_000;

let sequence = 0;
const pending = new Map();

window.addEventListener("message", (event) => {
  // Only the parent that opened this frame is talked to. The reply arrives with
  // `origin: null` because the host cannot name an opaque origin (ADR-030), so
  // the guard that matters is the source: nothing else can reach this frame.
  if (event.source !== window.parent) return;
  const data = event.data;
  if (!data || data.type !== RESULT || typeof data.id !== "string") return;
  const resolve = pending.get(data.id);
  if (!resolve) return;
  pending.delete(data.id);
  resolve(data);
});

/**
 * Ask the host for one operation.
 *
 * Resolves to `{ok, content?, code?}` — never rejects, because a refusal is a
 * normal outcome an app must render rather than crash on. `code` is a stable
 * machine code; it is deliberately not a sentence, so an app cannot present the
 * host's words as its own.
 */
export function request(capability, target, value) {
  const id = `r${++sequence}`;
  const message = { type: REQUEST, id, capability, target };
  if (value !== undefined) message.value = value;

  return new Promise((resolve) => {
    pending.set(id, resolve);
    // `'*'` because this frame's own origin is opaque and unnameable; the
    // payload carries nothing secret, only a request the host is free to refuse.
    window.parent.postMessage(message, "*");
    setTimeout(() => {
      if (pending.delete(id)) resolve({ ok: false, code: "app.request_timeout" });
    }, REPLY_TIMEOUT_MS);
  });
}
