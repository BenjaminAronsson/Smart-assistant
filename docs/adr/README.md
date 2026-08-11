# Architecture Decision Records

Format: context → decision → consequences. ADRs win over prose docs on conflict.
Status of all records: **Accepted (v2 baseline, 17 July 2026)** unless noted.

---

## ADR-001 — Rust core, replacing the v1 .NET decision {#adr-001}

**Context.** v1 chose .NET because it matched the owner's skill set. v2 adds assumption
A-08: Claude Code performs most implementation under human review. The binding constraint
shifts from human writing speed to *machine-checkable correctness*, plus an always-on
daemon footprint target (NFR-15).

**Decision.** Implement `jarvisd`, `jarvis-agent`, native tools, and `xtask` in Rust
(tokio/axum/sqlx/rmcp). Keep Angular for the shell. Allow Python/Node only out of process
(voice engines, third-party MCP servers, Playwright worker).

**Consequences.**
- (+) Exhaustive enums make illegal run-state and risk-tier transitions unrepresentable;
  ownership rules eliminate whole classes of agent-introduced concurrency bugs; single
  static binaries; <100 MB RSS achievable; official MCP Rust SDK and `hyprland-rs` fit
  the two most unusual subsystems.
- (−) Owner must learn to *review* Rust; compile times; no SignalR (superseded by the
  plain versioned WS protocol in `05` §3); EF replaced by sqlx migrations.
- Revisit trigger: if review friction dominates after M2, the fallback is not a rewrite —
  the ports/contracts design is language-portable — but no fallback is planned.

## ADR-002 — Modular monolith for v1 {#adr-002}

**Decision.** One `jarvisd` deployable composed of internal modules with explicit ports;
high-risk integrations and non-Rust engines out of process. **Reasoning.** Solo developer:
one debugger, one deployment unit, local transactions. Premature microservices multiply
config, versioning, and failure modes. Future seams (voice, model workers, tool workers,
browser, nodes) are already process boundaries. **Consequences.** Crate boundaries + arch
tests enforce module isolation; the shared database is not an excuse for cross-module
table access.

## ADR-003 — Deterministic orchestration, probabilistic planning {#adr-003}

**Decision.** The model returns structured proposals, tool calls, and text. A coded state
machine (`RunState`, `02` §4) controls lifecycle, budgets, approvals, retries,
cancellation, commit. **Reasoning.** "Agent loops until it feels done" is unsecurable and
untestable. The explicit loop makes every transition observable, caps replanning, supports
recovery, and enables deterministic tests with fake model/tool outputs.

## ADR-004 — Claude Code CLI is a transitional adapter, not the platform contract {#adr-004}

**Context.** Anthropic documents CLI auth via Console/API billing *or* eligible Claude
subscriptions, and documents non-interactive `-p` mode with JSON/stream-JSON output; app
subscriptions and API billing are separate products. **Decision.** Use the CLI behind a
`ModelProvider` port to unblock personal v1. General-reasoning profile: `claude -p` in a
controlled workdir, stream-JSON, built-in tools disabled, timeout + cancellation. Coding
profile: disposable worktree/container, explicit tool allowlist, reviewable patch
artifact. **Not allowed:** token extraction, undocumented session impersonation, assuming
unlimited availability, direct home-directory edits, unattended sudo, coupling domain
messages to CLI-specific events. **Consequences.** Replaceable by an Anthropic API adapter
or local profiles without touching domain logic; CLI health failures route to fallback.

## ADR-005 — Capability-aware routing; no model self-selection {#adr-005}

**Decision.** Provider profiles advertise capabilities (streaming, tool calling,
structured output, modalities, limits, locality/sensitivity classes, measured
latency/quality/cost), and deterministic code routes among eligible profiles. Local-cloud
collaboration is a routing policy with measurable fallback behavior, not a binary choice.
Never silently switch providers when sensitivity policy forbids. (Validated by the
OpenJarvis research findings on local/cloud quality gaps and tuned collaboration.)

## ADR-006 — Home Assistant is the home system of record {#adr-006}

**Decision.** Jarvis never manages Zigbee/Z-Wave/Matter/vendor clouds or discovery. It
reads state and invokes allowlisted services/intents via HA with a dedicated token. HA
automations remain deterministic and survive Jarvis downtime. Jarvis adds NL planning,
cross-domain context, presentation — not device-protocol ownership.

## ADR-007 — Voice is a pipeline of Wyoming services; push-to-talk first {#adr-007}

**Decision.** PTT before wake word (avoids false wake-ups, attribution, always-listening
concerns). VAD/STT/TTS/wake run as Wyoming-compatible out-of-process services so engines
(Silero, faster-whisper, whisper.cpp, Piper, openWakeWord) swap without core changes and
HA room satellites become the scale-out path. Code and *model asset* licenses are reviewed
independently, per asset.

## ADR-008 — PostgreSQL + pgvector + content-addressed artifact store {#adr-008}

**Decision.** Postgres for transactional state, JSON payloads, audit, embeddings
(pgvector); SHA-256-keyed CAS for blobs/bundles. SQLite only for throwaway spikes.
**Reasoning.** Concurrent services, vector search, and future remote access favor
Postgres; CAS gives integrity, dedup, provenance without database blobs.

## ADR-009 — Web-first UI with a native display agent {#adr-009}

**Decision.** Angular renders conversation, timeline, approvals, artifacts, dashboards;
versioned WebSocket carries typed events; Chromium app-mode windows give predictable
per-display rendering; a small Rust agent uses Hyprland's request/event sockets to place,
focus, observe. Desktop privilege never lives in the browser shell. Flutter remains an
option for later mobile/satellite clients on the same contracts.

## ADR-010 — No internal distributed bus until a second machine requires it {#adr-010}

**Decision.** Modules communicate in process; post-commit domain events flow through a
transactional outbox to WebSocket clients. When durable cross-machine messaging is proven
necessary (M7+), introduce NATS JetStream behind the existing `EventPublisher` port. MQTT
stays an IoT/HA concern, not Jarvis's command bus.

## ADR-011 — Single reasoning provider with deterministic degradation {#adr-011}

**Context.** v1 hardware cannot run useful local reasoning models, and no Anthropic API
billing exists. The only LLM available is Claude Code CLI via the owner's subscription,
whose quota comes in rate-limit windows.

