# M10 gate report — product hardening

**Status: NOT READY FOR SIGN-OFF.** Everything executable is green. The milestone's promise —
*the owner installs on a machine that has never had Jarvis, talks to it hands-free, breaks it on
purpose, restores it, upgrades it, and rolls the upgrade back, following written instructions
with no source tree and no help from the person who built it* — is only partly evidenced,
because three pieces of that promise need a person and hardware, and none of the three has
happened yet.

Prepared 2026-08-30 against this worktree, covering F10.1–F10.9 (all nine features ticked in
`docs/milestones/M10-features.md`). The last feature, F10.9 (the install artifact), landed as
PR #82.

---

## 1. Exit evidence

> **M10 exit evidence** (`docs/08-roadmap.md`, M10 row): *repeatable install/upgrade/rollback;
> full security release checklist passes; golden 10.*
>
> Fuller statement (`docs/milestones/M10-features.md`): *the owner installs on a machine that
> has never had Jarvis, talks to it hands-free, breaks it on purpose, restores it from a backup,
> upgrades it, and rolls the upgrade back — following written instructions the whole way, with
> no source tree and no help from the person who built it.*

| # | Claim | Result |
|---|---|---|
| 1 | Repeatable install | **PARTIAL** — CI installs a signed release into a pristine `ubuntu:24.04` container; a real machine with sound hardware has not run it |
| 2 | Repeatable upgrade/rollback | **PASS (code)** — `update_rollback.rs`; the two-binary case (old binary against a rolled-back DB) needs a second tagged release and is deferred to the first real upgrade |
| 3 | Backup/restore returns a *working* house | **PASS** — `golden10_the_house_survives`, mutation-checked |
| 4 | Security release checklist passes | **PASS (code)** — `release_signing.rs`, `cargo deny check`, both review subagents' findings closed on PR #82 |
| 5 | Golden 10 | **PASS** — registered in `cargo xtask golden`, exit 0 |
| 6 | Talks to it, hands-free | **PASS (code, from M8a/M8c)** — inherited, not re-demonstrated this milestone |
| 7 | Administrable (policy UI, accessibility) | **PASS** — F10.5 policy view matches `policy::evaluate`; F10.6 axe + keyboard-only pass |
| 8 | Diagnostics bundle is safe to share | **PASS** — `diagnostics_bundle.rs`, seeded secret/transcript/message body appear nowhere |
| — | NFR-04 on reference hardware | **NOT DONE** — dev-host figures only |
| — | Wake-word false-accept budget (ADR-032) | **NOT DONE** — harness verified, no corpus, one real-room reading 7.4x over budget |

Full per-claim evidence table: `docs/milestones/M10-acceptance.md` §1 (17 claims, all
executable and passing). §2 of that document is the crux of this report: three items that need
a person and hardware, none delegable to a script, none done.

### What blocks sign-off — read this before the measurements

`docs/milestones/M10-acceptance.md` §2 lists three gaps:

1. **The hardware half of a clean-machine install.** The *build* half is done — a `debian:13`
   container with nothing preinstalled built and ran the whole stack, and found four real
   defects in `docs/TRY-IT.md`'s prerequisite list (missing `build-essential`,
   `libasound2-dev`, `curl`/`git`/`ca-certificates`, and an unnamed Node version floor). A
   container has no sound card, no microphone, no compositor and no session bus. Pairing a real
   browser, a real audio device opening, the wake word firing in a room, and `room-node` under
   Hyprland remain unverified on a clean machine.
2. **NFR-04 on the reference 8 GB ultrabook.** Dev-host figures are 621 ms transcript / 119 ms
   first audio, against budgets of 800 ms / 1200 ms. That is a fast desktop; the budget in
   `docs/01` §4.1 is the ultrabook, and nothing has run there.
3. **The wake-word false-accept corpus (ADR-032).** The harness works
   (`crates/jarvis-agent/tests/wake_onnx.rs`) and the corpus does not exist. The one real-room
   reading recorded during M10 testing is **7.44 accepts/hour against ADR-032's budget of
   1.00** — for the word `alexa`, not the shipped `hey jarvis`, with no ground truth (nobody
   logged who said what, and some detections were a test clip playing deliberately). It is a
   signal, not a measurement. It is 7x over.

