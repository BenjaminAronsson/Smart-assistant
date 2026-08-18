//! `cargo xtask perf`: the milestone-gate performance harnesses.
//!
//! * `--rss` — the ultrabook resource budget (docs/01 §4.1, NFR-15). Flagged in
//!   the M2 gate carryforward as "no RSS harness exists — build before M4/M5";
//!   this is that harness.
//! * `--voice` — the NFR-04 voice pipeline latency harness (F5.2), which runs
//!   the `voice_latency` integration test against **fixture** Wyoming services
//!   and reports the daemon's own share of the transcript / first-audio budgets.
//!   See [`voice`] for what that number is and is not.
//!
//! `--rss` specifics follow.
//!
//! It measures two things about the real `jarvisd` binary, not a proxy for it:
//!   - cold start to healthy (NFR-15: < 2 s), timed from process spawn to the
//!     first `200 OK` from `GET /api/v1/diagnostics/health`;
//!   - idle RSS a couple of seconds after that (docs/01 §4.1: 40-80 MB typical,
//!     ≤120 MB hard ceiling), read straight from `/proc/<pid>/status` — Linux
//!     only, which matches this dev host and the deployment target.
//!
//! A release build is used deliberately: debug binaries run noticeably heavier
//! and would not be representative of the number this gate is checking.
//!
//! Requires a **live Postgres** reachable at `DATABASE_URL` (the same variable
//! the sqlx offline-cache workflow and `#[sqlx::test]` DB tests use, docs/09 §1
//! / root `CLAUDE.md`). This command does not start Postgres for you — a
//! `cargo xtask` subcommand silently hanging on a `compose up` is worse than a
//! precise error — so start it first:
//! `docker compose -f infra/compose/dev.yml up -d postgres` (or `podman-compose`
//! on hosts using podman, per `dev-host-podman-postgres` notes).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;

/// `[server].bind` default (`crates/jarvisd/src/config.rs` `ServerConfig`
/// default in `impl Default for Config`) — loopback-only, M0-M2 (docs/06 §7).
const BIND_ADDR: &str = "127.0.0.1:8741";
const HEALTH_PATH: &str = "/api/v1/diagnostics/health";

/// NFR-15.
const COLD_START_BUDGET: Duration = Duration::from_secs(2);
/// docs/01 §4.1 `jarvisd` row: "Idle" band and hard ceiling, in MB.
const IDLE_RSS_TYPICAL_MB: f64 = 80.0;
const IDLE_RSS_CEILING_MB: f64 = 120.0;

/// How long to let the daemon sit idle before sampling RSS. `jarvisd` is a
/// steady-state daemon with no startup-adjacent background work that would
/// need longer to settle (the outbox dispatcher is event-driven via
/// LISTEN/NOTIFY, the health-poll loop's idle interval is 5 minutes) — a
/// couple of seconds past "healthy" is enough for allocator/runtime warm-up to
/// finish.
const SETTLE_DURATION: Duration = Duration::from_secs(2);

/// Generous relative to the 2 s NFR-15 budget on purpose: a slow cold start is
/// gate-relevant evidence to report, not a reason for this harness to give up
/// before it can measure the number.
const HEALTH_POLL_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(mode: Option<&str>) -> anyhow::Result<()> {
    match mode {
        Some("--rss") => rss(),
        Some("--voice") => voice(),
        Some("--voice-real") => voice_real(),
        _ => anyhow::bail!("usage: cargo xtask perf <--rss|--voice|--voice-real>"),
    }
}

