//! Off-tab publisher for the Freenet topology contract.
//!
//! Browser dashboards can't auto-discover their host node's actual peer
//! list (CORS blocks `fetch /` from the sandboxed iframe; freenet-core
//! gates `NodeQueries` for webapps). They publish a "skeleton" entry
//! built from manually-entered known_nodes.
//!
//! This daemon runs alongside the freenet node as a normal OS process,
//! so it has *full* access to the client API. It periodically:
//!
//!   1. Sends `ClientRequest::NodeQueries(NodeDiagnostics)` to the local
//!      node — gets the real peer list, this node's location, version.
//!   2. Builds a `shared::contract::EntryPayload`.
//!   3. Signs with a persistent Ed25519 seed (loaded from / written to
//!      `<config-dir>/key.toml`).
//!   4. Pushes a `ContractRequest::Update` over the same WS.
//!
//! The contract therefore has a fresh, accurate entry per host running
//! the daemon — independent of whether anyone has the dashboard open.
//! Subscribers (any dashboard tab on any node) see those entries and
//! render the merged graph.
//!
//! Run as a systemd unit on each operator-controlled freenet node.

use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use ed25519_dalek::{Signer, SigningKey};
use freenet_stdlib::{
    client_api::{
        ClientRequest, ContractRequest, ContractResponse, HostResponse, NeighborHostingInfo,
        NodeDiagnosticsConfig, NodeDiagnosticsResponse, NodeQuery, QueryResponse, WebApi,
    },
    prelude::{CodeHash, ContractInstanceId, ContractKey, StateDelta, UpdateData},
};
use serde::{Deserialize, Serialize};
use shared::contract::{
    encode_contract_entry, ContractDelta, EntryPayload, NeighborInfo, SignedEntry,
};
use tokio::time::{interval, MissedTickBehavior};
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};
use url::Url;

mod health;
mod probe;
use health::HealthSnapshot;
use probe::ProbeCache;
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(
    name = "topology-publisher",
    about = "Periodically publishes this node's topology to a Freenet contract"
)]
struct Cli {
    /// WebSocket URL of the local freenet node (no path; the
    /// `/v1/contract/command?encodingProtocol=native` suffix is
    /// appended automatically). Default: `ws://127.0.0.1:7509`.
    #[arg(long, default_value = "ws://127.0.0.1:7509")]
    node_ws_url: String,

    /// Topology contract instance id (base58).
    #[arg(
        long,
        default_value = "BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE"
    )]
    instance_id: String,

    /// Topology contract code hash (base58, mixed-case is fine — bs58
    /// decodes case-sensitively and the canonical printout is lower).
    #[arg(
        long,
        default_value = "3Ug134jfYzEMkwJeRbTEgY33kgXHKEWnZLvmWi3eoDXV"
    )]
    code_hash: String,

    /// Seconds between publish cycles.
    #[arg(long, default_value_t = 60u64)]
    interval_secs: u64,

    /// Where to load/store the publisher's Ed25519 seed.
    /// On first run a fresh seed is generated and written here so
    /// subsequent runs keep the same publisher slot in the contract.
    #[arg(long)]
    key_file: Option<PathBuf>,

    /// Operator-chosen public display name (e.g. "baka", "orange").
    /// Shipped in `EntryPayload.version` so the dashboard can label this
    /// publisher's gateway with a human-readable string instead of a
    /// pubkey prefix. Purely cosmetic — the contract still identifies
    /// publishers by their Ed25519 pubkey.
    ///
    /// IMPORTANT: this is *public*. Don't pass `$(hostname)` or any
    /// other auto-detected machine identifier — that leaks server
    /// metadata to every dashboard visitor. Pick a friendly nickname
    /// you're comfortable showing to anyone on the network.
    ///
    /// `--label` is the deprecated alias retained for back-compat with
    /// existing systemd units; new deployments should use
    /// `--display-name`.
    #[arg(long, alias = "label")]
    display_name: Option<String>,

    /// Fallback `neighbors` entries (`label,host:port` or just
    /// `host:port`) to publish when `NodeDiagnostics` is unavailable
    /// (e.g. running against a freenet local-mode node). Repeatable.
    /// Network-mode nodes ignore this — they get the real peer list.
    #[arg(long = "neighbor", value_name = "LABEL,HOST:PORT")]
    neighbors: Vec<String>,

    /// Bind a tiny HTTP `/healthz` endpoint on `127.0.0.1:<port>`. A
    /// monitoring scraper or `curl` on the same host can read the
    /// daemon's last-known state as JSON. `0` (default) disables.
    /// Only `127.0.0.1` is bound — health data isn't sensitive but
    /// there's no reason to expose it across the LAN either.
    #[arg(long, default_value_t = 0u16)]
    metrics_port: u16,
}