Two more items are owner-only decisions, not evidence gaps: **F10.9's position in M10** (it
landed after F10.7 rather than before it, reversing the order the milestone brief originally
assumed — see `docs/milestones/M10-features.md` F10.9 note), and **the `/etc/jarvis/secrets.env`
secret-at-rest trade** (a systemd *system* service has no session bus and cannot reach the
keyring, so the packaged config falls back to a root-only file on disk instead of a keyring
reference — a deliberate, but unratified, narrowing of invariant 5's "secrets are keyring
references" for exactly one deployment path).

Ticking this milestone now would record it as met on evidence nobody gathered.

---

## 2. Measurements

| Check | Result |
|---|---|
| `cargo test --workspace --no-fail-fast` | **1610 passed, 0 failed, 2 ignored** |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo xtask arch-test` | 9 crates, dependency rules hold |
| `cargo xtask codegen --check` | generated outputs up to date |
| `cargo xtask golden` | golden 1–7, 9–12 + M3a/M3b/M5/M6 acceptance — pass |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok |
| `cargo xtask perf --rss` | cold start **0.053 s** (budget < 2 s) — PASS; idle RSS **22.9 MB** (typical band 40–80 MB, ceiling 120 MB) — PASS |
| web `npm test` / `npm run lint` / `npm run build` | 303 passed, lint clean, build ok |
| CI job `install-artifact` | **PASS** — cuts a signed release, verifies it, installs into a pristine `ubuntu:24.04` with no Rust, no Node, no source tree |
| NFR-04, dev host (not the reference machine) | 621 ms transcript / 119 ms first audio (budgets 800 ms / 1200 ms) — passes, but on the wrong hardware |
| Wake-word false-accept, controlled corpus | `accepts=0 rate=0.000/hour` over 3 clips / ~4 seconds — harness proof, not a measurement |
| Wake-word false-accept, one real room, one day | 102 detections / 13.58 h = **7.44/hour** (budget 1.00), word `alexa`, no ground truth |

---

## 3. Review findings (PR #82, F10.9)

Both review subagents ran against the install-artifact diff; every finding was fixed before
merge.

**security-auditor: 2 BLOCKING, 4 IMPORTANT, 8 MINOR.** Both BLOCKING and all four IMPORTANT
are fixed. Seven of the eight MINOR are fixed; the eighth was `println!("migrations applied")`
against the "use `tracing`, never `println!`" convention, kept deliberately — telemetry is
never initialised on the migrate path and `install.sh` reads that line on stdout — now with a
comment saying so, so the next reviewer does not file it again.

- **BLOCKING 1** — `EnvironmentFile=` put the plaintext production DSN into jarvisd's process
  image, and every child it spawns — `claude` included — inherited it, violating invariant 5.
  Fixed with `host_env::scrub_secrets` at every spawn site, plus a source-walking test that
  fails the build if a new `Command::new` forgets it.
- **BLOCKING 2** — CI published releases signed with a throwaway key to the exact URL the
  README tells owners to download from, so the documented verify step would have printed
  "release verified" over an artifact whose signature proves nothing about who built it. Fixed
  by deleting the publish job — releases are cut locally with the real key (`docs/06` §9).

**rust-reviewer: 0 BLOCKING, 3 IMPORTANT, 9 MINOR/NIT.** All three IMPORTANT and eight of the
nine MINOR/NIT are fixed. The one left is cosmetic and named here rather than quietly dropped:
`dist::staged_layout` still takes a `version` argument it ignores, so `workspace_version()` runs
on every stage to be discarded — and `release.sh` extracts the version a second time, with a
comment claiming the two cannot disagree and nothing enforcing it. Harmless today (release.sh's
extraction is the one that is tested, after it was the source of a real defect earlier in
F10.9); worth collapsing to one extraction when that code is next touched. Notably fixed:
`jarvisd migrate` could block forever on `pg_advisory_lock`, with both installers gated on it
(invariant 4; now bounded and interruptible), and `xtask dist` never cleared its staging
directory, so a stale file from a previous run was enumerated into the manifest, signed and
shipped.

**CI caught one no local run could:** under `set -o pipefail`, `first-run.sh --check-only` died
silently at "provider workdir" on any host with **no config file** — which is every fresh host,
and the exact command the README gives an owner for checking a new install.

None of these findings contradicts an Accepted ADR. The `/etc/jarvis/secrets.env` fallback
(§1, owner-only decision) is a narrowing of invariant 5's stated posture for the systemd
system-service path specifically — recorded above for owner attention, not treated as a
violation, because the CLAUDE.md invariant is prose the owner may formally except, not an ADR
this report can supersede.

---

## 4. Open risks

- **The three §2 gaps above** (clean-machine hardware, NFR-04 on reference hardware, false-accept
  corpus) are this report's headline risk. See §1.
- **The two-binary upgrade** — `update_rollback.rs` proves forward migration and rollback
  against one binary; it does not run an *old* binary against a rolled-back database, which
  needs two tagged releases. First real upgrade between tagged versions is the moment to check
  it (`docs/milestones/M10-acceptance.md` §2.4).
- **S3** (carried from M8): every spoken run answer is labelled `Normal`, so a run that used a
  mail or calendar tool and reads the result back can be spoken by ElevenLabs. Needs a
  tool-activity signal the socket does not currently carry (`RunUpdate` has no tool variant) —
  an application-layer change, not a patch. Mitigation unchanged: the feature is off unless
  explicitly consented to.
- **M8b D1** (carried): automations are created API-only; F10.5's policy surface is a natural
  fit but this was not folded in.
- **AEC active cost** (carried): ~9.3% of a core while a satellite is speaking. Only paid while
  speaking; revisit only if a satellite proves too slow.
- **Screen-reader feel and photo-background contrast** (`docs/12` §8.1): structure is tested;
  announcement order/verbosity in a real reader and an arbitrary photo behind the scrim are not
  reducible to assertions.
- Dark theme: still deferred, not part of M10.

---

## 5. Recommendation

**The code gate passes. The milestone is NOT ready for sign-off.**

Every command in `docs/milestones/M10-acceptance.md` §1 is green: 1610 tests, golden 1–7 and
9–12, arch-test, codegen, `cargo deny`, clippy, fmt, the RSS/cold-start budget, the web suite,
and a CI job that installs a signed release on a machine with no Rust, no Node and no source
tree. Both review subagents found real defects on F10.9 and every one was fixed before merge.
None of that is in question.

What is missing is not code — it is evidence that only a person at real hardware can produce,
and ticking this milestone without it would record "the owner installs on a machine that has
never had Jarvis" as demonstrated when it has only been demonstrated in a container with no
sound card. Three items, each with the exact command already written down in
`docs/milestones/M10-acceptance.md` §2, so the remaining work is a checklist:

1. **Clean-machine install — two machines, and only one of them needs hardware.**
   `M10-acceptance.md` §2.1 was split after this report was first written, because it had
   bundled the daemon host with a room node and made the whole item wait on satellite
   hardware. It does not: `cpal` is in `jarvis-agent` alone and `jarvisd` has no audio
   dependency at all.
   - **§2.1a, the host** — fresh Debian/Ubuntu, Docker, systemd. No microphone, no
     compositor. A VM is a legitimate host, including one on the development machine. What
     is left is only what a container cannot show: real systemd ordering, surviving a reboot
     unattended, and pairing a real browser.
   - **§2.1b, a room node** — microphone, speakers, Hyprland. The only part that genuinely
     needs hardware, and where the wake word has to fire in a real room.

2. **NFR-04 on the reference 8 GB ultrabook**, with the voice compose stack running:

   ```bash
   JARVIS_NFR04_REAL=1 cargo test -p jarvisd --test voice_latency_real -- --nocapture
   ```

   It prints both figures against budget and fails if either is exceeded (`M10-acceptance.md`
   §2.2).

3. **The wake-word false-accept corpus.** Capture hours of ordinary household audio at 16 kHz
   mono — television, conversation, kitchen noise — into a directory, then:

   ```bash
   JARVIS_WAKE_NOISE_CORPUS=/path/to/corpus JARVIS_AGENT_WAKE_WORD=hey_jarvis \
     cargo test -p jarvis-agent --features wake-word-onnx --test wake_onnx the_false_accept_rate -- --nocapture
   ```

   It fails the build if the rate exceeds one per hour (`M10-acceptance.md` §2.3). Given the
   7.4/hour real-room reading for a different word, do not be surprised if this fails on the
   first run — that is what the gate is for.

Plus two decisions only the owner can make: where F10.9 sits relative to F10.7 in the record
(it already landed after F10.7 in practice; this is a documentation call, not a re-ordering),
and whether the `/etc/jarvis/secrets.env` fallback for systemd system services is an accepted,
documented exception to invariant 5 or a defect to fix before sign-off.

Items 1–3 *are* this milestone's exit evidence. They cannot be delegated to a report — that is
not a gap in the work, it is what a gate is for.
