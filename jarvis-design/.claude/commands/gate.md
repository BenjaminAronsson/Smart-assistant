---
description: Run a milestone exit gate and produce the gate report for human sign-off
---
Run the GATE LOOP (docs/11 §2) for milestone: $ARGUMENTS (default: current milestone).

1. Collect the milestone's exit evidence list from docs/08 §1.
2. Run: full workspace suite; `cargo xtask golden` for the milestone's mapped scenarios
   (docs/07 §2); applicable security release gates (docs/06 §8) — at minimum arch-test
   and the adversarial suite from M2 on; `cargo xtask perf --rss` + latency scenarios
   against docs/01 §4.1 budgets (8 GB profile numbers).
3. Invoke perf-warden for the measurement review and security-auditor for a
   whole-milestone diff pass (since the previous gate tag).
4. Write docs/milestones/M<N>-gate-report.md: evidence item -> result (with measured
   numbers), failures, deviations requested (e.g. NFR-04 relaxation with rationale),
   and open risks.
5. STOP. Present the report for human sign-off. On approval, tag the repo
   (m<N>-complete) and update the roadmap checkmarks. Failed items go back into the
   feature list — a gate is never "passed with exceptions" silently.
