# M3b — repeatable UX acceptance (F3b.9)

The docs/12 §9 acceptance checklist for HUD work, made **repeatable**. §2 runs from one
command with no browser and no model quota. §3 lists what genuinely needs a browser,
with the exact commands, and is explicit about what is **not done**. This file is the
checklist the M3b `/gate` walks; it follows `M3a-acceptance.md`.

docs/12 §9, verbatim:

> Keyboard-only walkthrough · amber-exclusivity grep · card grammar only (no free-form
> model HTML) · lifecycle behaviors (shelve/restore/dismiss/2h TTL/approval exemption)
> tested · both wallpapers pass contrast audit · reduced-motion + hidden-window CPU
> checks pass · every web-sourced image on a card shows its source link · map renders
> offline from local PMTiles in the M-gate demo · screenshot set (idle, listening,
> speaking+canvas, approval interrupt, degraded, each background) attached to the PR for
> owner review.

## 1. Prerequisites

```bash
docker compose -f infra/compose/dev.yml up -d postgres
export DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis
```

`node` and `git` must be on `PATH` (already required by `xtask codegen` and golden 7).
Nothing in §2 needs a browser, a display server, a network, or a model provider — every
M3b scenario is deterministic grammar plus stored state (ADR-023, ADR-024).

## 2. One command

```bash
cargo xtask golden
```

runs the M1/M2 traces, golden 7, the four M3a acceptance scenarios, **and** the M3b set
below. A filter that matches nothing is a failure, not a pass — a renamed test cannot
silently turn a gate scenario into a no-op.

### 2.1 Per-feature UX acceptance

| Feature | What the scenario proves | Where |
|---|---|---|
| **F3b.4** panel lifecycle | Server half only: a canvas routing decision has no way to express retracting a pending approval (`CanvasAction` is exhaustive and two-armed), and the 2 h `panel_ttl_hours` is a documented, validated default rather than a constant buried in a component. **The shelve/restore/dismiss/TTL behaviours themselves are client state — see §3.** | `the_canvas_action_never_speaks_to_approvals` (`crates/jarvis-application/src/deepdive_tests.rs`); `the_ui_section_defaults_to_the_documented_values` (`crates/jarvisd/src/config.rs`) |
| **F3b.5** offline map | A tile inside coverage is served **from the local PMTiles archive** with the right type/encoding and a strong ETag — no network anywhere in the path; a tile outside the bounding box is **refused, never approximated** with a neighbouring region; an empty square inside coverage is `204`, never a neighbour's tile painted in its place | `in_region_tile_is_served_with_type_encoding_nosniff_and_a_strong_etag`, `a_tile_outside_the_bounding_box_is_refused_not_approximated`, `an_empty_square_inside_coverage_is_no_content_not_a_neighbour` (`crates/jarvisd/tests/map_api.rs`) |
| **F3b.6** deep dive | One narrative: a follow-up (and "open the second one") **extends** the canvas and never shelves what the human is reading; only a genuine topic change **shelves**, handing the old thread back rather than dropping it; each gallery tile carries **its own** source URL, domain chip and alt text, from its own page; the promotion offer is spoken once at the threshold as a single line; promoting writes a versioned Research Notes artifact to the **real CAS + Postgres**, a second promotion appends **v2 to the same document** instead of minting a rival, provenance keeps every source page, and it all reopens through a fresh store | `f3b6_a_follow_up_extends_the_canvas_a_new_topic_shelves_it_and_a_thread_promotes_to_one_growing_document` (`crates/jarvisd/tests/m3b_acceptance.rs`) |
| **F3b.7** timers | Set → nothing due, scheduler sleeps exactly to the moment → **the daemon is dropped and rebuilt against the same database** → the timer is still armed and now overdue → it fires flagged **missed**, sounds the tone, and the spoken line *says* it was missed → it rings **exactly once** across a second sweep → `timer.fired` outbox row + audit row + state change committed together → dismiss retires it | `f3b7_a_timer_set_before_a_restart_rings_as_a_missed_alarm_after_it` (`crates/jarvisd/tests/m3b_acceptance.rs`) |
| **F3b.8** lists | The list is created implicitly on first use, items keep the order they were spoken in, a check-off addresses **exactly one line by id** (not a clear-all), promotion writes `# Shopping` / `- [x] milk` / `- [ ] eggs` to the real CAS as v1, a later promotion appends **v2 to the same document** (a different fresh artifact id is deliberately ignored), and it reopens after a restart with every step in the audit chain | `f3b8_a_list_item_is_added_checked_off_and_promoted_to_one_versioned_document` (`crates/jarvisd/tests/m3b_acceptance.rs`) |

### 2.2 The docs/12 §9 checklist items that mechanise

