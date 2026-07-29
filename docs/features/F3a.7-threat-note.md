# F3a.7 Threat Note — MPRIS media control, `media.state` bar, cast-a-link window

Governing spec: **FR-22** (docs/01 §3), **docs/02 §11a**, **ADR-012** (Accepted),
docs/12 §2.3 (now-playing card grammar), docs/06 §3 (risk tiers). Exit evidence #4
(docs/08 §1): *pause whatever is playing from the media bar*.

Quoted requirements this feature must satisfy:

> **FR-22 (S)** — "Cast/open YouTube and generic web video in a dedicated media window on
> a chosen display; universal local playback control (play/pause/next/volume) for whatever
> is playing via MPRIS."

> **docs/02 §11a** — "A `media_mpris` adapter (zbus, D-Bus session bus) discovers
> `org.mpris.MediaPlayer2.*` players and exposes `media.playback`
> (play/pause/next/previous/seek/volume) plus a `media.state` transient WS event feeding
> the media bar … Risk tiers: transport control and volume-within-cap R1;
> playlist/library mutation R2; volume above cap requires approval (hearing protection is
> a real reversibility question). Media tools are registered in the standard catalogue —
> no special path."

> **docs/02 §11a (cast-a-link)** — "`media.open_url` launches/reuses the dedicated media
> Chromium window (own app-id, own profile, no credentials) on a chosen display via
> `jarvis-agent`; from there MPRIS provides transport control."

Out of scope by the approved M3 feature list: Spotify Web API, the `now-playing` **query**
(FR-32), and voice transport — all M5.

## Trust zones touched (docs/06 §2)

| Boundary | Zone | Notes |
|---|---|---|
| D-Bus **session** bus (`org.mpris.MediaPlayer2.*`) | **Z3** — local, semi-trusted process boundary | Any process on the user's session can own an MPRIS name and publish arbitrary `Metadata`. Player-published strings (title/artist/album/`mpris:artUrl`) are **untrusted content**, not instructions. |
| Media Chromium window (cast-a-link) | **Z3** process / **Z4** page content | Own app-id, own profile directory, **no credentials**, separate from browser-worker profiles. Page content never re-enters the model through this path. |
| Media bar (web shell) → jarvisd REST | Z1 authenticated device | Owner-driven action, bearer-authenticated, audited. |

## Risk tiers (docs/06 §3)

- **R1** — transport (`play`, `pause`, `play_pause`, `next`, `previous`, `stop`, `seek`)
  and `set_volume` **at or below** the configured cap. Reversible, local-only egress,
  auto-authorized within scope `media:control`.
- **R2** — `set_volume` **above** the cap (`media.volume_boost`). Approval + execution
  grant required: sudden loudness is not meaningfully reversible (hearing protection).
- Not built here: playlist/library mutation (Spotify, M5) — no R2 media-library surface
  exists in this feature.

## Threats and controls

1. **Player-published metadata as an injection vector (invariant 1, Z3→model).**
   A malicious/compromised session process can register an MPRIS name and set
   `xesam:title` to instruction-shaped text. Control: metadata is Z4-treated —
   length-capped, control characters stripped, never interpreted; it flows to the
   media bar as *data* and to a tool result as labelled content. No tool call can be
   produced by it: the only path to execution remains `policy::evaluate` + a registered
   executor.
2. **Hostile `mpris:artUrl` (SSRF / local file exfiltration).** A player may publish
   `file:///etc/shadow` or an internal URL as album art. Control: art URLs are scheme-
   filtered to `https`/`http`/`file` **and dropped unless `https`** before crossing the
   wire; the shell never fetches non-https art. jarvisd never fetches art itself.
3. **Volume-cap bypass (hearing protection).** Control: the cap is enforced in one
   domain function (`VolumePct::within_cap`) used by **both** the R1 tool executor and
   the owner-driven REST endpoint, so no path can diverge. Above-cap through the R1 tool
   fails closed with a message naming the R2 tool; above-cap through REST is rejected
   (the media bar cannot exceed the cap at all).
4. **Player-name injection into D-Bus calls.** A bus name is used to address a method
   call. Control: `PlayerId` is a validated newtype (`org.mpris.MediaPlayer2.` prefix,
   bounded length, no control characters, D-Bus name charset only). Parsing happens at
   the boundary; raw strings never reach the call site.
5. **Cast-a-link as an arbitrary-launch primitive (OS boundary, invariant 1).**
   `media.open_url` must not become "run any program with any argument". Controls:
   scheme is **https only**; the URL is a single-line token with no control characters;
   the agent launches a **fixed, allowlisted** browser command with a fixed app-id
   (`jarvis.media`) and a dedicated profile directory — the URL is passed as one argv
   element, never through a shell; the agent refuses any other app-id (existing
   `SURFACE_APP_PREFIX` discipline from F3a.4).
6. **Unbounded background work / battery drain (NFR perf, docs/09 §5).** Control: state
   is **event-driven** (`PropertiesChanged` + `NameOwnerChanged` subscriptions), never
   polled; one tracked task with a `CancellationToken`; the subscription is not started
   when `[media] enabled = false` or no session bus exists.
7. **Denial by absent/vanishing player.** A player appearing or disappearing mid-run must
   not error the run. Control: "no active player" is a successful, empty result
   (`MediaSnapshot::none`), and a `ServiceUnknown`/`NameHasNoOwner` D-Bus reply maps to
   the same clean outcome rather than an execution failure.

## User-visible failure states

- **No session bus / `[media] enabled = false`** → media surface reports "media control
  unavailable"; the media bar is absent. jarvisd starts normally.
- **No player running** → media bar absent (or "nothing playing"); tool returns a clean
  "no active player" result.
- **Player vanished between snapshot and command** → `409`-equivalent clean failure with
  "that player is no longer running"; state re-broadcast.
- **Above-cap volume from the bar** → rejected with the cap named; the model path is told
  to propose `media.volume_boost` (approval card).
- **Agent disconnected on cast-a-link** → the placement/launch is audited but reported as
  not dispatched (same `dispatched: false` semantics as F3a.4).

## Audit (invariant 6)

Every owner-driven media command writes an append-only audit event **before** dispatch
(`media.command`, actor `device:<id>`, target `player:<bus name>`, payload = verb +
requested level). Cast-a-link records the URL **verbatim** (docs/02 §11a: "The URL is
shown verbatim in the R1 audit event"). Tool-path executions are audited by the existing
tool-execution audit path — no new bypass.
