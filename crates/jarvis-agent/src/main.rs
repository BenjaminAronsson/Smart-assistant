#![deny(unsafe_code)]
//! Jarvis node agent (docs/02 §8/§12, FR-09/FR-10/FR-19): a paired client that
//! places Jarvis surfaces on monitors via Hyprland, and — from F8.2 — carries a
//! room's audio. It exposes a narrow, closed command set — **it is not a
//! shell**.
//!
//! A node holds its own Ed25519 identity, pairs through the owner-mediated
//! ceremony in ADR-031, pins the daemon's certificate, and keeps its token in
//! the OS keyring (or a 0600 file where there is no keyring).
//!
//! **There is no `JARVIS_AGENT_TOKEN`.** It was M3a's stopgap and it was the
//! last place a node credential sat in the clear: an environment variable is
//! readable from `/proc`, inherited by every child, and routinely captured in
//! process supervisors' logs and crash reports. Credentials now come from
//! pairing, and only from pairing (invariant 5).
//!
//! Hyprland is discovered from `XDG_RUNTIME_DIR` + `HYPRLAND_INSTANCE_SIGNATURE`
//! — and is required only for the classes that actually own a screen.

use anyhow::{Context, Result};

use jarvis_agent::cli::{self, Command};
use jarvis_agent::compositor::{self, HyprctlClient};
use jarvis_agent::identity::NodeKey;
use jarvis_agent::store::{CredentialStore, KeyringStore};
use jarvis_agent::{client, pairing};

/// Exit code for a node whose device was revoked. Distinct from a crash so a
/// supervisor can stop restarting it (docs/09): revocation is a decision, not a
/// fault, and `Restart=on-failure` should leave it stopped.
const EXIT_REVOKED: i32 = 3;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match cli::parse(std::env::args().skip(1))? {
        Command::Help => {
            println!("{}", cli::USAGE);
            Ok(())
        }
        Command::Pair {
            server,
            name,
            class,
        } => pair(&server, &name, &class).await,
        Command::Run => run().await,
        Command::Reset => {
            blocking(|| KeyringStore::open()?.clear()).await?;
            println!("credentials cleared; this node is no longer paired");
            Ok(())
        }
    }
}

async fn pair(server: &str, name: &str, class: &str) -> Result<()> {
    // Every credential-store call is blocking D-Bus or filesystem I/O, so it
    // runs on a blocking thread rather than stalling the executor.
    let store = blocking(KeyringStore::open).await?;
    if blocking_with(&store, |store| store.load()).await?.is_some() {
        anyhow::bail!(
            "this node is already paired; revoke it in the shell and run \
             `jarvis-agent reset` before pairing again"
        );
    }

    println!("Pairing {name} ({class}) with {server}");
    println!("Open a pairing window in the Jarvis shell, then type the code it shows.");
    let code = prompt_for_code()?;

    let credentials = pairing::pair(server, name, class, &code).await?;
    // The class is reported back because the server assigns it — telling the
    // owner what they got is not the same as telling them what they asked for.
    let assigned = credentials.device_class.clone();
    blocking_with(&store, move |store| store.save(&credentials)).await?;

    println!("Paired. The daemon assigned the class `{assigned}`.");
    println!("Credentials stored in the {}.", store.backend());
    println!("Start this node with `jarvis-agent run`.");
    Ok(())
}

/// Runs a blocking store operation off the async executor.
async fn blocking<T, F>(operation: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .context("credential store task panicked")?
}

/// The same, for an operation that borrows the store.
///
/// The store is cloned into the blocking task, which is cheap: it holds a path
/// or nothing at all. Cloning rather than borrowing is what lets the closure be
/// `'static`, which `spawn_blocking` requires.
async fn blocking_with<T, F>(store: &KeyringStore, operation: F) -> Result<T>
where
    F: FnOnce(&KeyringStore) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let store = store.clone();
    tokio::task::spawn_blocking(move || operation(&store))
        .await
        .context("credential store task panicked")?
}

/// Reads the one-time code from the terminal. Never an argument, never an
/// environment variable (invariant 5).
fn prompt_for_code() -> Result<String> {
    use std::io::{BufRead as _, Write as _};

    print!("Pairing code: ");
    std::io::stdout().flush().context("flushing the prompt")?;
    let mut code = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut code)
        .context("reading the pairing code")?;
    let code = code.trim().to_owned();
    if code.is_empty() {
        anyhow::bail!("no pairing code entered");
    }
    Ok(code)
}

async fn run() -> Result<()> {
    let store = blocking(KeyringStore::open).await?;
    let credentials = blocking_with(&store, |store| store.load()).await?.context(
        "this node is not paired — run `jarvis-agent pair --server <url> --name <name>` first",
    )?;
    // Load the key at startup rather than at next-pair time. It is not used to
    // authenticate — ADR-031 §3 keeps the token as the per-request credential —
    // but a node whose stored key has rotted should say so now, while the owner
    // is watching, not months later when they try to re-pair it. The
    // fingerprint is the value the owner can compare against the device list.
    let key = NodeKey::from_seed_base64(&credentials.private_key)
        .context("this node's stored identity key is unreadable; re-pair it")?;
    tracing::info!(
        server = %credentials.server_url,
        class = %credentials.device_class,
        key_fingerprint = %key.fingerprint(),
        pinned = credentials.server_fingerprint.is_some(),
        backend = store.backend(),
        "jarvis-agent starting"
    );

    // The class the *server* assigned decides what this node needs, which is
    // why there is no `--node` role flag with teeth: a client is told its
    // authority, it never infers it (docs/05 §6.3). A `voice-node` has no
    // screen, so demanding a compositor from it would be a bug, not a check.
    let compositor = match credentials.device_class.as_str() {
        "voice-node" => None,
        _ => Some(HyprctlClient::from_env().context(
            "this node's class owns a screen, but it is not running under Hyprland \
             (XDG_RUNTIME_DIR / HYPRLAND_INSTANCE_SIGNATURE unset)",
        )?),
    };

    // Ctrl-C / SIGTERM flips the shutdown watch so the client loop drains.
    // Intentionally detached, untracked work (invariant 4): a process-lifetime
    // signal listener with nothing to drain — it self-terminates on the first
    // signal and the awaited client loop below is the real shutdown join point.
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = tx.send(true);
        }
    });

    let revoked = match &compositor {
        Some(compositor) => client::run(&credentials, compositor, rx).await?,
        None => client::run(&credentials, &compositor::NoCompositor, rx).await?,
    };

    if revoked {
        // Clean, deliberate, and loud: the owner revoked this node, so it stops
        // rather than retrying, and says what to do about it.
        tracing::error!("this device was revoked; stopping");
        eprintln!(
            "This node's device was revoked. Run `jarvis-agent reset`, then pair again \
             if you want it back."
        );
        std::process::exit(EXIT_REVOKED);
    }
    Ok(())
}
