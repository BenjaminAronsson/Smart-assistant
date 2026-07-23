#!/usr/bin/env node
// Jarvis browser worker (F3a.5, docs/02 §8, docs/06 §5, ADR-027).
//
// An out-of-process Playwright driver. It is DUMB and UNTRUSTED by design: the
// Rust host (`jarvis-adapters::browser`) owns the tool catalogue and every
// ToolPolicy; this worker only executes the single typed action the host sends
// and reports what happened. It declares no tools, no safety, and no policy —
// the host ignores any field here it does not model, so nothing this process
// emits can introduce a tool call (invariant #1).
//
// Protocol (line-delimited JSON over stdio, one exchange per line):
//   host → worker:  {"step":<u64>,"action":"navigate|extract|click|download|screenshot",
//                    "url":<string?>,"selector":<string?>}
//   worker → host:  {"ok":<bool>,"content":<string?>,"final_url":<string?>,"error":<string?>}
//
// Isolation (ADR-027): in production this process runs inside a per-trust-domain
// container; in dev/CI it runs as a plain process with an isolated profile
// directory. Both are configured by the host at launch. Credentials, if any,
// arrive as environment variables the host injected from the secret store — this
// worker never prompts for them and never logs them (invariant #5).
//
// Config via environment (host-set, never argv):
//   JARVIS_BROWSER_PROFILE_DIR   isolated user-data-dir for this trust domain
//   JARVIS_BROWSER_HEADLESS      "0" for visible mode (consequential ops), else headless
//   JARVIS_BROWSER_NAV_TIMEOUT_MS navigation timeout (default 15000)

import { chromium } from "playwright";
import readline from "node:readline";

const PROFILE_DIR = process.env.JARVIS_BROWSER_PROFILE_DIR || "";
const HEADLESS = process.env.JARVIS_BROWSER_HEADLESS !== "0";
const NAV_TIMEOUT_MS = Number(process.env.JARVIS_BROWSER_NAV_TIMEOUT_MS || "15000");
// Bound what we ever hand back; the host caps again, but do not stream megabytes.
const MAX_EXTRACT_CHARS = 24 * 1024;

let context = null;
let page = null;

async function ensurePage() {
  if (page) return page;
  // A persistent context bound to the isolated profile directory keeps cookies /
  // storage per trust domain (ADR-027). An empty profile dir → ephemeral.
  context = await chromium.launchPersistentContext(PROFILE_DIR, {
    headless: HEADLESS,
    // No downloads to arbitrary host paths unless a download action asks; keep
    // the default download dir inside the profile.
    acceptDownloads: true,
  });
  page = context.pages()[0] || (await context.newPage());
  page.setDefaultTimeout(NAV_TIMEOUT_MS);
  return page;
}

// Reply for one action. Only ever these fields; the host reads no others.
function reply(fields) {
  process.stdout.write(JSON.stringify(fields) + "\n");
}

function fail(error) {
  // Keep error text short and free of anything sensitive; the host sanitizes it
  // again before it can reach a log.
  reply({ ok: false, content: null, final_url: null, error: String(error).slice(0, 400) });
}

async function handle(req) {
  const p = await ensurePage();
  switch (req.action) {
    case "navigate": {
      await p.goto(req.url, { waitUntil: "domcontentloaded" });
      reply({ ok: true, content: `navigated`, final_url: p.url(), error: null });
      return;
    }
    case "extract": {
      // innerText of the body — the host treats this as untrusted (Z4) data.
      const text = (await p.evaluate(() => document.body?.innerText || "")).slice(
        0,
        MAX_EXTRACT_CHARS,
      );
      reply({ ok: true, content: text, final_url: p.url(), error: null });
      return;
    }
    case "click": {
      await p.click(req.selector);
      reply({ ok: true, content: `clicked`, final_url: p.url(), error: null });
      return;
    }
    case "download": {
      const [download] = await Promise.all([
        p.waitForEvent("download"),
        p.goto(req.url, { waitUntil: "commit" }).catch(() => {}),
      ]);
      const path = await download.path();
      reply({ ok: true, content: path || "", final_url: p.url(), error: null });
      return;
    }
    case "screenshot": {
      const dir = PROFILE_DIR || ".";
      const file = `${dir}/screenshot-${req.step}.png`;
      await p.screenshot({ path: file });
      reply({ ok: true, content: file, final_url: p.url(), error: null });
      return;
    }
    default:
      // Unknown action: the host never sends one (its catalogue is closed), but
      // fail closed rather than guess.
      fail(`unknown action`);
  }
}

async function main() {
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let req;
    try {
      req = JSON.parse(trimmed);
    } catch {
      fail("malformed request");
      continue;
    }
    try {
      await handle(req);
    } catch (e) {
      fail(e?.message || e);
    }
  }
  if (context) await context.close();
}

main().catch((e) => {
  fail(e?.message || e);
  process.exit(1);
});