fn parse_neighbor_arg(s: &str) -> NeighborInfo {
    let trimmed = s.trim();
    let (_label, addr) = match trimmed.split_once(',') {
        Some((l, a)) => (l.trim().to_string(), a.trim().to_string()),
        None => (String::new(), trimmed.to_string()),
    };
    NeighborInfo {
        address: addr,
        location: None,
        is_gateway: true,
    }
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    seed_hex: String,
}

fn default_key_path() -> PathBuf {
    let base = dirs_minimal::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("freenet-net-graph").join("publisher-key.toml")
}

mod dirs_minimal {
    use std::path::PathBuf;
    /// XDG_CONFIG_HOME, else $HOME/.config, else None.
    /// Tiny inline replacement for the `dirs` crate so this binary
    /// doesn't pull a transitive dep just for one path lookup.
    pub fn config_dir() -> Option<PathBuf> {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg));
            }
        }
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config"))
    }
}

fn load_or_create_key(path: &Path) -> Result<SigningKey> {
    if let Ok(bytes) = fs::read_to_string(path) {
        let parsed: KeyFile = toml::from_str(&bytes).context("parse key file")?;
        let raw = hex::decode(parsed.seed_hex.trim()).context("decode seed hex")?;
        if raw.len() != 32 {
            return Err(anyhow!("seed must be 32 bytes; got {}", raw.len()));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&raw);
        return Ok(SigningKey::from_bytes(&seed));
    }
    // First run — generate, persist with 0600 perms.
    use rand::RngCore;
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let sk = SigningKey::from_bytes(&seed);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create key dir")?;
    }
    let kf = KeyFile {
        seed_hex: hex::encode(seed),
    };
    fs::write(path, toml::to_string(&kf).context("encode key file")?)
        .context("write key file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)?.permissions();
        perm.set_mode(0o600);
        fs::set_permissions(path, perm)?;
    }
    info!(path = %path.display(), "generated new publisher key");
    Ok(sk)
}

