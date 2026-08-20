# Try it — from a clone to talking to it

Every command here was executed in this order on a Linux machine and produced
the output shown (F10.1, 2026-08-19). Where something cannot be verified from a
terminal, it says so rather than pretending.

This is the *development* path — one machine, loopback, no TLS. The production
install (systemd units, TLS, LAN) is `docs/09-operations.md` and
`infra/jarvisd.toml.example`; the first-run **check** for that path is
`infra/install/first-run.sh`.

**Time:** about 20 minutes, most of it waiting for a release build and a model
download.

---

## 0. What you need

- `curl`, `git`, `ca-certificates` — to fetch the source and the toolchain at all
- **`build-essential`** (or your distribution's C toolchain) — Rust needs a C linker,
  and without it the very first build stops at ``error: linker `cc` not found``
- **`libasound2-dev`** — only for `jarvis-agent`; ALSA headers for the microphone.
  `jarvisd` alone does not need it
- Rust (the pinned toolchain installs itself from `rust-toolchain.toml`)
- Docker or Podman with compose
- **Node ≥ 22.22 / 24** for the web shell — Debian and Ubuntu's `apt` Node is v20 and
  the Angular CLI refuses it by name, so install from nodejs.org or nvm
- A microphone and speakers, if you want the hands-free part

On a Debian-family machine, that is:

```bash
sudo apt-get install -y build-essential curl git ca-certificates libasound2-dev
```

This list was produced by **actually running the install on a machine that had none of
it** — a clean `debian:13` container, no toolchain, no build cache — and recording where
it stopped. `pkg-config`, `libssl-dev` and `cmake` are deliberately *not* here: TLS is
rustls throughout, so nothing links against OpenSSL.

## 1. Start Postgres, and the speech services

```bash
docker compose -f infra/compose/dev.yml up -d postgres
docker compose -f infra/compose/voice.yml up -d          # Whisper + Piper
```

The **first** start of `wyoming-whisper` downloads the `base-int8` model
(~150 MB) and needs working internet **inside the container**. If it logs
`LocalEntryNotFoundError`, the container has no network — that is the usual
rootless-Podman symptom, and everything except speech-to-text still works.

```bash
export DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis
cargo sqlx migrate run     # or: sqlx migrate run
```

## 2. Build and start the daemon

```bash
cargo build --release -p jarvisd
(cd web && npm ci && npm run build)

export JARVIS_DB_URL="$DATABASE_URL"
export JARVIS__STORAGE__ARTIFACTS_ROOT="$PWD/.jarvis-artifacts"
export JARVIS__SERVER__WEB_ASSETS="$PWD/web/dist/jarvis-shell/browser"
export JARVIS__VOICE__ENABLED=true
export JARVIS__VOICE__WYOMING_STT=tcp://127.0.0.1:10300
export JARVIS__VOICE__WYOMING_TTS=tcp://127.0.0.1:10200

./target/release/jarvisd
```

Check it, in another terminal:

```bash
curl -s http://127.0.0.1:8741/api/v1/diagnostics/health | python3 -m json.tool
```

```json
{ "status": "ok", "version": "0.1.0",
  "adapters": { "database": { "state": "up" } },
  "pairingCode": "876-635",
  "paired": false }
```

`pairingCode` appears **only** until an owner exists, and only on loopback
(docs/05 §6.2). `paired: false` is what the install check reads.

## 3. Pair your browser

Open <http://127.0.0.1:8741/>. The HUD loads: a presence orb, "Ask me
something.", and a **Hold to speak** button — push-to-talk, which works whether
or not you ever set up a wake word (NFR-11 requires a non-voice path).

Go to **⚙ Settings** (or `/settings`). Until this browser is paired it says so,
and tells you to fetch the pairing code from the health page above.

To pair it, take the `pairingCode` from step 2:

```bash
curl -s -X POST http://127.0.0.1:8741/api/v1/auth/pair \
  -H 'Content-Type: application/json' \
  -d '{"pairingCode":"876-635","deviceName":"my laptop"}'
```

The response's `deviceToken` is the **only** time that token crosses the wire.
The shell keeps its own in `localStorage`; for `curl`, save it:

```bash
export OWNER_TOKEN=...   # deviceToken from the response
```

## 4. Add a satellite that answers to its name

Provision the wake-word models (never vendored — ADR-032 checks each against a
pinned SHA-256 and installs nothing unless every file matches):

```bash
infra/install/fetch-wake-assets.sh
cargo build --release -p jarvis-agent --features wake-word-onnx
```

Open a pairing window as the owner, then pair the node with the code it prints:

```bash
curl -s -X POST http://127.0.0.1:8741/api/v1/devices/pairing-window \
  -H "Authorization: Bearer $OWNER_TOKEN" -H 'Content-Type: application/json' -d '{}'
# -> {"pairingCode":"615-093","expiresAt":"..."}

./target/release/jarvis-agent pair \
  --server http://127.0.0.1:8741 --name kitchen --class room-node
# it prompts for the code; it is never taken from an argument or the environment
```

```
Paired. The daemon assigned the class `room-node`.
Credentials stored in the OS keyring.
```

Run it:

```bash
./target/release/jarvis-agent run
```

```
audio output ready device=default
microphone ready device=default muted=false
wake word active: this node answers to its name  wake_word=hey jarvis
connected to jarvisd; listening for directives
```

**Say "hey jarvis".** The node logs `wake word detected` and opens a capture
stream. Nothing is streamed before that line appears — that is the property
ADR-032 §2 exists for, and it is asserted at the socket in the test suite.

If you would rather not say it out loud, play a recording into the room:

```bash
JARVIS_AGENT_WAKE_WORD=alexa ./target/release/jarvis-agent run   # in one terminal
paplay ~/.cache/jarvis-wake-assets/alexa_test.wav                # in another
```

That is exactly how the hands-free path was verified: played aloud, heard by a
real microphone, detected by the engine on the node.

## 4b. Check what it can actually do

Before wondering why it will not research anything, ask it:

```bash
curl -s http://127.0.0.1:8741/api/v1/diagnostics/health | python3 -m json.tool
```

```
database         up
home-assistant   disabled  set [integrations.home_assistant] enabled
tools            up        2 registered: example.light, message.send
voice-stt        up
voice-tts        up
web-search       disabled  set [integrations.web_search] — without it nothing can research, …
```

**`disabled` means nobody configured it, not that it is broken** — that is the whole reason
the distinction is reported. Read `tools` first: it is the honest answer to "what can this
thing do", and a registry smaller than you expected is the fastest sign an integration
failed to register.

The default install is deliberately near-empty. Optional capabilities are opt-in, so a
fresh daemon has **two** tools and cannot search the web, control lights, or play anything
until you turn those on.

### "It answers, but it never looks anything up"

Expected on a default install: `web.search` and `web.fetch` register **only** when a
provider is configured (needs a Brave API key). This also explains a second symptom that
looks unrelated — the **sources and gallery cards never appear**, because they are
projected from a research thread, and without search there is no thread.

```toml
[integrations.web_search]
enabled = true
provider = "brave"
api_key_secret = "keyring:jarvis/websearch-key"
```

### "It answers in text but never speaks"

Check `voice-tts` above. `disabled` means `[voice] enabled` or `wyoming_tts` is unset. If
it says `up`, the daemon is synthesizing and the problem is downstream — start it with
`RUST_LOG=debug` and watch for the speech path, and check the browser console, since
playback needs a live `AudioContext`.

## 5. What you should see

| | |
|---|---|
| `GET /` | the HUD, served by `jarvisd` |
| `/settings` | devices, automations, and voice — wake word, ElevenLabs toggle, spend |
| `jarvis-agent run` | `wake word detected` when you say the word |
| `curl /api/v1/devices` with `$OWNER_TOKEN` | your browser and your node, with the scopes each holds |

Note what the node is **not** given: a `room-node` holds `display-agent` and
`voice-capture`, never `ui`, and `executesTools` is false. A satellite cannot
enumerate its siblings, read the household's spend, or run a tool.

## 6. Stopping

```bash
# Ctrl-C, or from another terminal — both drain properly
kill $(pgrep -x jarvis-agent)
kill $(pgrep -x jarvisd)
docker compose -f infra/compose/voice.yml down    # frees ~1 GB of resident model
```

---

## Known rough edges, as of F10.1

Stated plainly because you are about to hit them, not to be defensive:

- **Speech-to-text needs the container to reach the internet once.** If Whisper
  logs `LocalEntryNotFoundError` it never downloaded its model. Wake word,
  pairing, the shell, timers and automations all work regardless; only
  transcription is affected.
- **`room-node` expects a compositor.** The class that owns a screen requires
  Hyprland (`XDG_RUNTIME_DIR` + `HYPRLAND_INSTANCE_SIGNATURE`). For a
  speaker-only satellite — a Pi in a kitchen — use `--class voice-node`.
- **A listening node costs about 8.6% of a fast desktop core**, of which the
  wake pipeline itself is 2.2%. Measured, and inside ADR-032's budget on this
  class of machine; not yet measured on a Pi.
- **The wake word is `hey jarvis`.** openWakeWord publishes models for six words
  only, and "Andy" is not among them (ADR-032 §1). Any other word needs a model
  trained for it; a node configured for a word it has no model for says so at
  startup and falls back to push-to-talk rather than failing.
- **Creating an automation is API-only.** The settings surface lists them,
  enables, disables and shows history (M8b D1).

## Lost your device token?

The `deviceToken` crosses the wire exactly once, so if you lose it there is
nothing to look up. Restart `jarvisd` with the recovery flag set:

```bash
JARVIS_RECOVER_PAIRING=1 ./target/release/jarvisd
```

The journal logs a fresh pairing code and the loopback health page shows it
again. Pair a replacement device the ordinary way, then revoke the lost one from
it — recovery **adds** a way in rather than clearing anything, so your existing
devices, nodes, timers and automations keep working throughout.

Restart without the flag once you are back in. The flag is gated on being able
to restart the daemon, which is host access — strictly stronger than the
loopback access the first-run ceremony already trusts, and no more than someone
with a shell could do by reading the database directly.