| §9 item | How it is checked, headlessly | Where |
|---|---|---|
| **card grammar only (no free-form model HTML)** | (a) The set of card types the Angular switch narrows on is compared against the **contract's own JSON Schema** — a `HudCardDto` variant without a renderer, or a renderer for a type the contract does not register, fails. (b) No markup sink (`innerHTML`, `outerHTML`, `insertAdjacentHTML`, `bypassSecurityTrust*`, `document.write`, `createContextualFragment`, `new Function(`) appears anywhere under `web/src/app/hud`, comments stripped so prose about a sink is not mistaken for one — and the stripper itself is tested so it cannot silently swallow the thing it is looking for | `the_client_renders_exactly_the_registered_card_types`, `the_hud_face_contains_no_markup_sink`, `the_comment_stripper_cannot_swallow_a_sink` (`crates/xtask/tests/hud_acceptance.rs`) |
| **every web-sourced image on a card shows its source link** | Three layers. **Wire:** a walk over every serialized card variant requires anything image-shaped to carry non-empty `sourceUrl`, `sourceDomain` and `alt`. **Producers:** the same walk over what jarvisd's deep-dive projections actually emit. **Templates:** exactly two HUD templates may contain an `<img>` — the attributed `sourced-image` component (which must mount its chip) and now-playing album art, which is the player's own content and owes no chip | `every_web_sourced_image_on_any_card_carries_its_source_link` (`crates/jarvis-contracts/tests/cards.rs`); `every_web_sourced_image_a_producer_can_emit_carries_its_source_link` (`crates/jarvisd/tests/m3b_acceptance.rs`); `every_image_on_a_hud_card_goes_through_the_attributed_component` (`crates/xtask/tests/hud_acceptance.rs`) |
| **amber-exclusivity grep** | `--c-wait` may be referenced on exactly two HUD surfaces — the presence hue registry (bound to `waiting`, and only `waiting`) and the ringing-timer card, both of which are asking for a human decision. Any third use fails | `amber_is_reserved_for_surfaces_that_want_a_human_decision` (`crates/xtask/tests/hud_acceptance.rs`) |
| **both wallpapers pass contrast audit** | Arithmetic, not eyeballs: the glass panel is composited over each bundled wallpaper's **worst-case pixel** and measured against WCAG AA. The tokens are **parsed out of `backgrounds.ts` and `styles.scss`**, not restated, so a token change moves the audit with it or breaks the parse. Current margins: `bright-haze` body 16.36:1, secondary 10.33:1; `deep-dusk` body 7.97:1, secondary 5.04:1 (AA needs 4.5:1). The browser suite runs the same maths in `contrast.spec.ts` | `both_bundled_wallpapers_pass_the_wcag_aa_contrast_audit` (`crates/xtask/tests/hud_acceptance.rs`) |
| **map renders offline from local PMTiles** | §2.1, F3b.5 row | `crates/jarvisd/tests/map_api.rs` |

## 3. What needs a browser — and its exact procedure

Four §9 items and the F3b.4 lifecycle behaviours are properties of a *rendered* page.
They live in the Angular suite and cannot be substituted for by anything in §2. A grep
can prove a sink is absent; it cannot prove a surface is usable.

**These specs already run on every PR.** `.github/workflows/ci.yml` sets
`CHROME_BIN=/usr/bin/google-chrome` and runs
`npm test -- --browsers=ChromeHeadlessNoSandbox`, so §3.2's table is *covered* — it is
simply not runnable on a host with no browser binary. The genuinely outstanding item is
§3.3, which is a review artifact rather than a test.

### 3.1 Prerequisite: a browser binary

```bash
export PATH=/home/agent/.local/node/bin:$PATH        # Angular CLI needs node >= 22.22.3
export CHROME_BIN=$(command -v chromium || command -v google-chrome)
cd web && npm ci
```

`karma.conf.js` already ships the headless launcher this needs
(`ChromeHeadlessNoSandbox`, `--no-sandbox --disable-gpu`); `CHROME_BIN` is the only
missing input. On a host without one, Karma fails with
`No binary for Chrome browser on your platform`.

### 3.2 Run the browser-gated suite

```bash
cd web
node node_modules/.bin/ng test --browsers=ChromeHeadlessNoSandbox --watch=false
```

(Invoke `ng` directly rather than `npm run test`, which re-resolves node from `PATH`.)

