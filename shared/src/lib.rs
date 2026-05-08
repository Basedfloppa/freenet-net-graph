//! Wire types shared between the Yew frontend and the topology-contract
//! WASM.
//!
//! Two distinct namespaces live here:
//!
//! - The aggregated graph view (`Topology`, `GatewayView`, `PeerView`,
//!   `KnownNode`, `ContractView`, `ContractMeta`) — what the frontend
//!   builds from contract subscription data and renders in the UI. Lives
//!   here (rather than directly in the frontend) so it can be reused if
//!   anyone ever writes another consumer of the same format.
//! - The contract namespace ([`contract`]) — `EntryPayload` /
//!   `SignedEntry` / `ContractState`, plus sign/verify helpers. Used by
//!   the in-browser publisher worker (to sign) and the contract WASM
//!   (to verify and merge).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod contract;

/// Aggregated topology view, built from verified entries received over the
/// topology-contract subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    pub gateways: Vec<GatewayView>,
    /// Statically known network nodes — typically the public default
    /// gateways from `freenet.org/keys/gateways.toml`. They are NOT scraped
    /// (their HTTP dashboards are usually firewalled in production), but we
    /// place them on the graph so the visualisation includes them even
    /// before any operator-controlled gateway reports them as a peer.
    /// Frontend dedupes by `address` against scraped peers.
    #[serde(default)]
    pub known_nodes: Vec<KnownNode>,
    /// Per-contract enrichment populated by the backend's lazy probe of
    /// `/v1/contract/web/{key}/`. Keyed by full base58 contract key. Absent
    /// keys are "not yet probed". `#[serde(default)]` so older backend
    /// versions remain wire-compatible.
    #[serde(default)]
    pub contract_meta: BTreeMap<String, ContractMeta>,
    pub fetched_at: u64,
}

/// Webapp-related metadata the backend resolves for a contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractMeta {
    /// True if `/v1/contract/web/{key}/` returned 200 on at least one
    /// scraped gateway — i.e. the contract ships a webapp bundle.
    pub has_web_interface: bool,
    /// `<title>` extracted from the iframe content (the
    /// `?__sandbox=1` route). `None` when the contract isn't a webapp,
    /// the response wasn't HTML, or the title was empty.
    pub title: Option<String>,
    /// Wall-clock seconds since epoch when this entry was filled. Lets
    /// the backend expire stale entries (e.g. retry failed probes).
    pub probed_at: u64,
}

/// A statically-injected node — a known public gateway whose dashboard the
/// aggregator can't scrape but whose existence on the network we want to
/// visualise unconditionally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownNode {
    /// Friendly name (e.g. "nova", "vega").
    pub label: String,
    /// `host:port` matching the format the gateway dashboard prints for peers.
    pub address: String,
    /// Ring location, if known statically. Usually `None` — when a scraped
    /// gateway happens to be connected to this node, the frontend will pick
    /// up the real location from the scraped peer entry.
    pub location: Option<f64>,
    pub is_gateway: bool,
    /// Where this entry came from: "default" (built-in list) or "cli"
    /// (passed via --known-gateway). Used for sidebar grouping.
    pub source: String,
}

/// One gateway's view of itself and its directly connected peers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayView {
    pub label: String,
    pub url: String,
    pub status: FetchStatus,
    pub own_location: Option<f64>,
    pub external_address: Option<String>,
    pub version: Option<String>,
    pub peers: Vec<PeerView>,
    /// Contracts this gateway is subscribed to, as listed in the
    /// "Subscribed Contracts" panel of its HTML dashboard. May be missing
    /// on older freenet builds; serde defaults the field to empty.
    #[serde(default)]
    pub contracts: Vec<ContractView>,
    /// Wall-clock ms when the publisher entry behind this gateway view
    /// was signed — i.e. how fresh its peer/contract data is. The
    /// frontend uses this to fade gateways that haven't reposted in a
    /// while ("stale publisher" UX). Older state shapes that don't
    /// carry this field deserialise to `None` and render as fresh.
    #[serde(default)]
    pub last_seen_ms: Option<u64>,
}

/// One row from a gateway's "Subscribed Contracts" table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractView {
    /// Full base58-encoded contract key (the `title=` attribute on the
    /// dashboard row, NOT the truncated `<code>` body).
    pub key: String,
    /// Free-form "X ago" strings as the dashboard prints them; passed
    /// through unmodified.
    pub subscribed_ago: Option<String>,
    pub last_update_ago: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FetchStatus {
    Ok,
    ParseFailed { message: String },
    Unreachable { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerView {
    pub address: String,
    pub is_gateway: bool,
    pub location: Option<f64>,
    pub connected: Option<String>,
}