/// `cargo xtask perf --voice`: the NFR-04 voice-latency harness (F5.2).
///
/// Delegates to the `voice_latency` integration test rather than re-implementing
/// a WebSocket client and the Wyoming framing here — that test already drives
/// the real daemon against fixture speech services, and duplicating it in xtask
/// would mean two harnesses that can disagree. This wrapper is the operator
/// front door: it states plainly what is and is not being measured, runs the
/// harness in **release** (a debug build is not representative, same reason as
/// `--rss`), and exits non-zero only when the overhead budget is genuinely
/// breached.
///
/// **This does not produce the NFR-04 number.** The STT/TTS models are fixtures
/// here, so what is measured is the daemon's own share of the budget. Validating
/// NFR-04 itself needs the reference machine with real faster-whisper/Piper
/// services on it (docs/02 §9, docs/08 §6) — that measurement, and the resulting
/// model-size/budget decision, belongs in the M5 gate report, not in a harness
/// that would otherwise have to invent a figure.
fn voice() -> anyhow::Result<()> {
    let database_url = std::env::var("DATABASE_URL").context(
        "DATABASE_URL is not set — export it, e.g. \
         DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis",
    )?;
    preflight_postgres(&database_url)?;

    let root = workspace_root()?;
    println!("perf --voice: NFR-04 voice pipeline latency (docs/01 §4.1, docs/02 §9)");
    println!(
        "perf --voice: fixture Wyoming STT/TTS — MODEL TIME EXCLUDED; this measures the daemon's"
    );
    println!("perf --voice: own share of the 0.8s transcript / 1.2s first-audio budgets.");
    println!("perf --voice: building and running the harness in release mode...");

    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "test",
            "-p",
            "jarvisd",
            "--release",
            "--test",
            "voice_latency",
            "--",
            "--nocapture",
        ])
        .current_dir(&root)
        .env("DATABASE_URL", &database_url)
        .status()
        .context("failed to run the voice latency harness")?;
    anyhow::ensure!(
        status.success(),
        "perf --voice: the voice pipeline overhead budget was exceeded — see the report above"
    );
    println!();
    println!(
        "perf --voice: PASS — daemon-side overhead is within its share of NFR-04. Record the \
         reference-hardware end-to-end figures (real faster-whisper/Piper) in the M5 gate report."
    );
    Ok(())
}

/// `cargo xtask perf --voice-real`: the **actual** NFR-04 figures (D-M5-3).
///
/// The other half of `--voice`. That one deliberately excludes the speech
/// models so it runs anywhere; this one includes them, which is the only way to
/// produce the numbers docs/01 §4.1 actually budgets — and the reason D-M5-3
/// stayed open from M5 until a harness existed to close it.
///
/// Needs the Wyoming services up:
///
/// ```text
/// docker compose -f infra/compose/dev.yml -f infra/compose/voice.yml up -d
/// cargo xtask perf --voice-real
/// ```
///
/// It measures **the machine it runs on**. NFR-04 is specified on the 8 GB
/// reference profile, so record which machine produced the figure — a pass on a
/// workstation is evidence the pipeline is sane, not that the budget holds on
/// the target.
fn voice_real() -> anyhow::Result<()> {
    let database_url = std::env::var("DATABASE_URL").context(
        "DATABASE_URL is not set — export it, e.g. \
         DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis",
    )?;
    preflight_postgres(&database_url)?;

    let root = workspace_root()?;
    println!("perf --voice-real: NFR-04 end to end, REAL faster-whisper + Piper (D-M5-3).");
    println!("perf --voice-real: final transcript < 0.8 s after end of speech;");
    println!("perf --voice-real: first audio < 1.2 s after the response text begins.");
    println!("perf --voice-real: this measures THIS machine, not the 8 GB reference profile.");

    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args([
            "test",
            "-p",
            "jarvisd",
            "--release",
            "--test",
            "voice_latency_real",
            "--",
            "--nocapture",
        ])
        .current_dir(&root)
        .env("DATABASE_URL", &database_url)
        .env("JARVIS_NFR04_REAL", "1")
        .status()
        .context("failed to run the real voice latency harness")?;
    anyhow::ensure!(
        status.success(),
        "perf --voice-real: NFR-04 was exceeded on this machine — see the report above"
    );
    println!();
    println!("perf --voice-real: PASS — record the figures and the machine in the gate report.");
    Ok(())
}

