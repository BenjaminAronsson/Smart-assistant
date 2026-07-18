---
description: Run the feature loop for one vertical feature (spec -> threat note -> tests -> implement -> review -> DoD)
---
Run the FEATURE LOOP (docs/11 §2) for: $ARGUMENTS

Model check: if this feature touches jarvis-domain or jarvis-application (state machine,
policy, grants, contracts), it needs the strong model — prompt the owner to /model up if
on Sonnet. Pure infra/adapter/web-shell features run fine on Sonnet 4.6.

Mandatory order:
1. **Spec**: locate governing FR/NFR + doc sections; quote them in your plan. If no spec
   covers this, STOP — it needs a spec addition or an ADR, not code.
2. **Threat/risk note** (2–10 lines): trust zones touched, risk tiers involved,
   user-visible failure states. This goes into the PR description.
3. **Contracts first**: DTO/port/schema changes, `cargo xtask codegen`, commit generated.
   (ws-contracts + sqlx-data skills apply.)
4. **Tests first**: invoke the test-architect subagent for domain/application changes;
   confirm tests fail for the right reason.
5. **Implement** to green using the inner loop (cargo check -> targeted tests). Relevant
   skills: state-machine, policy-grants, provider-adapter, low-power, angular-shell.
6. **Reviews**: always rust-reviewer; security-auditor if the diff touches
   policy/grants/tools/adapters/gateway/secrets; contract-keeper if contracts/migrations
   changed; perf-warden if anything resident/background/dependency was added. Address
   BLOCKING findings before proceeding.
7. **DoD** (docs/07 §3): walk the checklist explicitly; run the full workspace suite
   (build, test, clippy -D warnings, fmt --check, arch-test, sqlx prepare --check,
   web lint/test/build).
8. Produce a small vertical PR: description = feature summary + threat note + DoD
   confirmation + any spec ambiguities found (flag those to the human).
