# M3a — repeatable acceptance scenarios (F3a.8)

The M3 exit evidence (docs/08 §1) as **runnable** scenarios. Everything in §2 runs
from one command; §3 lists the parts that need real hardware/browsers and how to
verify them by hand. This file is the checklist the M3a `/gate` walks.

## 1. Prerequisites

```bash
docker compose -f infra/compose/dev.yml up -d postgres   # live Postgres for the DB scenarios
export DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis
```

`node` and `git` must be on `PATH` — golden 7 spawns the **real** `tools/coding-worker`
in a real `git worktree`. Both are already project prerequisites (`xtask codegen` needs
node) and both are installed in CI.

## 2. One command

```bash
cargo xtask golden
```

runs golden traces 1–7 plus the four M3a acceptance scenarios, and **fails if a
scenario did not actually run** (a filter that matches nothing is treated as a
failure, not a pass).

| # | Exit evidence (docs/08 §1) | Scenario | Where |
|---|---|---|---|
| 1 | Create/reopen an artifact after restart | `artifact_reopens_through_a_fresh_app_instance` — create via the ports, rebuild the app state on the same DB, `GET` the artifact and its blob; the content address still verifies | `crates/jarvisd/tests/artifacts_api.rs` (+ `artifact_reopens_after_a_simulated_restart` in `crates/jarvis-infra/tests/artifacts.rs` for the persistence half) |
| 2 | Place a canvas on the selected monitor | `open_places_canvas_on_the_requested_monitor` — `POST /api/v1/display/open` resolves the requested monitor from the display profile, audits, and dispatches the placement; unknown/malformed monitors fail closed | `crates/jarvisd/tests/display_api.rs` |
| 3 | Audited browser flow | `every_action_records_append_only_audit` — every typed browser action writes an append-only audit row before its effect; a step that cannot be audited fails closed | `crates/jarvis-adapters/src/browser.rs` (`mod tests`) |
| 4 | Pause whatever is playing from the media bar | `pause_from_the_media_bar_pauses_the_active_player` — the media-bar command resolves the active player and applies `Pause`, audited before it is applied | `crates/jarvisd/tests/media_api.rs` |
| 7 | Golden 7: coding task → patch artifact in a disposable worktree, **no direct deployment** | `golden7_coding_patch.rs` — the real Node worker runs in a real disposable `git worktree` against live Postgres + the CAS: the diff lands as an immutable v1 `CodeText` artifact with its `artifact.created` audit in the same transaction, it reopens through a fresh store, **the source repo keeps its HEAD, a clean tree and no applied file**, the worktree is gone, and a hostile worker that adds `applied`/`deploy`/`tool_call` to its reply changes none of it | `crates/jarvisd/tests/golden7_coding_patch.rs` |

## 3. What CI substitutes, and how to verify it for real

CI has no Hyprland, no browser binaries and no D-Bus session bus, so three
scenarios run against fakes at the seam **below** the OS boundary. The fake is
always the outermost hop; everything the milestone claims (policy, audit,
sanitization, fail-closed behaviour) is exercised for real. To verify the last hop
on a real desktop:

- **#2 canvas placement (F3a.4).** Run `jarvis-agent` under Hyprland with a display
  profile that maps `artifact_canvas` to a real monitor, then
  `POST /api/v1/display/open` for an artifact id. Expect the window on that monitor
  and one `display.open` audit row. With no monitor match the request must fail
  closed rather than pick a screen.
- **#3 browser flow (F3a.5).** Launch `tools/browser-worker` with Playwright
  installed (`npm ci && npx playwright install chromium` in that directory) and run
  a `navigate` + `extract` against a local fixture page. Expect one audit row per
  step and page text that is sanitized/capped — a page that asks for a tool call
  must remain inert text (ADR-027 records container vs process isolation).
- **#4 media pause (F3a.7).** With `[integrations.media] enabled = true` and any
  MPRIS player running (`playerctl` to confirm), press pause in the media bar.
  Expect the player to pause and an audit row written *before* the effect. Volume
  above `max_volume_pct` must ask for approval instead of applying.

## 4. Notes

- Golden 7 deliberately scripts the coding step (`JARVIS_CODING_CMD`) instead of
  calling a model: the trace must be deterministic and quota-free. The isolation,
  worktree disposal, artifact/audit persistence and the no-deployment property are
  all real.
- These scenarios are additive to the M1/M2 traces; `cargo xtask golden` remains the
  single gate entry point.