fn ws_url(base: &str) -> Result<Url> {
    let trimmed = base.trim_end_matches('/');
    let with_path = format!("{trimmed}/v1/contract/command?encodingProtocol=native");
    Url::parse(&with_path).context("invalid ws url")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Skeleton payload — same shape as the browser publisher emits when
/// it can't query peers. Used by the daemon against local-mode nodes
/// (where NodeQueries is rejected). The neighbours list comes from
/// `--neighbor` CLI args; otherwise empty.
fn build_skeleton_payload(
    sk: &SigningKey,
    neighbors: &[NeighborInfo],
    label: Option<&str>,
) -> EntryPayload {
    EntryPayload {
        public_key: sk.verifying_key().to_bytes(),
        external_address: String::new(),
        own_location: None,
        version: label.map(|s| s.to_string()),
        neighbors: neighbors.to_vec(),
        contracts: vec![],
        timestamp_ms: now_ms(),
    }
}

fn build_payload(
    sk: &SigningKey,
    diag: &NodeDiagnosticsResponse,
    hosting: Option<&NeighborHostingInfo>,
    label: Option<&str>,
    probe_cache: &ProbeCache,
) -> EntryPayload {
    let neighbors = diag
        .connected_peers_detailed
        .iter()
        .map(|p| NeighborInfo {
            address: p.address.clone(),
            // NodeDiagnostics doesn't expose per-peer ring location yet —
            // when it does, plug it in here. Until then `None` is honest.
            location: None,
            // Same — gateway-flag isn't exposed per peer. Default false.
            is_gateway: false,
        })
        .collect();
    let own_location = diag
        .node_info
        .as_ref()
        .and_then(|n| n.location.as_ref())
        .and_then(|s| s.parse::<f64>().ok());
    let external_address = diag
        .node_info
        .as_ref()
        .and_then(|n| n.listening_address.clone())
        .unwrap_or_default();

    // Build the `contracts` list as a deduplicated union of three
    // sources, in order of confidence:
    //   1. `contract_states.keys()` — every contract whose state this
    //      node currently caches. The most authoritative "I host this"
    //      signal in the API.
    //   2. `NeighborHostingInfo.my_hosted` — same intent, sometimes
    //      provides keys missing from `contract_states` (different
    //      caching path internally).
    //   3. `subscriptions` — client-WS-level subscriptions (webapps
    //      attached over WS); useful as a last-resort enumeration.
    // Subscribers display whatever shows up here — no `bincode`
    // breaking change required.
    // `(instance_id, Option<code_hash_base58>)`. Code hash is the
    // grouping key subscribers use to recognise "same app, different
    // instance" — see [`shared::contract::encode_contract_entry`] for
    // the wire suffix. Only `contract_states` carries `ContractKey`
    // (which has both fields); the other two sources are instance-only,
    // so their code-hash slot is `None` (subscribers fall back to the
    // existing title-based grouping for those).
    let mut entries: Vec<(String, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for k in diag.contract_states.keys() {
        let inst = k.encoded_contract_id();
        if seen.insert(inst.clone()) {
            entries.push((inst, Some(k.encoded_code_hash())));
        }
    }
    if let Some(h) = hosting {
        for entry in &h.my_hosted {
            if seen.insert(entry.contract_key.clone()) {
                entries.push((entry.contract_key.clone(), None));
            }
        }
    }
    for s in &diag.subscriptions {
        let key = s.contract_key.to_string();
        if seen.insert(key.clone()) {
            entries.push((key, None));
        }
    }

    // Decorate each key with the daemon's local-HTTP probe verdict
    // (`is_webapp`, `<title>`) so subscribers can render the friendly
    // name and the "✓ web" badge without doing any HTTP themselves.
    // Keys we haven't probed yet (or that timed out transiently) ship
    // bare — the dashboard treats `None` as "unknown".
    let contracts: Vec<String> = entries
        .into_iter()
        .map(|(k, code_hash)| {
            let r = probe_cache.get(&k).unwrap_or_default();
            encode_contract_entry(&k, r.is_webapp, r.title.as_deref(), code_hash.as_deref())
        })
        .collect();

    EntryPayload {
        public_key: sk.verifying_key().to_bytes(),
        external_address,
        own_location,
        version: label.map(|s| s.to_string()),
        neighbors,
        contracts,
        timestamp_ms: now_ms(),
    }
}

/// What `publish_one_cycle` reports back on a successful publish so the
/// caller can update the health snapshot without re-reading state.
#[derive(Debug, Clone, Copy, Default)]
struct PublishOutcome {
    peer_count: usize,
    contract_count: usize,
}

#[allow(clippy::too_many_arguments)]
async fn publish_one_cycle(
    api: &mut WebApi,
    sk: &SigningKey,
    instance: ContractInstanceId,
    code_hash: CodeHash,
    label: Option<&str>,
    fallback_neighbors: &[NeighborInfo],
    diagnostics_supported: &mut bool,
    http_host: &str,
    http_port: u16,
    probe_cache: &mut ProbeCache,
) -> Result<PublishOutcome> {
    let payload = if *diagnostics_supported {
        api.send(ClientRequest::NodeQueries(NodeQuery::NodeDiagnostics {
            config: NodeDiagnosticsConfig::full(),
        }))
        .await
        .map_err(|e| anyhow!("send NodeDiagnostics: {e:?}"))?;
        // Issue the hosting query immediately; both responses come
        // back asynchronously and we collect whichever lands.
        let _ = api
            .send(ClientRequest::NodeQueries(NodeQuery::NeighborHostingInfo))
            .await;

        // Drain responses until both queries answer, or 10 s pass —
        // whichever first. Without the timeout, a node that silently
        // ignores `NeighborHostingInfo` (older builds; non-stable API)
        // would block the whole publish loop forever waiting for the
        // second response. We publish with whatever we got.
        let mut diag: Option<NodeDiagnosticsResponse> = None;
        let mut hosting: Option<NeighborHostingInfo> = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                debug!(
                    diag = diag.is_some(),
                    hosting = hosting.is_some(),
                    "query collection deadline reached"
                );
                break;
            }
            match tokio::time::timeout(remaining, api.recv()).await {
                Ok(Ok(HostResponse::QueryResponse(QueryResponse::NodeDiagnostics(d)))) => {
                    diag = Some(d);
                }
                Ok(Ok(HostResponse::QueryResponse(QueryResponse::NeighborHosting(h)))) => {
                    hosting = Some(h);
                }
                Ok(Ok(_)) => {
                    debug!("ignoring non-query response while waiting");
                }
                Ok(Err(e)) => {
                    let s = format!("{e:?}");
                    if s.contains("not supported") {
                        warn!(
                            "node rejected NodeQueries (`not supported`); falling back to skeleton publishing for the rest of this run"
                        );
                        *diagnostics_supported = false;
                    } else {
                        return Err(anyhow!("recv: {s}"));
                    }
                    break;
                }
                Err(_) => {
                    debug!(
                        diag = diag.is_some(),
                        hosting = hosting.is_some(),
                        "recv timed out"
                    );
                    break;
                }
            }
            if diag.is_some() && hosting.is_some() {
                break;
            }
        }
        match diag {
            Some(d) => {
                debug!(
                    peers = d.connected_peers_detailed.len(),
                    contracts = d.contract_states.len(),
                    hosted_via_hosting = hosting.as_ref().map(|h| h.my_hosted.len()).unwrap_or(0),
                    location = ?d.node_info.as_ref().and_then(|n| n.location.clone()),
                    "got node diagnostics"
                );
                // Probe any contract keys we haven't classified yet so the
                // payload can ship friendly names and the "✓ web" badge.
                let mut all_keys: Vec<String> =
                    d.contract_states.keys().map(|k| k.to_string()).collect();
                if let Some(h) = hosting.as_ref() {
                    for entry in &h.my_hosted {
                        all_keys.push(entry.contract_key.clone());
                    }
                }
                for s in &d.subscriptions {
                    all_keys.push(s.contract_key.to_string());
                }
                probe::refresh_cache(http_host, http_port, &all_keys, probe_cache).await;
                build_payload(sk, &d, hosting.as_ref(), label, probe_cache)
            }
            None => build_skeleton_payload(sk, fallback_neighbors, label),
        }
    } else {
        build_skeleton_payload(sk, fallback_neighbors, label)
    };
    let payload_bytes = bincode::serialize(&payload).context("serialize payload")?;
    let sig = sk.sign(&payload_bytes);
    let signed = SignedEntry {
        payload: payload_bytes,
        signature: sig.to_bytes(),
    };
    let delta = ContractDelta {
        entries: vec![signed],
    };
    let delta_bytes = bincode::serialize(&delta).context("serialize delta")?;

    let key = ContractKey::from_id_and_code(instance, code_hash);
    api.send(ClientRequest::ContractOp(ContractRequest::Update {
        key,
        data: UpdateData::Delta(StateDelta::from(delta_bytes)),
    }))
    .await
    .map_err(|e| anyhow!("send Update: {e:?}"))?;

    info!(
        peers = payload.neighbors.len(),
        contracts = payload.contracts.len(),
        webapps = probe_cache.webapp_count(),
        probed = probe_cache.len(),
        "published topology entry"
    );
    Ok(PublishOutcome {
        peer_count: payload.neighbors.len(),
        contract_count: payload.contracts.len(),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let key_path = cli.key_file.clone().unwrap_or_else(default_key_path);
    let sk = load_or_create_key(&key_path)?;
    info!(
        pubkey = %hex::encode(sk.verifying_key().to_bytes()),
        key_path = %key_path.display(),
        "loaded publisher identity"
    );

    let instance =
        ContractInstanceId::from_str(cli.instance_id.trim()).context("parse instance_id")?;
    let code_bytes = bs58::decode(cli.code_hash.trim())
        .into_vec()
        .context("decode code_hash base58")?;
    let code_hash = CodeHash::try_from(code_bytes.as_slice())
        .map_err(|e| anyhow!("code_hash length: {e}"))?;

    let url = ws_url(&cli.node_ws_url)?;
    // Same host:port serves both the WS endpoint and the HTTP API the
    // probe hits — derive once so we don't ask the user for two URLs.
    let http_host = url.host_str().unwrap_or("127.0.0.1").to_string();
    let http_port = url.port().unwrap_or(7509);
    info!(%url, http_host = %http_host, http_port, "connecting to local freenet node");

    let fallback_neighbors: Vec<NeighborInfo> =
        cli.neighbors.iter().map(|s| parse_neighbor_arg(s)).collect();
    // State that should survive WS reconnects:
    //   - `probe_cache`: rebuilding it requires re-probing hundreds of
    //     contracts; keeping it across reconnects avoids a thundering
    //     herd on the local HTTP server every time the WS blips.
    //   - `diagnostics_supported`: a node that rejected NodeQueries
    //     during the previous session is still going to reject them
    //     after a reconnect — same WS shape, same gating.
    let mut probe_cache: ProbeCache = ProbeCache::new();
    let mut diagnostics_supported = true;

    // `watch` channel: publish loop is the single writer, every health
    // server connection is a reader. last-write-wins matches the
    // semantic we want (always serve the freshest snapshot).
    let (health_tx, health_rx) = watch::channel(HealthSnapshot::default());
    if cli.metrics_port > 0 {
        let port = cli.metrics_port;
        let rx = health_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = health::run_health_server(port, rx).await {
                warn!(error = %e, "health server exited; /healthz will not be available");
            }
        });
        info!(port = cli.metrics_port, "health endpoint /healthz active");
    }

    // Reconnect with exponential backoff. The outer freenet node could
    // be restarting, the WS could drop after a network blip, etc. —
    // without this loop the systemd unit would respawn and lose the
    // probe cache on every blip. Backoff is bounded so a sustained
    // outage doesn't burn CPU or spam the journal.
    let backoff_min = Duration::from_secs(1);
    let backoff_max = Duration::from_secs(60);
    let mut backoff = backoff_min;
    loop {
        match run_session(
            &url,
            &sk,
            instance,
            code_hash,
            cli.display_name.as_deref(),
            &fallback_neighbors,
            &mut diagnostics_supported,
            &http_host,
            http_port,
            &mut probe_cache,
            cli.interval_secs.max(5),
            &health_tx,
        )
        .await
        {
            Ok(()) => {
                // Sessions don't return Ok normally — the publish loop
                // is infinite. If we got here, treat as a clean exit
                // and try again with min-backoff.
                backoff = backoff_min;
            }
            Err(e) => {
                warn!(error = %e, backoff_secs = backoff.as_secs(), "session ended; reconnecting");
            }
        }
        // Reflect "session is down" in the health snapshot so a
        // scraper notices the gap immediately rather than after the
        // next successful publish.
        let mut snap = health_tx.borrow().clone();
        snap.session_alive = false;
        let _ = health_tx.send(snap);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(backoff_max);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    url: &Url,
    sk: &SigningKey,
    instance: ContractInstanceId,
    code_hash: CodeHash,
    label: Option<&str>,
    fallback_neighbors: &[NeighborInfo],
    diagnostics_supported: &mut bool,
    http_host: &str,
    http_port: u16,
    probe_cache: &mut ProbeCache,
    interval_secs: u64,
    health_tx: &watch::Sender<HealthSnapshot>,
) -> Result<()> {
    let (stream, _resp) = connect_async(url.as_str()).await.context("ws connect")?;
    let mut api = WebApi::start(stream);

    // Subscribe so the daemon also sees what other publishers ship —
    // could be useful for diagnostics / logging, and forces the local
    // node to keep the contract hot.
    api.send(ClientRequest::ContractOp(ContractRequest::Subscribe {
        key: instance,
        summary: None,
    }))
    .await
    .map_err(|e| anyhow!("send Subscribe: {e:?}"))?;
    info!("subscribed to topology contract");

    // Tell systemd we're done starting — without this `READY=1` the
    // unit (`Type=notify`) stays in "starting" state and watchdog
    // stays disarmed. Idempotent on reconnects: harmless to call
    // again, systemd just sees a no-op.
    health::notify_ready();

    // Mark session as live in the health snapshot the moment we
    // subscribe — a scraper polling /healthz right after a reconnect
    // shouldn't see `session_alive: false` until we drop again.
    {
        let mut snap = health_tx.borrow().clone();
        snap.session_alive = true;
        let _ = health_tx.send(snap);
    }

    let mut tick = interval(Duration::from_secs(interval_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tick.tick().await;
        match publish_one_cycle(
            &mut api,
            sk,
            instance,
            code_hash,
            label,
            fallback_neighbors,
            diagnostics_supported,
            http_host,
            http_port,
            probe_cache,
        )
        .await
        {
            Ok(outcome) => {
                // Refresh the snapshot served at /healthz and ping the
                // systemd watchdog (if `WatchdogSec=` is configured)
                // so the unit isn't killed for inactivity.
                let snap = HealthSnapshot {
                    last_publish_unix: HealthSnapshot::now_secs(),
                    last_publish_secs_ago: 0,
                    session_alive: true,
                    last_peer_count: outcome.peer_count,
                    last_contract_count: outcome.contract_count,
                    last_webapp_count: probe_cache.webapp_count(),
                    probed_total: probe_cache.len(),
                };
                let _ = health_tx.send(snap);
                health::ping_watchdog();
            }
            // A `send Update`/`send NodeQueries` failure almost always
            // means the WS is dead — bubble up to trigger reconnect.
            // The matchable text comes from the `anyhow!` strings in
            // `publish_one_cycle`.
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("send Update")
                    || msg.contains("send NodeDiagnostics")
                    || msg.contains("send Subscribe")
                    || msg.contains("recv:")
                {
                    return Err(e.context("WS session error; reconnecting"));
                }
                warn!(error = %e, "publish cycle failed; will retry next tick");
            }
        }
        // Drain any UpdateNotifications that arrived during the cycle
        // so the channel doesn't back up. Non-blocking peek-style: try
        // a few cheap recvs with a short timeout.
        for _ in 0..16 {
            match tokio::time::timeout(Duration::from_millis(10), api.recv()).await {
                Ok(Ok(resp)) => match resp {
                    HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                        ..
                    }) => debug!("update notification received"),
                    HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                        ..
                    }) => debug!("subscribe ack"),
                    _ => debug!(?resp, "other response"),
                },
                Ok(Err(e)) => {
                    // Hard recv error → WS is dead, return so the outer
                    // loop can reconnect.
                    return Err(anyhow!("recv error during drain: {e:?}"));
                }
                Err(_) => break, // timeout — channel idle
            }
        }
    }
}