| §9 item | Spec that runs it |
|---|---|
| **lifecycle behaviors** (shelve / restore / dismiss / 2 h TTL / **approval exemption**) | `web/src/app/hud/panel-lifecycle.spec.ts` — shelve on a new topic and restore; restoring is a *swap*, not a replace; at most four shelved panels, oldest dropped; a pending approval is **never** shelved and survives clear-all; per-card and per-chip dismiss; silent expiry at the 2 h default with approvals exempt; a configured `panel_ttl_hours` honoured; a nonsense TTL rejected rather than expiring everything instantly |
| **keyboard-only walkthrough** | `presence-orb.spec.ts` (the orb is a real focusable `<button>` with an `aria-label`, not a click-only div), plus the card/approval/list specs' keyboard paths |
| **reduced-motion** | `hud-state.service.spec.ts` / `hud.spec.ts` — the ambient-motion gate is off under `prefers-reduced-motion` |
| **hidden-window CPU** | `hud-state.service.spec.ts` — ambient motion stops when the window is inactive. The *measured* CPU number is a manual observation (§3.4) |
| **contrast audit** (browser copy) | `contrast.spec.ts` — same maths as §2.2, kept as the in-browser cross-check |
| **map out-of-region fallback, client half** | `map-card.spec.ts`, `map-geo.spec.ts`, `map-gl-view.spec.ts` — never blank, never the wrong region: outside coverage the card degrades to online raster or a coordinates-only readout rather than an empty tile grid |

### 3.3 The screenshot set — **NOT DONE**

docs/12 §9 requires a screenshot set attached to the PR for owner review:
**idle, listening, speaking + canvas, approval interrupt, degraded, and each background**
(`none`, `abstract`, `photo`) — nine frames.

**This was not produced.** There is no browser binary on the build host and no way to
obtain one: Karma reports `No binary for Chrome browser on your platform`, and
`storage.googleapis.com`, `cdn.playwright.dev` and `download.mozilla.org` are all
blocked by network policy (HTTP 403); apt offers only snap-transitional stubs. No
substitute was invented — a hand-drawn or synthesized frame would be worse than an
absent one, because the owner would be reviewing something that is not the product.

Procedure once a browser exists:

```bash
export PATH=/home/agent/.local/node/bin:$PATH
export CHROME_BIN=$(command -v chromium || command -v google-chrome)
cd web && npm ci
node node_modules/.bin/ng serve --port 4200 &
```

Then, with `jarvisd` running and a paired device, capture at 1920×1080:

| # | Frame | How to reach it |
|---|---|---|
| 1 | Idle | Load the HUD; no run active |
| 2 | Listening | Start a voice turn (or set presence `listening` from the ops layer) |
| 3 | Speaking + canvas | Ask a question that materializes cards; capture mid-caption with the canvas populated |
| 4 | Approval interrupt | Trigger an R2 proposal; capture the amber approval card with the urgency pulse |
| 5 | Degraded | Exhaust or stub the provider so the run queues; capture the degraded orb and status card |
| 6–8 | Each background | Repeat frame 3 with `[ui] background` set to `none`, `abstract`, `photo` |

Attach all frames to the PR. This item stays **open** on the M3b gate until then.

### 3.4 The *visual* contrast check — partially done

The **numeric** audit is done and automated (§2.2): both worst-case wallpapers pass WCAG
AA with margin, headlessly, on every `cargo xtask golden`.

What is **not** done is the visual confirmation over the real rendered wallpapers —
that the composited numbers match what a human sees, including the scrim and backdrop
blur, which the arithmetic models but does not render. That needs the same browser as
§3.3. Once available:

```bash
cd web
node node_modules/.bin/ng test --browsers=ChromeHeadlessNoSandbox --watch=false \
  --include='**/contrast.spec.ts'
```

then eyeball frames 6–8 of the screenshot set for text legibility over each wallpaper.

### 3.5 Hidden-window CPU measurement

With the HUD open and the window hidden or unfocused for 60 s, `top -p $(pgrep -f
chromium)` (or the browser's own task manager) must show the tab effectively idle. The
gate for the *behaviour* is automated (ambient motion stops); the **number** is a manual
observation on the reference machine (docs/09 §5).

## 4. Standing caveats

- **F3b.6 has no runtime path yet.** `DeepDiveService` is constructed nowhere in
  `jarvisd::run` — the deep-dive scenario in §2.1 therefore drives the use case plus
  jarvisd's card projection directly, which is the highest seam that exists. When the
  service is wired into the run loop, the scenario should be re-pointed at the HTTP/WS
  surface; the assertions do not change.
- **Doubles are used only below the OS boundary.** The timer scenario records the tone
  and the spoken line instead of emitting them (there is no audio device and no TTS
  pipeline before M5). Everything above that — state, audit, outbox, the missed flag,
  the announcement *text* — is real.
- **F3b.4 is client state by design.** Nothing server-side can shelve or restore a
  panel, so §2.1's F3b.4 row is deliberately narrow. The behaviours are covered, in
  `panel-lifecycle.spec.ts`, and are browser-gated.
- **One amber use sits outside the audited scope.** `web/src/app/artifacts/artifact-canvas.scss`
  paints the "sensitive" label with `--c-wait`. The artifact canvas is not the HUD face,
  so the §9 amber grep does not cover it, but it is a warning colour rather than a
  request for a decision — flagged for owner review rather than silently allowlisted.
- These scenarios are additive to the M1/M2/M3a traces; `cargo xtask golden` remains the
  single gate entry point.
