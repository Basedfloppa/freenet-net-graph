//! User-controllable runtime settings — persisted to `localStorage` so each
//! visitor of the dashboard can keep their own configuration.
//!
//! Why localStorage and not `?query=…` URL params: when this dashboard is
//! eventually packaged as a Freenet webapp contract, every visitor will
//! fetch the same WASM bundle from their local node. A user-specific
//! layer is needed for "which gateways do I want to scrape, how aggressive
//! is the layout, etc.". `localStorage` is the obvious origin-scoped slot
//! for that.
//!
//! Serialisation: a single JSON blob under one key (`STORAGE_KEY`). Easy
//! to inspect from devtools; `#[serde(default)]` on individual fields lets
//! us add new fields later without invalidating older saved settings.

use serde::{Deserialize, Serialize};
use shared::KnownNode;

const STORAGE_KEY: &str = "freenet-net-graph:settings:v1";

/// Single source of truth for user settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Statically-known nodes (typically public default gateways) injected
    /// into the graph regardless of who actually publishes about them.
    /// Defaults to the operator's anchor gateways (baka, orange) so a
    /// freshly-loaded dashboard always has *something* to publish into
    /// `EntryPayload.neighbors` — the sandboxed webapp can't probe the
    /// local node's real peer list (`fetch /` is CORS-blocked, server
    /// rejects `NodeQueries` from web apps), so without these defaults
    /// every publish would carry an empty neighbours array.
    #[serde(default = "default_known_nodes")]
    pub known_nodes: Vec<KnownNode>,
    /// Sidebar width in CSS pixels. Drag-resizer writes here.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: i32,
    /// Whether the search list shows nodes or contracts on first paint.
    #[serde(default)]
    pub filter_mode: PersistedFilter,
    /// Force-directed layout tuning. The graph component reads these
    /// every tick rather than using the previous `const` constants.
    #[serde(default)]
    pub layout: LayoutSettings,
    /// Optional subscription to a Freenet topology contract. When enabled,
    /// the frontend connects directly to the local node's WebSocket API
    /// and folds the per-node neighbour-list entries it receives into the
    /// same graph as locally-scraped data.
    #[serde(default)]
    pub contract: ContractSettings,
}

/// User-tunable knobs for the topology-contract subscription. Lives in the
/// same `localStorage` blob as the rest of `Settings` so a returning user
/// keeps their connection configuration without touching the backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractSettings {
    /// Master toggle. When `false`, the WebSocket is closed and no contract
    /// data merges into the graph.
    pub enabled: bool,
    /// WebSocket URL of the local freenet node's client API. The path
    /// `/v1/contract/command?encodingProtocol=native` is appended by
    /// `contract_client` automatically.
    /// Default: `ws://localhost:7509`.
    pub node_ws_url: String,
    /// Base58-encoded `ContractInstanceId` of the deployed topology
    /// contract. Empty until the operator deploys it and pastes the value.
    pub instance_id: String,
}

/// Default `node_ws_url` derived from the current page origin so the
/// dashboard talks to whichever node served it. When loaded as a webapp
/// contract on `http://<host>:7509/v1/contract/web/.../`, the iframe's
/// `window.location.origin` is `http://<host>:7509` and the WS endpoint
/// lives at the same host. Falls back to `ws://localhost:7509` outside a
/// browser (build-time, tests).
fn default_node_ws_url() -> String {
    let Some(window) = web_sys::window() else {
        return "ws://localhost:7509".to_string();
    };
    let origin = match window.location().origin() {
        Ok(o) if !o.is_empty() && o != "null" => o,
        _ => return "ws://localhost:7509".to_string(),
    };
    if let Some(rest) = origin.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = origin.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        "ws://localhost:7509".to_string()
    }
}

/// Pre-filled topology contract published by the operator. New
/// dashboards subscribe to this contract by default so a freshly-loaded
/// webapp "just works" without the user having to paste hashes from
/// external instructions. If you redeploy the contract against new
/// code or new initial state, update this constant.
const DEFAULT_TOPOLOGY_INSTANCE_ID: &str = "BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE";