**Decision.** Claude CLI is the sole reasoning provider in v1. The fallback tier is
**deterministic degradation, not a smaller model**: full UI/history, R0 tools,
direct/slash commands, rule-based home-intent grammar (HA sentence triggers), and a
visible queue for LLM-needing runs until the profile recovers. CPU-only embedding models
(`fastembed`/ONNX, bge-small class) remain in scope — they need no GPU — so memory and
retrieval are unaffected. Quota discipline: single-flight CLI execution, interactive
priority over background, deferrable-work batching into healthy windows, per-run budgets,
and no unattended quota-draining automations.

**Consequences.**
- (+) No dependency on hardware that doesn't exist; quota becomes a managed resource with
  visible state instead of a surprise failure; deterministic paths get built early and
  keep working forever.
- (−) True offline reasoning is unavailable in v1 (NFR-06 is satisfied in degraded mode:
  UI, history, local tools, HA, retrieval — but no generation); ambiguous natural-language
  home commands need the cloud until hardware improves.
- Revisit triggers: API billing enabled → add `anthropic-api` adapter; capable GPU/RAM →
  add `ollama` adapter and restore the M4 local-routing scope. Both are port-compatible,
  zero core changes. ADR-005's routing design stays as the framework these plug into.

## ADR-012 — Media: MPRIS as the control plane; Spotify API; Netflix excluded {#adr-012}

**Context.** The owner wants Jarvis to handle music and video (Spotify, YouTube,
Netflix). The three differ radically in integration surface: Spotify has an official Web
API with playback control (Premium required for control endpoints); YouTube offers no
ToS-clean third-party playback control for arbitrary sessions (Data API covers search
only); Netflix has no public API at all.

**Decision.** Three-tier strategy (`02` §11a):
(1) **MPRIS over D-Bus is the universal local transport-control plane** — one adapter
controls whatever is playing (Spotify desktop, Chromium/YouTube, mpv), giving
play/pause/next/volume and now-playing state for free;
(2) **Spotify Web API adapter** for service-level actions (search, play-by-URI, queue,
Connect device targeting): OAuth + PKCE, refresh token in keyring, playback R1 with a
volume cap, library mutations R2;
(3) **cast-a-link** opens web video in a dedicated credential-free media window placed by
`jarvis-agent`, with MPRIS taking over transport.
**Netflix is explicitly out of scope**: no search, browse, or account integration will be
built. Generic MPRIS pause/play and window focus incidentally work on whatever the human
starts there — that is hand-off, not integration, and it is the honest ceiling.

**Consequences.**
- (+) One cheap adapter (zbus/MPRIS, buildable at M3) covers most daily use ("pause",
  "next", "what's playing"), works in degraded mode via the deterministic grammar, and
  needs no accounts. Service depth is added only where a real API exists.
- (−) No "play The Crown on Netflix" — Jarvis opens Netflix and hands over; scraping or
  automating around the missing API is rejected (ToS + fragility). YouTube search without
  a Data API key routes through the browser worker (slower).
- Revisit triggers: an official Netflix/YouTube control API appearing; Spotify API terms
  changing; whole-house audio (M7) possibly adding Music Assistant via HA instead of
  direct integrations.

## ADR-013 — Real maps via MapLibre GL + PMTiles; no map-provider dependency {#adr-013}

**Context.** The HUD renders real route/place maps (docs/12 §3). Options: Google Maps
JS (API key, billing, online-only, tracking), Leaflet + OSM raster tiles (keyless but
online-only and load on a public tile service), or MapLibre GL + a PMTiles vector
extract served locally.

**Decision.** Production: **MapLibre GL JS rendering a PMTiles region extract served by
`jarvisd`** — a real interactive street map that is offline-capable (NFR-06), keyless,
cost-free, and private (no tile requests leave the machine). Bootstrap/dev: Leaflet +
OSM raster tiles with mandatory visible attribution. Google Maps is not integrated.
Place *data* (search, ratings, hours) remains a tools-layer concern with its own
attribution rules; the map renders geometry only.

**Consequences.**
- (+) Maps work in degraded/offline mode; no API keys or per-load costs; a regional
  extract is a few hundred MB on disk, one-time.
- (−) A tile pipeline task exists at M3 (download or build the region extract; config
  `[maps] pmtiles_path`); global coverage requires fetching new extracts; geocoding/
  routing beyond straight-line needs a separate decision if turn-by-turn is ever wanted
  (out of scope v1 — walk-time estimates come from the place-data tool).
- Revisit triggers: need for live traffic, turn-by-turn navigation, or global roaming.

## ADR-014 — General web search + fetch as the single knowledge/image source {#adr-014}

**Context.** Review of the HUD design (docs/12) found that several example scenarios
("who is this", weather, restaurant search) implicitly assumed data sources — current
facts, entity images, weather figures, place listings — that no tool in the design
actually supplied. Claude CLI's built-in web tools are deliberately disabled for the
reasoning profile (ADR-004), so without a replacement, Jarvis has no path to answer
"who is the current president" correctly, and no path to source an image for any
open-domain entity.

**Decision.** Add exactly one general-purpose tool pair — `web.search` + `web.fetch`
(R0, read-only, `02` §11b) — as the default open-domain knowledge source. No separate
image-search API: images come from the fetched page itself (`og:image`/primary image),
always carrying a visible source link on the HUD card. No dedicated weather or places
API in v1 — those queries also go through search+fetch, at best-effort quality, rather
than adding three narrow API integrations for a personal v1. Search provider is a
swappable config value (default Brave Search API) behind the same adapter-port pattern
as every other integration.

**Consequences.**
- (+) One new tool, not four; consistent attribution model (every web-sourced image
  visibly links its source, solving both the trust and the copyright question at once);
  fits the existing card grammar with no new rendering work; the routing rule (prefer
  search over model memory for time-sensitive phrasing) is a direct, testable fix for
  the stale-answer failure mode this review surfaced.
- (−) Best-effort data quality: restaurant hours/menus/ratings and weather figures
  parsed from fetched pages will be less structured and less reliable than a dedicated
  API would give; image relevance depends on what the source page happens to feature as
  its primary image. Fetched content is untrusted (Z4) and must go through the existing
  injection controls (`06` §5) — this tool is explicitly in scope for that threat table,
  not an exception to it.