fn rss() -> anyhow::Result<()> {
    let database_url = std::env::var("DATABASE_URL").context(
        "DATABASE_URL is not set — export it, e.g. \
         DATABASE_URL=postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis",
    )?;
    preflight_postgres(&database_url)?;

    let root = workspace_root()?;
    println!(
        "perf --rss: building jarvisd in release mode (a debug build is not representative)..."
    );
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "jarvisd", "--release"])
        .current_dir(&root)
        .status()
        .context("failed to run cargo build")?;
    anyhow::ensure!(status.success(), "cargo build -p jarvisd --release failed");

    let binary = root.join("target/release/jarvisd");
    anyhow::ensure!(
        binary.exists(),
        "release binary not found at {} after a successful build",
        binary.display()
    );

    println!("perf --rss: starting jarvisd ({})...", binary.display());
    let child = Command::new(&binary)
        // `DatabaseConfig::url_secret` defaults to `env:JARVIS_DB_URL`
        // (`Config::default()`); the secret reference is resolved at the
        // adapter boundary (invariant 5) — this is the env var it resolves.
        .env("JARVIS_DB_URL", &database_url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn jarvisd")?;
    let pid = child.id();
    let spawned_at = Instant::now();

    let addr: SocketAddr = BIND_ADDR
        .parse()
        .expect("BIND_ADDR is a hardcoded valid loopback address");

    let cold_start = match wait_for_health(addr, HEALTH_PATH, HEALTH_POLL_TIMEOUT, spawned_at) {
        Ok(elapsed) => elapsed,
        Err(error) => {
            terminate(child);
            return Err(error);
        }
    };
    println!(
        "perf --rss: healthy after {:.3}s — settling {:.0}s before sampling RSS...",
        cold_start.as_secs_f64(),
        SETTLE_DURATION.as_secs_f64()
    );
    std::thread::sleep(SETTLE_DURATION);

    let rss_result = read_rss_kb(pid);
    // Always terminate, whether the measurement succeeded or not — a failed
    // read must not leak an orphaned jarvisd.
    terminate(child);
    let rss_kb = rss_result?;
    let rss_mb = rss_kb as f64 / 1024.0;

    println!();
    println!("=== jarvisd perf report (docs/01 §4.1, NFR-15) ===");
    println!(
        "cold start to healthy: {:.3}s (budget: <2s)",
        cold_start.as_secs_f64()
    );
    println!(
        "idle RSS: {rss_mb:.1} MB (typical 40-{IDLE_RSS_TYPICAL_MB:.0} MB, hard ceiling {IDLE_RSS_CEILING_MB:.0} MB)"
    );

    let mut within_budget = true;
    if cold_start > COLD_START_BUDGET {
        println!(
            "FAIL: cold start {:.3}s exceeds the 2s budget (NFR-15)",
            cold_start.as_secs_f64()
        );
        within_budget = false;
    } else {
        println!("PASS: cold start is within the 2s budget (NFR-15)");
    }

    if rss_mb > IDLE_RSS_CEILING_MB {
        println!(
            "FAIL: idle RSS {rss_mb:.1} MB exceeds the {IDLE_RSS_CEILING_MB:.0} MB hard ceiling (docs/01 §4.1)"
        );
        within_budget = false;
    } else if rss_mb > IDLE_RSS_TYPICAL_MB {
        println!(
            "WARN: idle RSS {rss_mb:.1} MB is above the {IDLE_RSS_TYPICAL_MB:.0} MB typical band but within the {IDLE_RSS_CEILING_MB:.0} MB hard ceiling (docs/01 §4.1)"
        );
    } else {
        println!(
            "PASS: idle RSS {rss_mb:.1} MB is at or below the 40-{IDLE_RSS_TYPICAL_MB:.0} MB typical band (docs/01 §4.1)"
        );
    }

    anyhow::ensure!(
        within_budget,
        "perf --rss: budget exceeded — see report above"
    );
    Ok(())
}