impl Default for ContractSettings {
    fn default() -> Self {
        // `enabled` defaults to `true` so a freshly loaded webapp
        // immediately joins the network. The sandbox iframe loses
        // `localStorage` on hard reload (opaque "null" origin →
        // ephemeral storage), so users would otherwise have to re-tick
        // every time.
        Self {
            enabled: true,
            node_ws_url: default_node_ws_url(),
            instance_id: DEFAULT_TOPOLOGY_INSTANCE_ID.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedFilter {
    Nodes,
    Contracts,
}

impl Default for PersistedFilter {
    fn default() -> Self {
        PersistedFilter::Nodes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LayoutSettings {
    /// Inverse-square repulsion coefficient between every pair of nodes.
    pub k_repel: f64,
    /// Edge spring stiffness.
    pub k_edge: f64,
    /// Natural resting length of every edge, in pixels.
    pub edge_rest_length: f64,
    /// Linear pull toward the canvas centre.
    pub k_gravity: f64,
    /// Velocity damping per tick (0..1; closer to 1 = more inertia).
    pub damping: f64,
    /// Animation tick interval in ms. 33 → ~30 FPS.
    #[serde(default = "default_tick_ms")]
    pub tick_ms: u32,
    /// Velocity cap so a runaway node can't fly off-screen in one frame.
    #[serde(default = "default_max_speed")]
    pub max_speed: f64,
    /// Floor on inter-node distance for the repulsion calculation.
    /// Prevents `K_REPEL / d²` from blowing up when two nodes overlap.
    #[serde(default = "default_repel_min_dist")]
    pub repel_min_dist: f64,
    /// Soft viewport clamp radius from canvas centre. Beyond this radius
    /// gravity scales up quadratically so the graph snaps back instead
    /// of escaping the canvas.
    #[serde(default = "default_soft_clamp_radius")]
    pub soft_clamp_radius: f64,
}

fn default_tick_ms() -> u32 { 33 }
fn default_max_speed() -> f64 { 22.0 }
fn default_repel_min_dist() -> f64 { 14.0 }
fn default_soft_clamp_radius() -> f64 { 480.0 }

impl Default for LayoutSettings {
    fn default() -> Self {
        // Same values that lived as `const` in `graph.rs` before settings
        // existed. Kept here in one place so the defaults stay tunable.
        Self {
            k_repel: 1600.0,
            k_edge: 0.010,
            edge_rest_length: 150.0,
            k_gravity: 0.0035,
            damping: 0.85,
            tick_ms: default_tick_ms(),
            max_speed: default_max_speed(),
            repel_min_dist: default_repel_min_dist(),
            soft_clamp_radius: default_soft_clamp_radius(),
        }
    }
}

fn default_sidebar_width() -> i32 {
    320
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            known_nodes: default_known_nodes(),
            sidebar_width: default_sidebar_width(),
            filter_mode: PersistedFilter::default(),
            layout: LayoutSettings::default(),
            contract: ContractSettings::default(),
        }
    }
}

/// Anchor gateways shipped with the dashboard. The publishing node
/// includes them in `EntryPayload.neighbors` so subscribers see at
/// least these edges when no other publishers are on the contract
/// yet, and they appear in the graph regardless of subscription state.
///
/// Sandbox iframe origin is "null", so the dashboard can't enumerate
/// its local node's actual peers (CORS blocks `fetch '/'`,
/// `freenet-core` blocks webapp `NodeQueries`). These endpoints are
/// known operator-run gateways — peers any freenet node may dial
/// over UDP/31337 — so publishing them gives a useful starting
/// topology subscribers across the network can render.
///
/// `nova` / `vega` come from freenet-core itself (network defaults
/// referenced in `scripts/check-endpoints.sh`); `baka` / `orange` are
/// this operator's own gateways. Edit the list per deployment.
fn default_known_nodes() -> Vec<KnownNode> {
    vec![
        KnownNode {
            label: "nova".to_string(),
            address: "5.9.111.215:31337".to_string(),
            location: None,
            is_gateway: true,
            source: "default".to_string(),
        },
        KnownNode {
            label: "vega".to_string(),
            address: "100.27.151.80:31337".to_string(),
            location: None,
            is_gateway: true,
            source: "default".to_string(),
        },
        KnownNode {
            label: "baka".to_string(),
            address: "78.27.236.159:31337".to_string(),
            location: None,
            is_gateway: true,
            source: "default".to_string(),
        },
        KnownNode {
            label: "orange".to_string(),
            address: "145.249.246.115:31337".to_string(),
            location: None,
            is_gateway: true,
            source: "default".to_string(),
        },
    ]
}

impl Settings {
    /// Sanitise after deserialisation so a hand-edited `localStorage`
    /// entry can't crash the layout (NaN damping, zero poll interval, etc).
    pub fn normalize(mut self) -> Self {
        self.sidebar_width = self.sidebar_width.clamp(220, 800);
        self.layout.k_repel = self.layout.k_repel.clamp(50.0, 8000.0);
        self.layout.k_edge = self.layout.k_edge.clamp(0.0, 0.1);
        self.layout.edge_rest_length = self.layout.edge_rest_length.clamp(20.0, 400.0);
        self.layout.k_gravity = self.layout.k_gravity.clamp(0.0, 0.1);
        self.layout.damping = self.layout.damping.clamp(0.5, 0.99);
        self.layout.tick_ms = self.layout.tick_ms.clamp(16, 200);
        self.layout.max_speed = self.layout.max_speed.clamp(2.0, 80.0);
        self.layout.repel_min_dist = self.layout.repel_min_dist.clamp(2.0, 60.0);
        self.layout.soft_clamp_radius = self.layout.soft_clamp_radius.clamp(200.0, 600.0);
        self
    }
}

/// Try to load settings from `localStorage`, falling back to URL fragment.
/// Returns `None` when nothing is stored anywhere parseable.
///
/// URL fragment persistence note: a sandboxed iframe runs at an opaque
/// "null" origin, and Chrome treats `localStorage` for opaque origins
/// as ephemeral — it gets wiped on reload. The URL fragment (`#…`)
/// survives reloads, so we mirror cross-reload-relevant settings there
/// (currently `known_nodes` + `sidebar_width`).
pub fn load_from_storage() -> Option<Settings> {
    let storage = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    if let Some(storage) = storage.as_ref() {
        if let Ok(Some(raw)) = storage.get_item(STORAGE_KEY) {
            if let Ok(parsed) = serde_json::from_str::<Settings>(&raw) {
                return Some(parsed.normalize().with_persistent_from_fragment());
            }
        }
    }
    Some(Settings::default().with_persistent_from_fragment())
}

/// Persist settings to `localStorage` AND mirror cross-reload-relevant
/// fields to the URL fragment so a sandbox-iframe hard reload doesn't
/// reset them. Currently mirrored: `known_nodes` + `sidebar_width`.
pub fn save_to_storage(s: &Settings) {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Ok(Some(storage)) = window.local_storage() {
        match serde_json::to_string(s) {
            Ok(json) => {
                if let Err(e) = storage.set_item(STORAGE_KEY, &json) {
                    log::warn!("settings save failed: {e:?}");
                }
            }
            Err(e) => log::warn!("settings serialise failed: {e}"),
        }
    }
    write_persistent_to_fragment(&s.known_nodes, s.sidebar_width);
}

/// Read fields previously written by `write_persistent_to_fragment`.
fn read_fragment() -> Fragment {
    let mut out = Fragment::default();
    let Some(window) = web_sys::window() else {
        return out;
    };
    let Ok(hash) = window.location().hash() else {
        return out;
    };
    let trimmed = hash.trim_start_matches('#');
    for part in trimmed.split('&') {
        if let Some(rest) = part.strip_prefix("nodes=") {
            // base64-url-encoded JSON array of [label, address, loc, gw].
            // Tuple form is shorter than full struct names — keeps the
            // fragment under the 8 KiB shell cap even with many entries.
            let Some(decoded) = url_b64_decode(rest) else { continue };
            let Ok(s) = std::str::from_utf8(&decoded) else { continue };
            let Ok(parsed) = serde_json::from_str::<Vec<NodeTuple>>(s) else { continue };
            out.nodes = Some(
                parsed
                    .into_iter()
                    .map(|t| KnownNode {
                        label: t.0,
                        address: t.1,
                        location: t.2,
                        is_gateway: t.3,
                        source: "fragment".to_string(),
                    })
                    .collect(),
            );
        } else if let Some(rest) = part.strip_prefix("sb=") {
            if let Ok(v) = rest.parse::<i32>() {
                out.sidebar_width = Some(v);
            }
        }
    }
    out
}

#[derive(Default)]
struct Fragment {
    nodes: Option<Vec<KnownNode>>,
    sidebar_width: Option<i32>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct NodeTuple(String, String, Option<f64>, bool);

fn url_b64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn url_b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .ok()
}

/// Mirror the known-nodes list and sidebar width into the URL
/// fragment.
///
/// We're inside the sandboxed iframe whose location is
/// `…?__sandbox=1`. Updating *iframe* `location.hash` would survive
/// only this iframe's lifetime, not a hard reload of the outer page —
/// because on reload the user's address-bar URL is what the browser
/// re-opens, and that's the outer shell's URL. To get the data onto
/// the outer URL we use the freenet shell's `__freenet_shell__:
/// type:'hash'` postMessage protocol (`path_handlers.rs:672` in
/// freenet-core): the outer shell receives the hash from any iframe
/// and applies it to its own URL via `history.replaceState`. On
/// reload, the outer shell forwards the hash back to the freshly-
/// loaded iframe (line 951 `forwardHash`), iframe shim writes it
/// onto our `location.hash`, and `read_fragment` picks it up.
fn write_persistent_to_fragment(nodes: &[KnownNode], sidebar_width: i32) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let mut parts: Vec<String> = Vec::new();
    let tuples: Vec<NodeTuple> = nodes
        .iter()
        .map(|n| {
            NodeTuple(
                n.label.clone(),
                n.address.clone(),
                n.location,
                n.is_gateway,
            )
        })
        .collect();
    if !tuples.is_empty() {
        if let Ok(json) = serde_json::to_vec(&tuples) {
            // 8 KiB cap on `__freenet_shell__: type:'hash'` (see
            // path_handlers.rs:677 `slice(0, 8192)`). Drop the nodes
            // payload if encoding would exceed it; seed alone is
            // critical, the nodes list is nice-to-have.
            let encoded = url_b64_encode(&json);
            if encoded.len() < 7000 {
                parts.push(format!("nodes={}", encoded));
            }
        }
    }
    // Only mirror sidebar width when it differs from the default —
    // skip the fragment param when the user hasn't customised it so
    // the address bar stays clean for first-time visitors.
    if sidebar_width != default_sidebar_width() {
        parts.push(format!("sb={sidebar_width}"));
    }
    let hash_value = if parts.is_empty() {
        String::new()
    } else {
        format!("#{}", parts.join("&"))
    };

    let parent = match window.parent() {
        Ok(Some(p)) if !p.is_undefined() => p,
        _ => return,
    };
    if web_sys::js_sys::Object::is(&parent, &window) {
        // Top-level page (no outer shell wrapper). Just write our own
        // hash directly — best we can do.
        if let Ok(history) = window.history() {
            let _ = history.replace_state_with_url(
                &wasm_bindgen::JsValue::NULL,
                "",
                Some(&hash_value),
            );
        }
        return;
    }
    let payload = web_sys::js_sys::Object::new();
    let _ = web_sys::js_sys::Reflect::set(
        &payload,
        &"__freenet_shell__".into(),
        &wasm_bindgen::JsValue::TRUE,
    );
    let _ = web_sys::js_sys::Reflect::set(&payload, &"type".into(), &"hash".into());
    let _ = web_sys::js_sys::Reflect::set(&payload, &"hash".into(), &hash_value.into());
    let _ = parent.post_message(&payload, "*");
}

impl Settings {
    /// Hydrate `known_nodes` and `sidebar_width` from the URL fragment.
    /// Called both when localStorage parses cleanly and when we fall
    /// back to defaults — sandbox `localStorage` is wiped on reload,
    /// so the fragment is what makes a fresh load look "remembered".
    fn with_persistent_from_fragment(mut self) -> Self {
        let frag = read_fragment();
        // Fragment-restored nodes win over hardcoded defaults: if the
        // user's last session had a custom list, that list comes back.
        // The fragment is only set after `save_to_storage`, which only
        // happens after the user's first interaction — so on a truly
        // first-time load the fragment is empty and we keep the
        // defaults (baka, orange).
        if let Some(nodes) = frag.nodes {
            self.known_nodes = nodes;
        }
        // Same rationale: on a sandbox-iframe hard reload localStorage
        // is wiped, so without the fragment the user's resized sidebar
        // would snap back to the default. Clamp to the same range as
        // `normalize` so a hand-edited fragment can't break the layout.
        if let Some(w) = frag.sidebar_width {
            self.sidebar_width = w.clamp(220, 800);
        }
        self
    }
}

/// Erase the stored blob entirely. Used by the "reset to defaults" button
/// in the settings drawer.
pub fn clear_storage() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(STORAGE_KEY);
        }
    }
}