- Revisit triggers: place/restaurant data quality proves insufficient → add a dedicated
  Places API behind the same tool port; weather likewise; a genuinely different image
  need (e.g. person recognition) is its own future FR with its own privacy ADR, not an
  extension of this one (see `08` roadmap risk register note on person-recognition scope).

---

## ADR-015 — Location provider for "nearby" queries {#adr-015}

**Context.** Live validation of FR-25 against "find a lunch place nearby" returned
generic city-directory junk (Yelp/DoorDash category pages for New York, Chicago, Denver)
because `web.search` has no coordinates to localize the query — nothing in the design
supplies "nearby" with a "where."

**Decision.** Add a `LocationProvider` port with three sources, tried in order: (1)
paired-device GPS, when `jarvis-agent` or a mobile client reports one and the user has
granted the location scope; (2) a configured home coordinate
(`[location] home_lat`/`home_lon` in `jarvisd.toml`) — the practical default for a
single-PC desktop assistant that isn't moving; (3) IP-based geolocation as a last-resort
approximate fallback, clearly labeled as approximate when used. Every `web.search` call
classified as location-dependent (place/restaurant/"nearby"/"near me" phrasing) carries
resolved coordinates as a query parameter, not just text.

**Consequences.**
- (+) "Nearby" queries become answerable at all; location resolution is one small port,
  swappable, testable with a fixed fake coordinate in golden traces.
- (−) Location is sensitive data (NFR-02): it must be labeled and provenance-tracked
  through the context assembler like any other context item, never silently attached to
  outbound cloud requests. IP geolocation is coarse (city-level at best) and must be
  presented as approximate, never as precise.
- Revisit trigger: multiple paired devices/rooms (M7) need per-device location, not one
  global home coordinate.

## ADR-016 — Source-quality weighting and fluent, single-question clarification {#adr-016}

**Context.** Live validation of FR-25 against "show me microcondia" surfaced a real
failure mode: the query is a typo genuinely ambiguous between two distinct concepts
(mitochondria; microconidia, a fungal-spore term), and the top organic search results
were low-authority AI-generated blog content that flatly conflates the two. A naive
search-and-answer implementation would confidently serve the wrong, blended answer.

**Decision.** Two additions to the `web.search`/`web.fetch` tool (`02` §11b):
(1) **source-quality weighting** — when synthesizing a factual answer, prefer
encyclopedic/reference, government, academic, and established-outlet domains over
unrecognized content-farm domains; when authoritative and low-authority sources
conflict, trust the authoritative one and don't surface the conflict as uncertainty
unless it's genuine (e.g. contested current events);
(2) **fluent single-question clarification** — when a query is genuinely ambiguous
between distinct real interpretations (not merely low-confidence), Jarvis asks *one*
natural spoken/caption question in its own conversational voice and waits for the next
utterance to resolve it — e.g. "Did you mean the cell organelle, or the fungal spore
term?" — never a multiple-choice picker UI. Button/option pickers are a convention of
text chat interfaces, not of this voice-first HUD (`12` §1); disambiguation is dialogue,
not a form.

**Consequences.**
- (+) Closes a real misinformation risk cheaply — no new tool, just a synthesis and
  routing rule; keeps the HUD's voice-first character intact instead of reaching for a
  chat-app affordance under pressure.
- (−) Source-authority classification is itself a small maintained list/heuristic (needs
  periodic review, not a solved problem); clarification adds a round trip for genuinely
  ambiguous queries — acceptable since the alternative is confidently wrong output.
- Revisit trigger: if the authority heuristic proves too coarse (blocks legitimate niche
  sources, e.g. specialist forums for a hobby topic), move to a scored rather than
  binary trust model.

## ADR-017 — Deep-dive: thread continuity, gallery/sources cards, artifact promotion {#adr-017}

**Context.** All prior validation used one-shot queries. A real deep dive (follow-ups,
comparing sources, requesting many images, reading a source in full, keeping the result)
stresses three things the design didn't handle: FR-24 shelves the canvas on *every* new
query (wrong for follow-ups), there is no card for many sources or many images, and the
ephemeral HUD has no bridge to the durable Artifact system (FR-08) for a thread worth
keeping.

