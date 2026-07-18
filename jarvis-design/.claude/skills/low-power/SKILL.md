---
name: low-power
description: Building within the 8-16 GB ultrabook budget - resident-memory discipline, event-driven idle, lazy loading, worker serialization. Use when adding dependencies, background tasks, caches, or resident components.
---

# Low-power discipline

Spec: docs/01 §4.1 (8 GB is the FLOOR), docs/09 §5 (defaults, not opt-ins).

1. Before adding anything resident, ask: can it be transient (spawn/compute/drop) or
   lazy (load on demand, unload on idle)? Default yes. Resident requires a bounded size
   and a budget-table entry (perf-warden blocks otherwise).
2. Background work is event-driven (Notify, watch channels, Postgres NOTIFY) - never
   poll loops. A truly required interval (retention sweeps) is >= minutes and
   config-visible.
3. Caches are bounded (moka max_capacity or hand LRU) with size + hit-rate metrics.
4. Heavy transients (Playwright, coding CLI, STT) acquire the worker semaphore
   (`[budgets] max_concurrent_workers`, default 1). Never bypass it "because it's fast".
5. New dependencies: check `cargo tree -d` and compile/binary impact; prefer the light
   ecosystem-standard option; heavyweight additions get discussed in the PR.
6. Chromium is the biggest consumer and outside our process: app-mode windows only;
   don't spawn extra windows without a display-profile reason.

## Measuring
`cargo xtask perf --rss` samples jarvisd + postgres + workers at idle and under the
reference scenario. Assertions: idle <= 1.0 GB total stack; peak <= 2.0 GB on the 8 GB
profile / 3.0 GB on 16 GB. After M1, `powertop` at idle must not show jarvisd/postgres
among top consumers - if it does, it's a bug (usually a polling loop). Budget
relaxations are a human decision, logged in the gate report with rationale.
