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

mod probe;
use probe::ProbeCache;

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

    /// Optional friendly label included in `EntryPayload.version` so
    /// subscribers can tell publishers apart at a glance. The contract
    /// still keys on the Ed25519 pubkey; this is purely cosmetic.
    #[arg(long)]
    label: Option<String>,

    /// Fallback `neighbors` entries (`label,host:port` or just
    /// `host:port`) to publish when `NodeDiagnostics` is unavailable
    /// (e.g. running against a freenet local-mode node). Repeatable.
    /// Network-mode nodes ignore this — they get the real peer list.
    #[arg(long = "neighbor", value_name = "LABEL,HOST:PORT")]
    neighbors: Vec<String>,
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
    let mut keys: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for k in diag.contract_states.keys() {
        let s = k.to_string();
        if seen.insert(s.clone()) {
            keys.push(s);
        }
    }
    if let Some(h) = hosting {
        for entry in &h.my_hosted {
            if seen.insert(entry.contract_key.clone()) {
                keys.push(entry.contract_key.clone());
            }
        }
    }
    for s in &diag.subscriptions {
        let key = s.contract_key.to_string();
        if seen.insert(key.clone()) {
            keys.push(key);
        }
    }

    // Decorate each key with the daemon's local-HTTP probe verdict
    // (`is_webapp`, `<title>`) so subscribers can render the friendly
    // name and the "✓ web" badge without doing any HTTP themselves.
    // Keys we haven't probed yet (or that timed out transiently) ship
    // bare — the dashboard treats `None` as "unknown".
    let contracts: Vec<String> = keys
        .into_iter()
        .map(|k| {
            let r = probe_cache.get(&k).cloned().unwrap_or_default();
            encode_contract_entry(&k, r.is_webapp, r.title.as_deref())
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
) -> Result<()> {
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

    let webapp_count = probe_cache
        .values()
        .filter(|r| r.is_webapp == Some(true))
        .count();
    info!(
        peers = payload.neighbors.len(),
        contracts = payload.contracts.len(),
        webapps = webapp_count,
        probed = probe_cache.len(),
        "published topology entry"
    );
    Ok(())
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

    let mut tick = interval(Duration::from_secs(cli.interval_secs.max(5)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let fallback_neighbors: Vec<NeighborInfo> =
        cli.neighbors.iter().map(|s| parse_neighbor_arg(s)).collect();
    let mut diagnostics_supported = true;
    // Cache probe results across cycles so we hit the local HTTP API
    // only for newly-seen contracts. Lifetime = process lifetime.
    let mut probe_cache: ProbeCache = ProbeCache::new();

    loop {
        tick.tick().await;
        match publish_one_cycle(
            &mut api,
            &sk,
            instance,
            code_hash,
            cli.label.as_deref(),
            &fallback_neighbors,
            &mut diagnostics_supported,
            &http_host,
            http_port,
            &mut probe_cache,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => warn!(error = %e, "publish cycle failed; will retry next tick"),
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
                    warn!(error = %e, "recv error during drain");
                    break;
                }
                Err(_) => break, // timeout — channel idle
            }
        }
    }
}