**Decision.** Four additions, each reusing existing machinery:
(1) **Thread continuity (FR-27).** The router gains a *continuation vs. new-topic*
classifier — the same signal-in-the-routing-request mechanism as the location and
ambiguity signals (ADR-015/016). Continuations ("tell me more", "what about Y", "compare
that to Z", pronoun/topical back-reference) *extend* the active canvas — new cards append,
prior cards stay — and do NOT shelve. Only a genuine topic change shelves (FR-24
unchanged for that case). Pending approvals remain exempt.
(2) **Two new registered card types** (`12` §2.3): a **sources card** (a compact list of
the pages consulted, each a title + domain + link, for "show me the references"), and a
**gallery card** (a small grid of images, capped at 6–8, each tile individually
source-badged because images may come from different pages — one shared source link is
not acceptable when provenance differs).
(3) **Read-the-source is a browser handoff, not HUD re-rendering.** "Open that / let me
read it" routes to the existing browser worker (FR-15): open the real page, visibly, in a
Chromium window on a chosen display. The HUD never reproduces full page content — that is
both a scope boundary and a copyright one.
(4) **Artifact promotion.** Past a threshold (config `[ui] deepdive_promote_after`,
default 3 follow-ups on one thread), Jarvis offers to promote the thread into a
**Research Notes artifact** (FR-08): a versioned markdown document with accumulated facts
(paraphrased, not scraped), every source consulted, and referenced images — reopenable
after restart, the permanent record. The canvas keeps showing only the current
conversation state; the artifact is where the full bibliography and history live.

**Consequences.**
- (+) Deep dives feel continuous instead of resetting each turn; references and images
  get correct per-item attribution; durable output uses the artifact system already built
  rather than a bolt-on; full thread history stays in the ops-layer Run Spine, keeping the
  HUD face uncluttered.
- (−) A gallery is N search+fetch calls, not one — a real latency and tool-call-budget
  cost on a Claude-CLI-only, single-flight setup (ADR-011); hence the hard image cap and a
  visible budget impact. The continuation classifier will sometimes misjudge a boundary
  (shelve when it should extend, or vice-versa); mitigations: it is correctable by voice
  ("new topic" / "go back to X"), and shelving is reversible via Restore (FR-24), so an
  error costs one utterance, not lost work.
- Revisit trigger: if promotion-worthy threads are common, consider auto-promoting silently
  and notifying, rather than offering each time.


## ADR-018 — Home Assistant area→entity resolution and partial-failure reporting {#adr-018}

**Context.** Validation of "turn on the lamps in the living room" showed the design
specifies allowlisted HA control (ADR-006) but never how a plural, area-scoped command
("the lamps", "living room lights") expands to concrete entities, nor what Jarvis says
when only some succeed.

**Decision.** The HA adapter resolves area + device-class references to the concrete
allowlisted entity set using cached HA area/entity metadata (HA remains authoritative).
Execution is per-entity; the spoken/caption result reports outcome honestly and
specifically: full success ("living room lamps on"), or partial with the exact failure
("three of four on — the corner lamp isn't responding"), never a blanket "done" that
hides a failure. Resolution and partial-failure paths run in the deterministic grammar
(zero LLM/quota) where the phrasing is a known pattern.

**Consequences.**
- (+) Plural/area commands — the common case for voice home control — actually work;
  partial failure is surfaced, not swallowed, which is a trust property.
- (−) Requires keeping area/entity metadata reasonably fresh (cache invalidation on HA
  state change); ambiguous area names ("the lights" with no room and multiple rooms
  occupied) fall back to the fluent-clarification path (ADR-016).
- Revisit trigger: whole-house/multi-room presence (M7) makes "here"/"this room" resolve
  by device location.

## ADR-019 — News-interest profile for topicless news queries {#adr-019}

**Context.** "What is the latest news" has no topic — like "nearby" had no location.
Raw search returned generic bulletin-index pages. A daily-use assistant can't answer a
topicless news request well without knowing what the user cares about, and can't ask a
clarifying question every single time (the query recurs daily).

**Decision.** Add a user **news-interest profile** (config `[news] topics`, `[news]
sources`, optional per-topic weight) — the same idea the `morning` example skill hints
at, promoted to a real, reviewable setting. "What's the news" resolves against the
profile into concrete topic queries, each rendered as a headlines/digest card
(FR-25/ADR-014). With no profile configured, Jarvis asks once, fluently, what the user
follows, and offers to remember it (writing to the profile) rather than re-asking daily.

**Consequences.**
- (+) Topicless news becomes answerable and personal; reuses the headlines card and the
  memory/settings machinery already specified; degrades to one-time clarification, not
  daily nagging.
- (−) The profile is user state that needs a review/edit surface (like memory items,
  FR-16) and must respect the same privacy/provenance handling; a stale profile yields
  stale-feeling news until edited.
- Revisit trigger: automatic interest inference from usage — deferred; explicit
  configuration first, never silent behavioral profiling.

## ADR-020 — Neutral, attributed framing for contested and political news {#adr-020}

**Context.** "Latest on Iran" returned active-conflict, casualty-heavy, politically
contested coverage where sources carefully attribute and hedge ("the IRGC *claimed*",
"CNN could not independently verify"). The design has source-quality rules (ADR-016) but
nothing requiring Jarvis to preserve that neutrality and attribution — a HUD that
flattens "Iran claims X / US claims Y" into one confident voice would misinform.

**Decision.** For contested, political, or conflict news, Jarvis (1) attributes claims
to their source rather than asserting them as established fact, preserving the hedging
present in reporting; (2) presents contested points even-handedly rather than adopting
one side's framing; (3) does not sensationalize or dwell on graphic detail in the spoken
summary. This is a synthesis rule on the news/headlines path, applied whether the item
came from search or a dedicated source. It is a firm behavioral rule, not a stylistic
preference.

**Consequences.**
- (+) Keeps a trusted personal assistant from becoming a confident misinformation vector
  on exactly the topics where that does the most harm; aligns the HUD's single-voice
  brevity with honest attribution.
- (−) Attributed, even-handed summaries are longer and less punchy than a flat headline —
  an accepted cost on contested topics specifically; judging "contested" is itself a
  classification the model performs and can occasionally misjudge (err toward
  attribution when unsure).
- Revisit trigger: none expected; this is a standing safety rule.

## ADR-021 — Shopping is informational only; never monetized {#adr-021}

**Context.** "Recommend a good new keyboard" works via search but had no card type and no
policy. Product recommendation raises a trust question: does Jarvis earn from what it
recommends?

**Decision.** Two parts:
(1) a **product/recommendation card type** (`12` §2.3): product name, price, a few key
specs, a one-line "why", and a source/retailer link — distinct from the place card.
(2) an **invariant: Jarvis product recommendations are purely informational and are never
monetized** — no affiliate links, no retailer kickbacks, no sponsored placement, ever.
Recommendations are ranked only by fit and source quality (ADR-016). Any retailer link is
a plain reference, identical in status to any other source link. This is a firm
invariant, listed alongside the other non-negotiables — a paid recommendation is a
corrupted recommendation, and this is a personal trust product, not a storefront.

**Consequences.**
- (+) The user can trust that "recommend X" reflects fit, not revenue; removes an entire
  class of conflict-of-interest and keeps the recommendation logic simple (rank by
  quality, full stop).
- (−) Forgoes a revenue path some assistants use — irrelevant for a personal, single-owner
  system, and the point.
- Revisit trigger: none for personal use. Any future multi-user/commercial variant would
  require a *new* explicit decision and disclosure, never a silent policy drift.


## ADR-022 — Media resolution: artist/playlist defaults and a "now playing" query {#adr-022}

**Context.** Desk review of ADR-012 against real commands ("play ABBA on Spotify",
"play playlist A", "what is this song playing") found three unspecified behaviors: what
"play an artist" resolves to, how "play playlist X" reaches the user's *own* library
rather than public search, and that there was no query path at all for "what's playing" —
only the passive media bar existed.

**Decision.**
(1) **Artist-context default.** `spotify.play` given an artist-only resolution starts
that artist's context (Spotify's own shuffled top-tracks/artist-radio behavior via
`context_uri`) — no clarifying question for the common case of naming an artist.
Clarification is reserved for genuine multi-match ambiguity (e.g. two different artists
with the same name), per the ADR-016 pattern.
(2) **Playlist-by-name resolves against the user's library first.** `spotify.play_playlist
{ name }` searches the user's own saved playlists (requires the `playlist-read` scope,
already anticipated but unused in the `media-integration` skill) and only falls back to
public catalog search if nothing matches, so "play my running playlist" doesn't silently
return an unrelated public playlist.
(3) **"What is this song playing" is a first-class query, not just ambient display.**
Answered from the same MPRIS metadata already feeding the media bar (title/artist/album,
`mpris:artUrl` when the active player provides it — Spotify desktop does) via a spoken
answer plus a **now-playing card** (`12` §2.3): title/artist/album, art if available,
source player/app noted. No new adapter — this is a routing and card-grammar gap, not a
missing tool.

**Consequences.**
- (+) The two most common voice patterns for starting music ("play an artist", "play a
  playlist") behave the way a person actually expects, without unnecessary clarification
  round-trips; "what's playing" gets an honest answer instead of silence.
- (−) Playlist name matching needs fuzzy/partial matching (library playlist names are
  user-chosen and inconsistent) — ambiguous matches use the ADR-016 fluent-question
  pattern, not a picker; art is best-effort and depends on the active player exposing it.
- Revisit trigger: none expected — this is a refinement of ADR-012, not a new
  architectural decision.


## ADR-023 — Timers, alarms, reminders: deterministic personal utilities {#adr-023}

**Context.** The use-case catalog (docs/13, C1–C4) found the single most-used real-world
voice-assistant category — timers, alarms, reminders — completely absent. FR-17
automations technically could host reminders but are heavyweight (policy re-evaluation,
LLM intents) for what is a stopwatch.

**Decision.** A dedicated lightweight **timers module**: set/query/cancel timers, alarms,
and one-shot reminders entirely in the deterministic grammar (zero LLM, works offline and
in degraded mode). Persisted in Postgres (survive restart, NFR-05); firing produces an
audible alert (configurable sound, TTS announcement for reminders: "reminder — call
Mom"), a **timer/reminder card** on the HUD (countdown live for timers), and voice
dismiss/snooze. Multiple concurrent timers are named/enumerable ("cancel the pasta
timer", "how long left?"). Recurring/conditional/LLM-flavored scheduling remains FR-17's
job — the boundary is: if it needs policy re-evaluation or model reasoning at fire time,
it's an automation; if it's "make a noise at time T", it's a timer.

**Consequences.** (+) Covers the top daily use case with the cheapest possible machinery;
no external deps; fully testable. (−) Alert audio needs a small always-available playback
path independent of the TTS pipeline (a fired alarm must sound even if voice services are
down); alarm reliability while `jarvisd` is stopped is honestly bounded — v1 fires on
restart with a "missed alarm" notice, it does not pretend to be a hardware clock.

## ADR-024 — Lists and quick notes {#adr-024}

**Context.** Catalog E1–E3: "add milk to the shopping list", "what's on the list", "take
a note" had no path. Artifacts are too heavyweight for a grocery line.

**Decision.** A lightweight **lists/notes store**: named lists (shopping, todo, …) with
add/remove/check-off/read by deterministic grammar where phrasing is clear (LLM assist
only for ambiguous phrasing); a **list card** (items, check-off by voice or tap); quick
notes are single-item captures into a Notes list. A list or note can be promoted to a
versioned artifact (FR-08) when it grows into a document — same promotion pattern as
Research Notes (ADR-017). Local Postgres storage, plain rows, exportable.

**Consequences.** (+) Cheap, offline, daily-value; reuses card grammar + promotion
pattern. (−) One more small schema + grammar surface; sharing lists across users is out
of scope (single-owner v1).

## ADR-025 — Calendar via CalDAV {#adr-025}

**Context.** Catalog D1–D3: no calendar path at all. Owner accepted v1-Should scope.

**Decision.** One **CalDAV adapter** (works with Nextcloud, Fastmail, iCloud, and Google
via bridge/app-password): reads are R0 ("what's on today", "next meeting") rendered as an
**agenda card** + spoken summary; creates/modifies are R2 with the exact event
(title/time/attendees) in the approval. Calendar data is sensitivity-labeled personal
context (NFR-02) — included in cloud-bound prompts only under the same visible
context-assembly rules as everything else. Provider choice is config
(`[integrations.caldav]`), credentials in keyring.

**Consequences.** (+) One adapter covers most providers; high daily value. (−) CalDAV
quirks vary by provider (test against at least two); recurring-event editing is the
classic hard part — v1 supports creating simple events and reading expanded occurrences,
editing recurrences is deferred.

## ADR-026 — Outbound messages via SMTP; message reading deferred to v2 {#adr-026}

**Context.** The design's own canonical R2 example — "send a message to the landlord" —
had a fully specified approval flow and **no channel adapter at the end of it** (catalog
I1). Reading email/messages (I2–I3) is a large privacy surface with lower urgency.

**Decision.** One **SMTP send adapter** completes the outbound flow: `message.send { to,
subject, body }`, R2, approval shows the verbatim recipient/subject/body (exactly as the
docs/12 approval card already renders), idempotency key per send, provider-agnostic SMTP
config with credentials in keyring. **Inbox reading is explicitly deferred to v2**
(FR-20 channels): it requires its own privacy treatment (continuous access to
correspondence is a different trust grant than sending one approved message) and its own
ADR when scoped.

**Consequences.** (+) The flagship approval flow becomes real end-to-end at M4; smallest
possible channel commitment. (−) Email only — no SMS/Signal/etc. in v1; deliverability
(SPF/DKIM of the owner's own account) is the owner's mail provider's problem, not
Jarvis's.

## ADR-027 — Browser worker isolation: container is the contract, process+profile-dir is the dev fallback {#adr-027}

**Status.** **Accepted (M3a gate, 30 July 2026).** Owner pre-approved the shape (Option A)
at M3 decomposition; formally accepted at the M3a `/gate` together with deviations
D-M3a-1 … D-M3a-7 (`docs/milestones/M3a-gate-report.md`).

**Context.** FR-15 and docs/02 §8 require browser automation to run Playwright in a
**dedicated worker process** with **isolated profiles per trust domain**, visible mode for
consequential operations, credentials from the secret store (never prompted), and **typed
tool actions** (navigate, extract, click, download, screenshot) carrying audit evidence.
docs/06 §5 ("Malicious MCP/tool server") requires a browser worker to be treated like any
untrusted tool server: separate OS identity/container, allowlist, schema validation,
outbound-network restrictions, and — critically — **the host overlays policy; the worker
cannot self-declare safety**. The open decision was the *isolation mechanism* and how far
it binds across production vs. CI, since that mechanism is a security boundary (invariants
1 and 3) and is expensive to change later. Options weighed: (A) per-trust **container** in
production with a **separate-process + isolated profile-directory** fallback behind one
host protocol; (B) process + `rlimit`s only; (C) `bubblewrap`/user-namespace sandbox.

**Decision.** Adopt **Option A**.
- **Production contract = a per-trust-domain container** (matches docs/02 §12 "workers =
  per-trust containers, read-only mounts default, CPU/mem/time/net limits"). The container
  runtime, mounts, and network policy are **ops/host configuration** applied when the
  worker is launched — not something the worker or any page content can influence.
- **Dev/CI fallback = a separate OS process with an isolated Playwright profile directory**,
  speaking the **same host↔worker stdio protocol** as the containerised worker. CI runs a
  **fake** worker (no browser binaries in CI, per F3a.8); real Playwright is manual-verify.
- **One protocol both sides honour:** line-delimited JSON over the worker's stdio (like the
  MCP host, F2.7, and `claude-cli`). The host sends a **typed action**; the worker returns
  a **result** (status + page-derived text). The worker's response is **Z4 untrusted**:
  the host reads only the fields it models and **ignores everything else** — a worker can
  neither introduce a new action nor declare a tool call (invariant 1).
- **Host owns ToolPolicy** via the same overlay discipline as the MCP host (F2.7): each
  typed action is a host-registered tool with a **host-authored `ToolPolicy`**; an action
  the host has not written a policy for is not registrable. The worker's output never
  influences risk, scopes, or reversibility.
- **Credentials are host-injected as environment/secret-store references at launch, never
  passed in the worker's argv and never prompted** (invariant 5, docs/06 §5). Keyring
  resolution happens at the jarvisd boundary; the adapter receives already-resolved launch
  configuration.
- **All page-derived strings are sanitized** with the F2.8 result validator
  (`sanitize_result_content`: strip C0/C1/DEL control chars, bidi/zero-width format chars,
  size-cap) before they reach a log, span, or the model (docs/06 §5 tool-result smuggling,
  CF-13).

**Consequences.**
- (+) One wire protocol means CI (fake worker), dev (process+profile-dir), and production
  (container) exercise the *same* host code path — the security-relevant logic
  (policy overlay, Z4 sanitization, per-step audit, "a page cannot inject a tool call") is
  unit-testable **without a browser** and is identical across environments.
- (+) The container requirement is deferred to ops packaging, not blocked on it — the

## ADR-028 — NanoClaw as a policy-gated worker, not the top-level brain {#adr-028}

**Status.** **Proposed** — brainstormed with the owner 2026-08-06; design detail in
`docs/superpowers/specs/2026-08-06-nanoclaw-worker-integration-design.md`. Needs owner
acceptance before implementation (human-only decision, `docs/11` §3).

**Context.** The owner already runs [NanoClaw](https://github.com/nanocoai/nanoclaw) —
ref 29, `docs/10-references.md` — as their day-to-day agent, reachable only through
Telegram. It has real strengths Jarvis doesn't: multi-channel reach (WhatsApp, Telegram,
Slack, Discord, Gmail), a container-per-session sandbox that can spawn sub-processes, and
persistent markdown/OKF memory. The owner wants those capabilities available through
Jarvis's presentation layer instead of two separate systems — this is FR-20's "OpenClaw
bridge" clause, made concrete, and ADR-026 explicitly deferred this exact decision to
FR-20's own future ADR.

The complication: NanoClaw's container runs an autonomous "call tools until done" loop via
Anthropic's Agent SDK — structurally the exact pattern ADR-003 forbids as Jarvis's
top-level control flow, and invariant #1 forbids any execution path that bypasses
`policy::evaluate`. Routing Jarvis's UI directly onto nanoclaw-as-the-brain would mean the
presentation layer fronts an engine with no policy gate behind it — not a style
preference, a real invariant break. Options weighed: (A) nanoclaw as a policy-gated
*worker*, invoked like the M3a browser/coding workers, opaque internals but a single
gated, audited, timeout-bounded invocation; (B) nanoclaw as the top-level brain with
Jarvis as a thin view, requiring an explicit, human-approved exception to ADR-003 and
invariant #1; (C) no execution bridge at all, memory-only sync between the two systems.

**Decision.** Adopt **Option A**. Register `worker.nanoclaw.delegate` as a tool following
the exact shape of the coding worker (`crates/jarvis-adapters/src/coding.rs`): a narrow
`NanoclawTransport` trait, a `NanoclawWorkerHost` that owns the host-authored `ToolPolicy`
and turns the worker's output into a durable `ArtifactManifest` + `artifact.created` audit
event. Jarvis's orchestrator decides *when* to invoke nanoclaw; nanoclaw's internal loop
stays opaque, the same way the coding worker's internal reasoning is opaque — but the
invocation itself is one ordinary policy-gated tool call, not a bypass. The worker talks
to nanoclaw's existing host process (CLI subprocess, mirroring `ChildCodingTransport`),
not to its containers directly — nanoclaw's own `inbound.db`/`outbound.db` session
machinery is not reimplemented. NanoClaw's markdown/OKF memory is exposed read-only as a
Jarvis tool now (tagged as nanoclaw-sourced, not silently merged with Jarvis-native facts)
with a path to M4's `memory_sources` schema later. Risk tier defaults to R1 only if the
owner configures a dedicated Jarvis-facing nanoclaw agent group with outbound channel
delivery disabled (bounding the blast radius to "wasted compute"); otherwise R2 with an
explicit `ExecutionGrant` per delegation. Full inbound-channel bridging (nanoclaw/Telegram
messages starting a Jarvis run) is out of scope for this decision — a later FR-20 slice,
using the same device-pairing + `/sessions/{id}/messages` + `/ws/v1` surface the web shell
already uses, no new trait required.

**Consequences.**
- (+) None of Jarvis's invariants bend — the orchestrator remains the only decision-maker
  about *when* nanoclaw runs, and every invocation is audited and gated like any other
  tool.
- (+) Reuses a pattern already built and reviewed twice (browser worker, coding worker)
  instead of inventing a third execution primitive.
- (+) NanoClaw stays untouched as an external dependency — no fork, no code changes to
  nanoclaw itself required for the first slice.
- (−) Jarvis cannot see or gate nanoclaw's *internal* tool use — the grant covers the
  delegation, not nanoclaw's sub-actions. Mitigated by the R1-only-if-outbound-disabled
  rule; revisit if nanoclaw needs real external side effects sooner.
- (−) Two memory systems coexist until M4 lands (nanoclaw's markdown/OKF, Jarvis's future
  `memories` schema) — read-only bridging only, no reconciliation, until then.
- Revisit trigger: if the owner decides they want nanoclaw itself to drive Jarvis's
  presentation layer (Option B), that requires a separate, explicit ADR revising ADR-003
  and invariant #1 — not a silent expansion of this one.
  adapter and its guarantees land now; the container profile is an F3a.8 / deployment
  concern.
- (−) The process+profile-dir dev fallback is weaker isolation than a container (shared
  kernel, host filesystem visible subject to profile-dir scoping) — acceptable for
  dev/CI, **not** for production; the gate must confirm production runs the container
  profile. (Rejected B: `rlimit`s alone give no filesystem/network isolation. Rejected C:
  `bubblewrap` adds a second isolation mechanism to maintain alongside the container one
  we already need for docs/02 §12.)
- Revisit trigger: if the container runtime choice (podman/docker/systemd-nspawn) forces a
  different launch handshake, that is a follow-up ADR, not a protocol change — the stdio
  contract is deliberately runtime-agnostic.


---

## Superseded / carried-over notes from v1

- v1 ADR "Use Microsoft.Extensions.AI as provider abstraction" is superseded by the
  Rust `ModelProvider` port (`05` §4); the intent (provider-neutral middleware with
  telemetry/caching) carries over as tower layers around adapters.
- v1 market scan (OpenClaw, OpenJarvis, Open Interpreter, Open WebUI, AnythingLLM,
  OpenVoiceOS, Rhasspy, Leon, LiveKit/Pipecat) remains the evidence base. Key carried
  lessons: one authoritative gateway with typed events; server-enforced scopes over
  self-declared capabilities; idempotency for side effects + monotonic sequences for UI
  recovery; persist important domain events for gap recovery; confirmation-by-default
  execution UX; agent-editable HTML is always untrusted and never shares an origin with
  privileged surfaces.

## ADR-029 — Generated-app format: a JSON spec against a locked Vite template, over a closed capability vocabulary {#adr-029}

**Status.** **Proposed** — written at F6.1 (M6). Confirms the default the owner settled on
2026-08-09 (`docs/milestones/M6-features.md` §"Scope decisions" #3); needs owner acceptance
at the M6 gate (human-only decision, `docs/11` §3).

**Context.** FR-18 says "generate small local web applications **from validated
templates**; open them sandboxed." docs/08 §6 recorded the format as a decision deferred to
M6, with the default "JSON spec + locked Vite template". The question this ADR settles is
what the *model* produces and what the *host* validates, because that determines what
"validated" can possibly mean: if a model emits arbitrary source, "validated template" is
marketing; if it emits a constrained document that the host renders against a template it
owns, the phrase has teeth.

The second, sharper half is what a generated app is allowed to **ask for**. docs/06 §6
requires "optional interaction only via a `postMessage` bridge exchanging short-lived
capability tokens for operations named in the artifact manifest; **undeclared capability ⇒
reject**". Through M3–M5 `Capability` was a free-form `String` newtype, which was harmless
while it was pure provenance metadata carried in a manifest nobody enforced. It stops being
harmless the moment a bridge enforces it: *a bridge that enforces free-form strings enforces
nothing*, because "is this capability declared?" is only decidable against a set the host —
not the model — defines.

**Decision.**
1. **The app spec is a small JSON document, not source code.** It names a host template id,
   a title, the capabilities the app declares, its data bindings, and its build limits.
   `jarvis-domain::appspec` owns validation; `jarvis-contracts::appspec` owns the wire
   shape. A spec is validated **before any build starts**, so an invalid spec fails in a
   pure function with a typed reason, not inside a Node worker as a timeout.
2. **Templates are a closed, host-owned, versioned set** (`TemplateId`, `dashboard/v1`).
   A template id selects the exact locked source tree and lockfile the builder uses, so an
   id a model could invent is an id the builder could not pin. The build is a **Vite**
   build against a committed lockfile with the network disabled (F6.2) — which is what the
   `build.lockfileHash` field already in the docs/04 §4 manifest was shaped for.
3. **The capability vocabulary is closed** (`Capability` becomes an exhaustive enum).
   Each variant names an **already-registered** tool and carries a declared risk tier.
   Unknown capability ⇒ the **spec** is rejected at validation time, not at bridge time.
   Adding a capability is a deliberate host change (a variant, a backing tool, a tier),
   never a string a model can invent.
4. **Naming is not authorizing** (invariant 1). A declared capability is at most an
   authorization to *ask* at bridge time. The host still runs `policy::evaluate` against
   the live registry and still mints an `ExecutionGrant` for R2+. `Capability::risk()` is a
   **preview** used for approval text only; a test in `jarvisd` asserts it never diverges
   from the registered tool's host-owned `ToolPolicy.risk`.
5. **Rejections echo untrusted text safely.** Template/capability/title/binding names come
   from model output and travel into problem bodies, spans and audit reasons, so every
   error variant that quotes one clamps its length and strips control and bidi/zero-width
   characters (CF-13, docs/06 §5).
6. **Limits are host-owned ceilings, and are rejected rather than clamped.** A spec may
   request *less* than the host maximum for bundle size and build time; requesting more is
   an error, so a caller never silently receives a build under a limit it did not choose.

**Consequences.**
- (+) "Validated template" becomes a decidable property, and "undeclared capability ⇒
  reject" becomes a decidable question — which is what makes golden 8 assertable at all.
- (+) The spec-validation table is a pure-domain test table with no I/O, so the whole
  rejection surface is covered without a builder, a browser, or a database.
- (+) The manifest's `capabilities` becomes an exhaustive union on the wire, so the web
  shell can `switch` on it instead of guessing at a string.
- (−) **Every capability needs host code.** A generated app can never reach a tool nobody
  wrote a variant for. That is the intended cost, not a limitation to engineer around.
- (−) The vocabulary starts small (three Home Assistant operations: read state, set a
  light, execute a scene — chosen to span R0/R1/R2 so the bridge exercises the
  auto-authorized, live-shown and approval+grant paths). Later milestones widen it
  deliberately.
- (−) **A stored capability string outside the vocabulary now fails the manifest load**
  rather than being dropped. Fail-closed is the only reading that keeps the bridge honest —
  a silently shortened capability list would describe a *less* capable app than the bundle
  in the CAS. No such row can exist today (every M3–M5 producer wrote an empty array), so
  this is asserted by a test rather than handled by a migration.
- Revisit trigger: if a template ever needs to carry model-authored *source* rather than a
  spec, this ADR is void and the sandbox story has to be re-derived from scratch — that is
  a different security posture, not an increment on this one.

## ADR-030 — Generated apps render in an opaque-origin sandboxed frame, not a second loopback origin {#adr-030}

**Status.** **Proposed** — written at F6.4 (M6). Needs owner acceptance at the M6 gate
(human-only decision, `docs/11` §3). Depends on and does not reopen [ADR-029](#adr-029).

**Context.** docs/06 §6 requires a generated app to run "in a sandboxed iframe or isolated
Chromium profile; restrictive CSP; **no same-origin relationship** with the control UI; no
arbitrary network; no direct MCP/host access." The M6 feature list named the choice
explicitly — a **separate loopback origin** (a distinct port is a distinct origin) versus
an **opaque-origin sandboxed frame** — and called it an ADR because everything else in the
milestone leans on it and it is expensive to move later. The v1 market-scan lesson carried
in this file's appendix is the constraint behind both options: *agent-editable HTML is
always untrusted and never shares an origin with privileged surfaces.*

Two facts shaped the answer. First, jarvisd authenticates with a **bearer device token**,
not cookies (docs/05 §6); an `<iframe src>` cannot carry an `Authorization` header, so a
second-origin design needs a new URL-token auth surface that exists for no other reason.
Second, a second loopback origin is **one** origin: every generated app served from it
shares its `localStorage`, `sessionStorage`, IndexedDB and BroadcastChannel. Two apps
generated from two unrelated requests would be same-origin *with each other*.

**Decision.**
1. **The app document is rendered in an iframe with `sandbox="allow-scripts"` and no
   `allow-same-origin`,** which gives it a **unique opaque origin per frame instance**.
   Not "a different origin from the shell" — a different origin from *everything*,
   including every other generated app. The attribute is static in the template; Angular
   refuses to bind `sandbox` at all (NG0910), so no runtime value can widen it.
2. **The shell fetches the document with its device token and passes it through `srcdoc`.**
   No new auth surface, no second listener, no port to configure or firewall — which also
   keeps the resident footprint where NFR-15 wants it.
3. **jarvisd serves it from a dedicated route** — `GET /api/v1/apps/{id}/versions/{v}/document`
   — that requires `ArtifactKind::Bundle` and sends `Content-Security-Policy: sandbox
   allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline';
   img-src data:; font-src data:; connect-src 'none'; form-action 'none'; base-uri 'none';
   frame-ancestors 'self'`. The **existing blob route is untouched**: it still serves every
   artifact as `attachment` + `nosniff`, so the download path never had to be relaxed to
   get a render path.
4. **The policy travels inside the document too.** `srcdoc` content is not delivered by the
   response whose header the browser saw, so the host prepends its own
   `<meta http-equiv="Content-Security-Policy">` as the first bytes of the served
   document. CSP composes intersectively — a second policy in the bundle can only
   *narrow*, never loosen, the host's.
5. **`script-src 'unsafe-inline'` is deliberate, not a concession.** A single-file bundle
   *is* one inline module script; the whole document is the untrusted unit, and running
   its script is the point of rendering it. What the policy denies is everything that
   script could reach: origin, network, form submission, base URI, plugins.
6. **`ArtifactKind::is_renderable_in_m3` is renamed** to `renders_inline_in_shell`, with a
   complementary `renders_in_app_sandbox`. A test asserts every kind has exactly one render
   path: a kind in neither would be silently unrenderable, a kind in both would be a
   same-origin escape.

**Consequences.**
- (+) Isolation is **per app instance**, strictly stronger than a shared second origin: no
  generated app can read another's storage, because it has none it shares with anything.
- (+) No second listener, no second auth surface, no URL-borne token to leak through a
  referrer, history entry or shoulder.
- (+) The renderable path is a *separate* route, so M3a's anti-execution guard on the blob
  route stays exactly as security-auditor B1 left it.
- (−) The shell holds the untrusted bytes in the control origin's JS heap on the way to
  `srcdoc`. That is a real footgun — one `innerHTML` away from the thing this ADR exists
  to prevent — so it is constrained by construction (a dedicated signal, one renderer, one
  binding) and asserted by tests that the bytes appear only in the frame.
- (−) `bypassSecurityTrustHtml` appears in the renderer, which looks alarming in review
  forever. It is correct: Angular's sanitizer would strip the app's own script, and
  Angular's sanitizer is not the boundary — the opaque origin and the CSP are.
- (−) An opaque origin cannot be named in a `postMessage` `targetOrigin`, and inbound
  messages arrive with `origin === "null"`. F6.5 must therefore verify the **`event.source`
  identity** against the frame's `contentWindow` rather than compare origin strings, and
  post **into** the frame with `targetOrigin: "*"` — which is safe only because the frame
  is opaque and single-purpose. This is the one place the choice makes F6.5's job harder,
  and it is recorded here so that it is designed rather than discovered.
- Revisit trigger: if a generated app ever legitimately needs persistent storage, a real
  network origin, or to be opened as a top-level window, this decision is void — those are
  a different product, and each would need its own origin story.