/// Fail fast with an operator-actionable message rather than let the health
/// poll below time out for the wrong reason.
fn preflight_postgres(database_url: &str) -> anyhow::Result<()> {
    let host_port = host_port_of(database_url)?;
    TcpStream::connect(&host_port).map_err(|error| {
        anyhow::anyhow!(
            "Postgres at {host_port} is not reachable ({error}) — start it first: \
             `docker compose -f infra/compose/dev.yml up -d postgres` (or the podman \
             equivalent), then re-run `cargo xtask perf --rss`"
        )
    })?;
    Ok(())
}

/// `postgres://user:pass@host:port/db` -> `host:port`. Deliberately minimal —
/// this only needs to support the one DATABASE_URL shape documented in
/// `CLAUDE.md` and `infra/compose/dev.yml`, not general connection-string
/// parsing.
fn host_port_of(database_url: &str) -> anyhow::Result<String> {
    let after_scheme = database_url
        .split("://")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("DATABASE_URL {database_url:?} is not a postgres:// URL"))?;
    let after_at = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    let host_port = after_at.split('/').next().unwrap_or(after_at);
    anyhow::ensure!(
        !host_port.is_empty(),
        "DATABASE_URL {database_url:?} has no host:port"
    );
    Ok(host_port.to_owned())
}

/// Poll the health endpoint with a plain HTTP/1.1 GET over a raw `TcpStream`
/// (no HTTP client dependency needed for a 200-or-not check) until it answers
/// `200`, or `timeout` elapses. Returns the elapsed time since `spawned_at` —
/// the cold-start-to-healthy latency NFR-15 budgets.
fn wait_for_health(
    addr: SocketAddr,
    path: &str,
    timeout: Duration,
    spawned_at: Instant,
) -> anyhow::Result<Duration> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    loop {
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .ok();
            stream
                .set_write_timeout(Some(Duration::from_millis(500)))
                .ok();
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut buf = Vec::new();
                let _ = stream.read_to_end(&mut buf);
                let response = String::from_utf8_lossy(&buf);
                if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
                    return Ok(spawned_at.elapsed());
                }
            }
        }
        if spawned_at.elapsed() > timeout {
            anyhow::bail!(
                "jarvisd did not answer 200 on {addr}{path} within {:.1}s — it may have failed \
                 to start (check config / DATABASE_URL) or is unhealthy",
                timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `VmRSS` from `/proc/<pid>/status`, in kB — the same field `ps`/`htop`
/// report as RSS. Linux-only, matching this dev host and the deployment
/// target (docs/09 §5).
fn read_rss_kb(pid: u32) -> anyhow::Result<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("reading /proc/{pid}/status — has jarvisd already exited?"))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .with_context(|| format!("VmRSS line malformed: {line:?}"))?;
            return Ok(kb);
        }
    }
    anyhow::bail!("VmRSS not found in /proc/{pid}/status")
}

/// Graceful shutdown first (`jarvisd`'s own `SIGTERM` handler drains runs and
/// the dispatcher, docs/02 §12), falling back to `SIGKILL` if it does not exit
/// promptly — either way, no orphaned process is left behind.
fn terminate(mut child: Child) {
    let pid = child.id();
    let sent = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .is_ok();
    if sent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    // Mirrors `codegen::workspace_root` in `main.rs` — xtask always runs via
    // `cargo xtask` from within the workspace.
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("cannot locate workspace root"))?
        .to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_extracts_from_the_documented_dev_url() {
        assert_eq!(
            host_port_of("postgres://jarvis:jarvis-dev-only@127.0.0.1:5432/jarvis").unwrap(),
            "127.0.0.1:5432"
        );
    }

    #[test]
    fn host_port_rejects_a_non_postgres_url() {
        assert!(host_port_of("not-a-url").is_err());
    }
}
