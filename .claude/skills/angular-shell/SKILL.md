---
name: angular-shell
description: Building the Angular web shell - generated types, WS client with resync, surfaces, approval UI, accessibility. Use whenever working in web/.
---

# Angular shell

Spec: docs/02 §8, docs/05, and the normative visual/interaction design in docs/12 (tokens, Run Spine, approval-card anatomy, keyboard map). Implement docs/12 exactly - palette/type deviations are a blocking review finding. State approach: Angular signals + services (deferred decision,
docs/08 §6 - no NgRx without an ADR).

1. Types come ONLY from `src/generated/` (cargo xtask codegen). A missing type is fixed
   in jarvis-contracts + codegen, never with a local interface.
2. WS core service: connect with bearer token; track last seq; on gap/reconnect fetch
   the timeline snapshot (`?since=`) + run snapshots, then resume the stream. Transient
   events (token deltas, partials) render immediately and are REPLACED by the durable
   snapshot event on completion - the UI converges to snapshot truth.
3. The front face is the HUD (docs/12): presence orb + caption + materialization
   canvas; the ops layer (Run Spine, timeline, ApprovalTray detail, Diagnostics) is one
   keystroke away (Ctrl+.). Result panels are REGISTERED card types only - the model
   proposes content, never layout or HTML (security property). Panel lifecycle per
   docs/12 §4: shelve on new query, restore, dismiss, 2h TTL, approvals exempt. Maps:
   MapLibre GL + PMTiles from jarvisd (Leaflet+OSM during bootstrap). Ambient motion
   pauses on hidden/unfocused window, reduced-motion, and battery-saver.
4. ApprovalTray is the most polished surface (spec mandate): exact_effect rendered
   VERBATIM (never truncated), risk badge, expiry countdown, approve/deny with
   optimistic-block confirmed by the WS decision event.
5. Provider indicator always visible: active profile, health, quota reset countdown in
   degraded mode, queue position for waiting runs (degraded.queued).
6. Accessibility (NFR-11): full keyboard operation, visible focus, aria-live for
   streaming text, transcript pane for voice. Keyboard-only test before calling a
   surface done.
7. Cancellation is one keystroke (Esc on the active run) and visible whenever a run is
   in ModelRunning/ToolRunning.
8. Bundle discipline: standalone components, lazy routes per surface, initial JS
   < 500 KB gzip, no component libraries without discussion.
