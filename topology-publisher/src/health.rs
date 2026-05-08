//! Tiny `/healthz` HTTP endpoint + systemd watchdog ping.
//!
//! The daemon already publishes its state into the topology contract,
//! but operators want a *local* readout that doesn't depend on the WS
//! contract round-trip — for monitoring scrapers, ad-hoc curl checks,
//! and to back the systemd `WatchdogSec=` killswitch.
//!
//! The HTTP side is a hand-rolled HTTP/1.1 server: a single GET path,
//! plain TCP, no TLS, no routing framework. We accept on
//! `127.0.0.1:<port>` so the endpoint doesn't leak across the LAN.
//!
//! `tokio::sync::watch` carries the latest snapshot from the publish
//! loop to the HTTP handler — single writer (publish loop), many
//! readers (one per accepted connection), no contention, last write
//! wins. That matches the semantics we want: every reader sees the
//! freshest snapshot, no buffer queueing.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
};
use tracing::{debug, warn};

/// Single immutable snapshot of the daemon's last-known state.
/// Serialised verbatim as the body of `GET /healthz`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct HealthSnapshot {
    /// Wall-clock unix seconds when the most-recent publish succeeded;
    /// `0` when no successful publish has happened yet (startup, or
    /// continuous failure since boot).
    pub last_publish_unix: u64,
    /// Seconds elapsed since the last successful publish at the time
    /// the snapshot is read. Convenience for monitoring scrapers that
    /// don't want to do the subtraction themselves; recomputed per
    /// request from `last_publish_unix`.
    pub last_publish_secs_ago: u64,
    /// `true` while a WS session is live (subscribed) — flips to
    /// `false` while the daemon is sleeping in reconnect-backoff.
    pub session_alive: bool,
    /// Cardinalities from the most-recent published payload, mirroring
    /// the daemon's `info!` log line so a `curl /healthz` is a
    /// drop-in replacement for tailing the journal.
    pub last_peer_count: usize,
    pub last_contract_count: usize,
    pub last_webapp_count: usize,
    pub probed_total: usize,
}

impl HealthSnapshot {
    pub fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Background task: serve `GET /healthz` forever, replying with the
/// JSON-serialised contents of `rx.borrow()`. Returns `Err` only on
/// fatal bind/listener failures — per-connection errors are logged
/// and the listener keeps accepting.
pub async fn run_health_server(port: u16, rx: watch::Receiver<HealthSnapshot>) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("bind 127.0.0.1:{port}"))?;
    debug!(port, "health server listening on 127.0.0.1");
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "health-server accept failed; continuing");
                continue;
            }
        };
        let rx = rx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_one(stream, rx).await {
                debug!(error = %e, "health-server connection error");
            }
        });
    }
}

async fn handle_one(
    mut stream: tokio::net::TcpStream,
    rx: watch::Receiver<HealthSnapshot>,
) -> Result<()> {
    // Read just enough of the request to find the path. We don't
    // honour Content-Length, Keep-Alive, or anything fancy — the
    // endpoint is for cheap localhost polling.
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    while total < buf.len() {
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let req_line = std::str::from_utf8(&buf[..total])
        .ok()
        .and_then(|s| s.lines().next())
        .unwrap_or("");
    let path = req_line.split_whitespace().nth(1).unwrap_or("/");

    let mut snap = rx.borrow().clone();
    snap.last_publish_secs_ago = if snap.last_publish_unix == 0 {
        0
    } else {
        HealthSnapshot::now_secs().saturating_sub(snap.last_publish_unix)
    };

    let (status, content_type, body) = match path {
        "/healthz" | "/healthz/" => (
            "200 OK",
            "application/json",
            serde_json::to_string(&snap).unwrap_or_default(),
        ),
        "/metrics" | "/metrics/" => (
            "200 OK",
            "text/plain; version=0.0.4",
            render_prometheus(&snap),
        ),
        "/" | "" => (
            "200 OK",
            "text/plain; charset=utf-8",
            "topology-publisher health endpoint\n\
             /healthz  - JSON snapshot\n\
             /metrics  - Prometheus text exposition\n"
                .to_string(),
        ),
        _ => ("404 Not Found", "text/plain", "not found".to_string()),
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

/// Render the snapshot in the Prometheus text exposition format
/// (version 0.0.4). One gauge per snapshot field, plus a build-info
/// metric so a scraper can join versions across upgrades. We don't
/// include counters because the daemon doesn't accumulate over its
/// lifetime — every gauge is "last cycle's value".
fn render_prometheus(snap: &HealthSnapshot) -> String {
    let mut s = String::with_capacity(1024);
    let metric = |s: &mut String, name: &str, help: &str, value: u64| {
        s.push_str("# HELP ");
        s.push_str(name);
        s.push(' ');
        s.push_str(help);
        s.push('\n');
        s.push_str("# TYPE ");
        s.push_str(name);
        s.push_str(" gauge\n");
        s.push_str(name);
        s.push(' ');
        s.push_str(&value.to_string());
        s.push('\n');
    };
    metric(
        &mut s,
        "topology_publisher_last_publish_unix_seconds",
        "Unix timestamp of the last successful publish; 0 if none yet.",
        snap.last_publish_unix,
    );
    metric(
        &mut s,
        "topology_publisher_seconds_since_last_publish",
        "Seconds elapsed since the last successful publish.",
        snap.last_publish_secs_ago,
    );
    metric(
        &mut s,
        "topology_publisher_session_alive",
        "1 when a WebSocket session is live, 0 during reconnect-backoff.",
        snap.session_alive as u64,
    );
    metric(
        &mut s,
        "topology_publisher_last_peer_count",
        "Peer count in the most-recent published payload.",
        snap.last_peer_count as u64,
    );
    metric(
        &mut s,
        "topology_publisher_last_contract_count",
        "Contract count in the most-recent published payload.",
        snap.last_contract_count as u64,
    );
    metric(
        &mut s,
        "topology_publisher_last_webapp_count",
        "Number of contracts in the last payload that the local probe \
         classified as webapps.",
        snap.last_webapp_count as u64,
    );
    metric(
        &mut s,
        "topology_publisher_probed_total",
        "Total number of contract keys currently held in the probe cache \
         (probed at least once since startup or reconnect).",
        snap.probed_total as u64,
    );
    s.push_str("# HELP topology_publisher_build_info Static info about the running publisher build.\n");
    s.push_str("# TYPE topology_publisher_build_info gauge\n");
    s.push_str(&format!(
        "topology_publisher_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));
    s
}

/// Send a `WATCHDOG=1` notification to systemd, if `$NOTIFY_SOCKET` is
/// set (i.e. we're actually running under a systemd unit with
/// `WatchdogSec=`). No-op outside systemd. Errors are logged at debug
/// level and otherwise ignored — a missed watchdog ping is not worth
/// crashing the publisher over.
pub fn ping_watchdog() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]) {
        debug!(error = %e, "sd_notify watchdog ping failed");
    }
}

/// Send a `READY=1` notification to systemd. Required exactly once at
/// startup when the unit declares `Type=notify`: until this fires
/// systemd holds the unit in "starting" state and refuses to consider
/// `WatchdogSec=` armed. After this, any subsequent `WATCHDOG=1`
/// pings actually reset the watchdog timer.
pub fn notify_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        debug!(error = %e, "sd_notify READY=1 failed (likely not running under systemd)");
    }
}
