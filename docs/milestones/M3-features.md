# M3 Artifacts & desktop — feature list

Status: **APPROVED 2026-07-22** (milestone loop, docs/11 §2). Owner approved the
decomposition with three decisions:
1. **Split M3 into M3a + M3b** at the phase boundary. **M3a** (F3a.1–F3a.8) is the
   artifact system, desktop agent, isolated workers, and media — it satisfies **all five
   M3 exit-evidence bullets** and gets its own `/gate`. **M3b** (F3b.1–F3b.9) is the
   docs/12 HUD redesign + deep-dive + personal utilities, gated separately. This keeps the
   "S"-priority HUD (FR-23/24/27) from blocking the "M" exit evidence and keeps per-session
   context honest. *docs/08 §1 records M3 as one milestone; note the split in the M3a gate
   report and `/sync-docs` it back into the roadmap.*
2. **Coding worker is patch-only** (F3a.6): produce a reviewable patch **artifact** in a
   disposable worktree; **applying/deploying a patch is deferred** to a later milestone
   (matches golden 7's "no direct deployment").
3. **Pivot the front face to the docs/12 voice-first HUD now** (F3b.1): the M1/M2
   conversation + approval-tray surfaces move into the ops layer (`Ctrl+.`), not deleted.

M2 signed off 2026-07-22 (`docs/milestones/M2-gate-report.md`, tag intentionally skipped
by owner). **Start with M3a, feature F3a.1.** Do M3a's `/gate` before starting M3b.

Milestone scope (docs/08 §1): Artifact CAS + renderers, display profiles, `jarvis-agent`
+ Hyprland IPC, isolated Playwright worker, MPRIS adapter + media window + media bar
(FR-22), HUD face per docs/12 (card grammar, panel lifecycle FR-24, backgrounds FR-23,
MapLibre/PMTiles map card + out-of-region fallback), deep-dive support (FR-27),
timers/alarms/reminders (FR-33), lists/notes (FR-34).

Exit evidence (docs/08 §1 — **all five land in M3a**): **(1)** create/reopen an artifact
after restart; **(2)** place a canvas on a selected monitor; **(3)** an audited browser
flow; **(4)** pause whatever is playing from the media bar; **(5)** golden 7 (a coding task
creates a patch artifact in a disposable worktree — no direct deployment).

Each feature is a vertical slice sized for one session and runs the `/feature` loop
(spec → threat note → contracts/tests first → implement → review → DoD → small PR).
"Read" names the exact spec sections for that session (token discipline, CLAUDE.md).

**Model discipline (CLAUDE.md §"Model strategy").** Anything touching
`jarvis-domain`/`jarvis-application` (artifact domain types, application ports, deep-dive
router signal, timers/lists domain), the new `jarvis-agent` binary (OS boundary, `unsafe`
justification budget), and the two isolated workers (isolation is a security boundary,
invariants 1/3) is **strong-model** work. The Angular HUD volume (F3b.1–F3b.5) and the
list card (F3b.8) are tightly constrained by docs/12 + generated contracts and **may** run
on Sonnet — owner decides per session. Golden/screenshot plumbing (F3a.8, F3b.9) may be
Sonnet.

**Invariants that bite in M3.** Invariant 1 (text never grants authority) now extends to
**fetched page content in the browser worker** and **model-proposed card content** — the
client renders only *registered* card types, never model-authored HTML/layout on the HUD
face (docs/12 §2.3); the coding worker produces a *reviewable patch artifact*, never a
direct host mutation (golden 7). Invariant 3 keeps the artifact domain types free of
CAS/sqlx. Invariant 6 (append-only audit) covers artifact provenance, browser-step
evidence, and timer firing.

---

# M3a — Artifacts, desktop agent, workers, media (exit evidence)

### Phase A — Artifact system (exit evidence #1)

- [x] **F3a.1 — Artifact domain types + immutable manifest (domain)** · *strong model*
  `jarvis-domain::artifact`: `ArtifactId` + `ArtifactVersion` newtypes, immutable
  `ArtifactManifest` (id, version, creator `RunId`, `Sha256`, media type, `RendererKind`,
  `sources`, `Sensitivity`, build environment, declared `capabilities`), `ArtifactKind`
  (exhaustive: Markdown/HTML, code/text, image, chart, bundle — bundle reserved for M6
  generated apps), provenance value type. No I/O; `#![deny(unsafe_code)]`; newtyped IDs. A
  manifest is immutable once created — a new version is a new manifest, never a mutation
  (test this). Refs: FR-08, docs/02 §6, docs/04 §4, ADR-008. Read: docs/02 §6, docs/04 §4,
  ADR-008; skill `sqlx-data` (manifest shape only). Deps: none. rust-reviewer mandatory.

- [x] **F3a.2 — Artifact CAS blob store + manifest/provenance persistence + ports (infra + application)** · *strong model*
  `jarvis-application::ports`: `ArtifactStore` (create version, get manifest, open blob,
  list versions) + `BlobStore` traits. `jarvis-infra`: content-addressed **file** blob
  store keyed by SHA-256 (write-once, verify-on-read, atomicity: temp-write + fsync +
  rename), Postgres `artifact` schema (manifests + provenance + version chain) behind the
  ports; **manifest + provenance written in one transaction with the audit event**
  (invariant 6). Migration `0010_artifact_init.sql`; `cargo sqlx prepare` committed.
  Restart-safe: reopen a manifest and re-verify the blob hash. Refs: FR-08, NFR-05,
  invariant 6, docs/04 §4, ADR-008. Read: docs/04 §4, docs/06 §7 (CAS integrity); skills
  `sqlx-data`, `low-power`. Deps: F3a.1. contract-keeper (migration) + rust-reviewer mandatory.

- [x] **F3a.3 — Artifact contracts + REST/WS + create/reopen-after-restart E2E (contracts + jarvisd)** · *strong model*
  `jarvis-contracts`: `ArtifactManifestDto` + `ArtifactVersionDto` + `artifact.created`/
  `artifact.updated` WS events; `xtask codegen` → committed TS. jarvisd: `POST/GET
  /api/v1/artifacts`, `GET /artifacts/{id}[/versions]`, blob download with correct media
  type + content-addressed ETag. **Exit-evidence #1 test:** create an artifact, restart
  jarvisd (or drop the pool), reopen by id, blob hash still verifies. Refs: FR-08/09,
  docs/05 §1. Read: docs/05 §1–§4; skills `ws-contracts`, `sqlx-data`. Deps: F3a.2.
  contract-keeper mandatory.

### Phase B — Desktop agent, workers, media (exit evidence #2/#3/#4 + golden 7)

- [x] **F3a.4 — `jarvis-agent` + Hyprland IPC + display profiles: place a surface on a selected monitor (jarvis-agent + jarvisd)** · *strong model*
  Flesh out the `jarvis-agent` stub: connect to Hyprland request/event UNIX sockets, expose
  the **narrow** command set only (list monitors/workspaces/windows, launch an **allowlisted**
  app in app-mode with a stable app-id, focus/move a window, capture an approved screenshot
  region) — **it is not a shell** (docs/02 §8). Display-profile mapping in jarvisd: logical
  surfaces (Conversation, RunTimeline, ApprovalTray, ArtifactCanvas, AmbientStatus,
  Diagnostics) → monitor/workspace. `#![deny(unsafe_code)]` waived only in `jarvis-agent`
  with a justified, tested `unsafe` block if any (CLAUDE.md). **Exit-evidence #2:** place the
  canvas surface on a chosen monitor. Refs: FR-09/10, docs/02 §8/§12. Read: docs/02 §8/§12;
  skill `low-power`. Deps: F3a.3 (a surface to place). security-auditor (OS boundary,
  screenshot/clipboard scope) + rust-reviewer mandatory. **If Hyprland IPC surfaces an
  irreversible protocol/isolation choice, stop and draft an ADR.**

- [ ] **F3a.5 — Isolated Playwright browser worker + typed tool actions + audit evidence (tools + adapters)** · *strong model*
  `tools/browser-worker` (out-of-process, per-trust-domain isolated profiles, visible mode
  for consequential ops, credentials from the secret store — never prompted). Typed actions
  (navigate, extract, click, download, screenshot) with per-step **audit evidence**;
  `jarvis-adapters` host wraps it behind host-owned `ToolPolicy` (worker never self-declares
  safety — same overlay discipline as the MCP host F2.7). Fetched page content is Z4
  untrusted (reuse F2.8 sanitization): **a page cannot inject a tool call** (adversarial
  test). **Exit-evidence #3:** an audited browser flow. Refs: FR-15, docs/02 §8, docs/06
  §5/§6, invariant 1. Read: docs/02 §8, docs/06 §5/§6; skills `policy-grants`, `low-power`.
  Deps: F3a.4 (visible window placement), F2.8 (sanitization). security-auditor + rust-reviewer
  mandatory. **Isolation model may need an ADR — stop and draft if so.**

- [ ] **F3a.6 — Sandboxed coding worker → patch artifact in a disposable worktree (golden 7; PATCH-ONLY) (tools + adapters)** · *strong model*
  `tools/coding-worker`: run a delegated coding task in a **disposable git worktree**,
  producing a **reviewable patch artifact** (F3a.1 `code/text`/diff kind) — **never a direct
  host mutation or deployment** (golden 7, docs/02 §8). **Owner decision: patch-only —
  *applying* a patch is a separate approved action deferred to a later milestone; do not wire
  an apply path in M3.** Resource/time/network limits; worktree torn down after the diff is
  captured. Host adapter registers it with real `ToolPolicy` (producing the patch is R0/R1
  output). Golden 7 scenario itself lands in F3a.8. Refs: FR-15 (coding variant), docs/02 §8,
  docs/07 §2 (7), invariant 1. Read: docs/02 §8, docs/07 §2; skills `policy-grants`,
  `golden-traces`. Deps: F3a.2 (artifact store), F3a.5 (worker-isolation pattern).
  security-auditor + rust-reviewer mandatory.

- [ ] **F3a.7 — MPRIS adapter + `media.playback` + media-bar `media.state` + cast-a-link media window (adapters + web)** · *strong model (web part may be Sonnet)*
  `jarvis-adapters::media_mpris` (zbus, D-Bus session bus): discover
  `org.mpris.MediaPlayer2.*`, expose `media.playback` (play/pause/next/previous/seek/volume
  — transport + volume-within-cap **R1**, volume-above-cap **R2** per docs/02 §11a) and a
  transient `media.state` WS event feeding a minimal **media bar** (docs/12 §5). `media.open_url`
  reuses the dedicated media Chromium window (own app-id/profile, **no credentials**) via
  `jarvis-agent` (F3a.4). **Exit-evidence #4:** pause whatever is playing from the media bar.
  Spotify Web API + `now-playing` query + voice transport are **M5** — not here (docs/08 §1).
  Refs: FR-22, ADR-012, docs/02 §11a, docs/12 §5. Read: docs/02 §11a, ADR-012; skills
  `media-integration`, `policy-grants`. Deps: F3a.4. rust-reviewer + security-auditor (D-Bus
  surface) mandatory.

### Phase C — M3a exit-evidence demonstrator

- [ ] **F3a.8 — Golden 7 + M3a acceptance scenarios (golden)** · *strong model (harness may be Sonnet)*
  Fill golden slot 7 (docs/07 §2): a coding task creates a **patch artifact in a disposable
  worktree; no direct deployment** (drives F3a.6). Add repeatable acceptance scenarios for
  the other exit evidence: artifact create/reopen-after-restart (F3a.3), place-canvas-on-
  monitor (F3a.4 — agent-fake where CI has no Hyprland), audited browser flow (F3a.5 —
  fixture page), media-bar pause (F3a.7 — MPRIS fake). This feature **demonstrates the M3
  exit evidence**; M3a `/gate` follows. Refs: FR-08/10/15/22, docs/07 §2. Read: docs/07 §2;
  skill `golden-traces`. Deps: F3a.1–F3a.7.

---

# M3b — HUD face, deep-dive, personal utilities (docs/12 UX)

Start only after M3a is gated. The HUD pivot (F3b.1) makes the docs/12 face the default and
moves the M1/M2 conversation/approval surfaces into the ops layer (`Ctrl+.`).

### Phase D — HUD face (docs/12; renders M3a's artifacts/media/cards)

- [ ] **F3b.1 — HUD face scaffold: presence orb + caption + materialization canvas + ops-layer toggle + glass token system (web)** · *Sonnet ok*
  Replace the M1 conversation front face with the docs/12 HUD face: presence orb (state
  color **and** motion — accessibility), spoken caption (`aria-live`, per-sentence reveal
  until voice lands in M5), empty materialization canvas, `Ctrl+.`/orb toggle to the **ops
  layer** (reuse the existing Run Spine / timeline / approval surfaces — do not rebuild).
  Glass token system (`--glass-*`, hue tokens) wired (backgrounds in F3b.4).
  Resolution-independent (docs/12 §7); keyboard reachable (docs/12 §8). Refs: FR-09, docs/12
  §1/§2.1/§2.2/§7/§8. Read: docs/12 §1–§2, §7–§8; skill `angular-shell`. Deps: F3a.3 (WS
  client). Accessibility checks per DoD.

- [ ] **F3b.2 — Card grammar v1 + reveal animation + web-sourced-image source chip (web)** · *Sonnet ok*
  Registered HUD card types (docs/12 §2.3): value readout, place, entity/person, media/menu
  grid, **headlines/digest**, now-playing (data only; live in M5), approval (reuse the F2.5
  tray as a card), status/queued, error. (timer/reminder + list + sources/gallery cards land
  with their features F3b.6/F3b.7/F3b.8.) Signature reveal animation (clip-path wipe, corner
  brackets, image light-sweep) with reduced-motion honored. **Every web-sourced image carries
  a visible source-link chip** (docs/12 §2.3, FR-25/ADR-014). **Card grammar only — no
  model-authored HTML on the HUD face** (security property; grep test, docs/12 §9). Refs:
  FR-09, docs/12 §2.3, invariant 1. Read: docs/12 §2.3/§9; skill `angular-shell`. Deps:
  F3b.1. Assert the no-free-form-HTML property in tests.

- [ ] **F3b.3 — Artifact renderers + ArtifactCanvas surface (web)** · *Sonnet ok*
  Render M3a artifacts on the ArtifactCanvas surface: Markdown/HTML (sanitized), code/text,
  image, simple chart (per the `dataviz` skill). Reopen-after-restart visible in the UI
  (composes exit-evidence #1). Bundle/generated-app rendering is **M6 sandbox** — not here.
  Refs: FR-08/09, docs/02 §6, docs/12 §2.3. Read: docs/02 §6, docs/12 §2.3; skills
  `angular-shell`, `dataviz`. Deps: F3a.3, F3b.1.

- [ ] **F3b.4 — Panel lifecycle (FR-24) + backgrounds (FR-23) + glass-contrast audit (web)** · *Sonnet ok*
  Panel lifecycle (docs/12 §4): new query **shelves** current panels (max 4, restorable),
  per-panel + clear-all dismissal, **2-hour TTL** (`[ui] panel_ttl_hours`), **pending
  approvals exempt**. Backgrounds (docs/12 §5): none/abstract/photo with the adaptive
  `--glass-*` token switch; **contrast audit ≥4.5:1 body / ≥3:1 large-caption on both bundled
  worst-case wallpapers** (DoD gate). Motion/power policy (docs/12 §6): ambient motion stops
  when hidden/unfocused/reduced-motion/battery-saver. Refs: FR-23/24, docs/12 §4/§5/§6. Read:
  docs/12 §4–§6; skills `angular-shell`, `low-power`. Deps: F3b.1, F3b.2. Lifecycle +
  approval-exemption + contrast tests are the DoD.

- [ ] **F3b.5 — Map card: MapLibre GL + local PMTiles serve + out-of-region fallback (web + jarvisd)** · *strong (tile-serving) / Sonnet (card)*
  Map card (docs/12 §3, ADR-013): MapLibre GL JS rendering a **PMTiles region extract served
  by jarvisd** (offline, no API key, no tracking); destination pin + current-location dot +
  route polyline + coords/distance/walk-time (tabular-nums); "open large" affordance.
  **Coverage fallback**: outside the extract bbox → online OSM raster (network up) or a
  **coordinates-only card** (offline) — never blank/wrong-region. Dev bootstrap (Leaflet+OSM
  raster) acceptable until the PMTiles pipeline exists; OSM attribution never hidden.
  **PMTiles extract tooling** is a docs/08 §6 decision due now — default: **downloaded
  regional extract** (confirm at feature start; ADR if a self-built pipeline is chosen). Refs:
  FR-25 (map render), ADR-013, docs/12 §3. Read: docs/12 §3, ADR-013; skill `angular-shell`.
  Deps: F3b.2, F2.9 (location coords).

### Phase E — Deep dive & personal utilities

- [ ] **F3b.6 — Deep-dive: thread continuity + sources/gallery cards + source handoff + Research Notes promotion (application + web)** · *strong model*
  `jarvis-application`: a **continuation-vs-new-topic** router signal (same mechanism as F2.9
  location / F2.10 ambiguity) — continuations *extend* the canvas (append cards, no shelve),
  only a genuine topic change shelves (FR-24 unchanged); approvals stay exempt. **Sources
  card** + **gallery card** (each image individually source-badged — provenance differs,
  ADR-017). "Open that / read it" → **browser-worker handoff** (F3a.5), HUD never re-renders
  full page content. **Research Notes artifact promotion** past `[ui] deepdive_promote_after`
  (default 3): offer (spoken) to save the thread as a versioned markdown artifact (F3a.1) —
  accumulated *paraphrased* facts, every source, referenced images. Refs: FR-27, ADR-017,
  docs/12 §2.3/§2.5. Read: docs/12 §2.5, ADR-017; skills `web-lookup`, `angular-shell`. Deps:
  F3a.5, F3b.2, F3b.3, F2.10. rust-reviewer + security-auditor (paraphrase-not-scrape,
  per-item attribution) mandatory.

- [ ] **F3b.7 — Timers / alarms / reminders (FR-33, ADR-023) (domain + application + infra + adapters + web)** · *strong model*
  A dedicated lightweight **timers module**, entirely in the **deterministic grammar (zero
  LLM, offline, degraded-mode-safe)**: set/query/cancel named timers, alarms, one-shot
  reminders. Postgres-persisted (migration `0011_timers_init.sql`; restart-safe). Firing =
  **audible alert on a playback path independent of the TTS pipeline** (sounds even if voice
  is down) + TTS announcement when available + a **live countdown/reminder card** (F3b.2 card
  type); voice dismiss/snooze; **missed alarms announced on restart** with a notice. Boundary
  with FR-17: "make a noise at T" is a timer; anything needing policy re-eval or model
  reasoning at fire time is an automation. Refs: FR-33, ADR-023, docs/02 §11e. Read: docs/02
  §11e, ADR-023; skills `sqlx-data`, `angular-shell`. Deps: F3a.2 (or own schema), F3b.2.
  rust-reviewer + contract-keeper (migration) mandatory.

- [ ] **F3b.8 — Lists & notes (FR-34, ADR-024) + list card + artifact promotion (domain + infra + web)** · *Sonnet ok (strong if domain-heavy)*
  Lightweight **lists/notes store**: named lists (shopping, todo, …) with
  add/remove/check-off/read by **deterministic grammar** where phrasing is clear (LLM assist
  only for ambiguous phrasing); quick notes as single-item captures into a Notes list. **List
  card** (items, check-off by voice or tap; F3b.2). **Promotion to a versioned artifact** when
  a list grows into a document — same pattern as Research Notes (F3b.6). Postgres plain rows,
  exportable; migration `0012_lists_init.sql`. Refs: FR-34, ADR-024, docs/02 §11e. Read:
  docs/02 §11e, ADR-024; skills `sqlx-data`, `angular-shell`. Deps: F3a.2, F3b.2, F3b.6
  (promotion pattern). contract-keeper (migration) mandatory.

### Phase F — M3b exit-evidence demonstrator

- [ ] **F3b.9 — HUD screenshot set + UX acceptance scenarios (web + golden)** · *Sonnet ok*
  Attach the **HUD screenshot set** (idle, listening, speaking+canvas, approval interrupt,
  degraded, each background) + the **contrast audit** on both worst-case wallpapers for owner
  review (docs/12 §9). Add repeatable UX acceptance: panel shelve/restore/dismiss/TTL +
  approval-exemption (F3b.4), continuation-vs-new-topic + gallery per-item attribution +
  Research Notes promotion (F3b.6), timer set/fire/persist/missed-alarm (F3b.7), list
  add/check-off/promote (F3b.8), map offline-from-PMTiles + out-of-region fallback (F3b.5).
  Refs: docs/12 §9, FR-23/24/27/33/34, ADR-013. Read: docs/12 §9, docs/07 §3; skill
  `golden-traces`. Deps: F3b.1–F3b.8.

---

## Dependency sketch

```
M3a:  F3a.1 ─ F3a.2 ─ F3a.3 ─┬─ F3a.4 ─┬─ F3a.5 ─ F3a.6 ─┐
                             │         └─ F3a.7 ──────────┤
                             └──────────────────────── F3a.8   → M3a /gate
M3b:  F3b.1 ─┬─ F3b.2 ─┬─ F3b.3 ────┐
             │         ├─ F3b.4     │
             │         ├─ F3b.5     │
             │         ├─ F3b.7     │
             │         └─ F3b.8     │
             └───────── F3b.6 ──────┴─ F3b.9              → M3b /gate
```
(F3b.5 also depends on F2.9; F3b.6 also depends on F3a.5; F3b.8 depends on F3b.6's
promotion pattern.)

## Explicitly out of scope for M3 (scope control, docs/08 §7)

- **Spotify Web API + `now-playing` query + voice transport (FR-21/32)** — M5. M3 ships the
  universal **MPRIS** control + media bar + cast-a-link window only.
- **Real HA control, CalDAV, SMTP send, memory/embeddings, deterministic HA/math grammar** —
  M4/M5. Timers/lists use their own deterministic grammar; not the HA grammar.
- **Generated-app bundle rendering + capability bridge (FR-18)** — M6. The artifact `bundle`
  kind is reserved in F3a.1 but not executed.
- **Voice pipeline (STT/TTS/VAD), barge-in, wake word (FR-13)** — M5. The HUD caption reveals
  per-sentence in M3; TTS timing-mark sync is wired when voice lands.
- **Applying/deploying a coding patch** — deferred (owner decision, F3a.6 patch-only).
- **Golden 8/9/10** — M6/M5/M8. M3 demonstrates golden 7 only.
- **Distributed rooms / second display node / device pairing (FR-19)** — M7. Display profiles
  here are single-machine, multi-monitor only.

## Resolved decisions (owner, 2026-07-22)

1. **M3 split** into M3a (exit evidence) + M3b (HUD/UX), gated separately. Reflect in the
   roadmap via `/sync-docs` after the M3a gate.
2. **Coding worker patch-only** (F3a.6) — no apply path in M3.
3. **HUD pivot now** (F3b.1) — docs/12 face becomes default; M1/M2 conversation/approval
   surfaces move to the ops layer, not deleted.
4. **PMTiles**: default to a downloaded regional extract (docs/08 §6); confirm/ADR at F3b.5.

## Deviations recorded during implementation (for /gate)

- **D-M3a-1 (F3a.3): artifact surface shipped read-only; `artifact.created` WS event and
  any client create endpoint deferred.** The F3a.3 line named "`artifact.created`/`.updated`
  WS events" and "POST/GET /api/v1/artifacts". Shipped: the read API (`GET …/versions`, `GET
  …/versions/{v}/blob`) + DTOs + codegen + reopen-after-restart E2E. **Not** shipped, by
  design: (a) the `artifact.created` DomainEvent — deferred to **F3a.6**, its first producer,
  per the F2.5→F2.6 no-producer-less-replayable-event precedent (also avoids a premature
  DomainEvent→timeline-snapshot obligation); (b) a client `POST` create — artifacts are run
  outputs produced through the ports, never client-uploaded (stricter security reading of
  invariant 1). Owner: confirm at M3a `/gate`. Blob download added `nosniff` +
  `Content-Disposition: attachment` (served, never rendered inline — the HUD renderer F3b.3
  is the only sanctioned render path; security-auditor B1).
- **CF-M3a-A (F3a.3): blob download buffers the whole blob in memory, no served-size cap.**
  `BlobStore::get -> Vec<u8>` + `Body::from`. Fine for M3a (markdown notes, patches are
  small) but a streaming/size-capped read port is needed before large-artifact producers
  (F3a.6 patches are still small; **M6 `Bundle`** is the real trigger). Verify-on-read
  currently requires buffering to re-hash, so streaming needs chunked-hash-then-emit.
