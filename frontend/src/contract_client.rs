//! Browser-side subscriber for the Freenet topology contract.
//!
//! When `Settings.contract.enabled` is true, this module:
//!
//! 1. Opens a WebSocket to `{node_ws_url}/v1/contract/command?encodingProtocol=native`.
//! 2. Sends `ContractRequest::Subscribe { key, summary: None }` once the
//!    socket fires `onopen`.
//! 3. Decodes incoming `HostResponse::ContractResponse(UpdateNotification)`
//!    events, expects each to wrap one or more
//!    `shared::contract::SignedEntry` records, verifies the Ed25519
//!    signatures, and emits the decoded `EntryPayload`s to the App via a
//!    Yew `Callback`.
//!
//! The App treats every incoming entry as a peer-supplied "view" of one
//! node — its public key is the stable identity, its `external_address`,
//! `own_location`, and `neighbors` synthesise a `GatewayView` shape that
//! merges into the same aggregation pipeline as locally-scraped data.
//!
//! Why subscribe from the browser (not the backend)? The end-state of
//! this dashboard is a Freenet webapp contract: every visitor's browser
//! fetches the same WASM bundle from their *local* node, then uses *that
//! node*'s WS API. There is no shared backend in that future. Putting
//! the subscriber here keeps the backend optional.

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;

use crate::ws_shim::WsShim;
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractRequest, ContractResponse, Error as WebApiError,
    HostResponse,
};
use freenet_stdlib::prelude::{ContractInstanceId, UpdateData};
use shared::contract::{EntryPayload, SignedEntry};
use yew::Callback;

use crate::settings::ContractSettings;

/// Public-facing connection state. The settings drawer renders this as a
/// status pill so the user can tell whether their dashboard is live or
/// silently broken.
#[derive(Clone, Debug, PartialEq)]
pub enum ContractStatus {
    /// `Settings.contract.enabled = false`.
    Disabled,
    /// WS is opening; haven't received `onopen` yet.
    Connecting,
    /// `Subscribe` request sent; waiting for confirmation / first event.
    Subscribing,
    /// We've received at least one entry from the contract.
    Subscribed,
    /// Anything went wrong: bad key, WS closed, decode error, etc.
    /// Carries a short reason for the UI.
    Error(String),
}

/// One verified entry decoded from a `SignedEntry` that came over the
/// subscription. Emitted to the App via the `on_entry` callback.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteEntry {
    /// Hex-encoded Ed25519 public key — also the identity slot in the
    /// contract's `BTreeMap`. Stable across publishes from the same node.
    pub publisher_pubkey_hex: String,
    /// Decoded payload — same shape as what `topology-publisher` signs.
    pub payload: EntryPayload,
}

/// Owned handle to a live subscription. Drop it to close the WebSocket
/// (`WsShim::Drop` sends a `1000 Normal Closure` frame via
/// `WebSocket::close`).
pub struct ContractClient {
    /// Owns the `WsShim`; `Drop` closes the WebSocket when the client
    /// goes out of scope (the `Rc<RefCell<Option<…>>>` shape exists so
    /// the `onopen` callback inside `WsShim::start` can reach back in
    /// to send the initial Subscribe). Marked unused-but-load-bearing:
    /// dropping this field is the only way the subscription terminates.
    #[allow(dead_code)]
    api: Rc<RefCell<Option<WsShim>>>,
}

impl ContractClient {
    /// Open the WS, send the subscribe request, and wire up the
    /// `on_entry` / `on_status` callbacks. Returns `Err` synchronously
    /// only for *configuration* problems (bad URL, malformed instance id).
    /// The actual `WebSocket::new` failure surfaces via `on_status` once
    /// the browser tries to dial.
    pub fn start(
        cfg: &ContractSettings,
        on_entry: Callback<Vec<RemoteEntry>>,
        on_status: Callback<ContractStatus>,
    ) -> Result<Self, String> {
        let instance = ContractInstanceId::from_str(cfg.instance_id.trim())
            .map_err(|e| format!("bad instance_id: {e}"))?;

        let url = format!(
            "{}/v1/contract/command?encodingProtocol=native",
            cfg.node_ws_url.trim_end_matches('/')
        );

        on_status.emit(ContractStatus::Connecting);

        let socket = web_sys::WebSocket::new(&url)
            .map_err(|e| format!("WebSocket::new failed: {e:?}"))?;

        let api: Rc<RefCell<Option<WsShim>>> = Rc::new(RefCell::new(None));

        let result_handler = {
            let on_entry = on_entry.clone();
            let on_status = on_status.clone();
            move |res: Result<HostResponse, ClientError>| {
                handle_host_response(res, &on_entry, &on_status);
            }
        };

        let error_handler = {
            let on_status = on_status.clone();
            move |e: WebApiError| {
                on_status.emit(ContractStatus::Error(format!("{e:?}")));
            }
        };

        let onopen_handler = {
            let api = api.clone();
            let on_status = on_status.clone();
            move || {
                // Once the socket is open, fire off the `Subscribe`
                // request. We do it inside `spawn_local` because
                // `WsShim::send` is async and we're inside a sync
                // browser callback. Errors at this stage become
                // `Error(...)` status updates.
                let api = api.clone();
                let on_status = on_status.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let mut guard = api.borrow_mut();
                    let Some(api) = guard.as_mut() else {
                        on_status.emit(ContractStatus::Error(
                            "WsShim was dropped before onopen fired".into(),
                        ));
                        return;
                    };
                    let req = ClientRequest::ContractOp(ContractRequest::Subscribe {
                        key: instance,
                        summary: None,
                    });
                    if let Err(e) = api.send(req).await {
                        on_status.emit(ContractStatus::Error(format!(
                            "subscribe send: {e:?}"
                        )));
                        return;
                    }
                    on_status.emit(ContractStatus::Subscribing);
                    // Issue a parallel `Get` so we receive a snapshot
                    // of the contract's CURRENT state. `Subscribe`
                    // alone only delivers `UpdateNotification`s for
                    // *future* changes — every entry that was already
                    // in the contract before this tab loaded would be
                    // invisible without an explicit Get. The
                    // `GetResponse` handler in `handle_host_response`
                    // decodes the state and emits each verified
                    // `RemoteEntry` exactly the same way Update path
                    // does.
                    let get = ClientRequest::ContractOp(ContractRequest::Get {
                        key: instance,
                        return_contract_code: false,
                        subscribe: false,
                        blocking_subscribe: false,
                    });
                    if let Err(e) = api.send(get).await {
                        web_sys::console::log_1(
                            &format!("[net-graph] initial Get send err: {e:?}").into(),
                        );
                    }
                });
            }
        };

        let started = WsShim::start(socket, result_handler, error_handler, onopen_handler);
        *api.borrow_mut() = Some(started);

        Ok(Self { api })
    }

}

fn handle_host_response(
    res: Result<HostResponse, ClientError>,
    on_entry: &Callback<Vec<RemoteEntry>>,
    on_status: &Callback<ContractStatus>,
) {
    let resp = match res {
        Ok(r) => r,
        Err(e) => {
            on_status.emit(ContractStatus::Error(format!("host error: {e}")));
            return;
        }
    };
    match resp {
        HostResponse::ContractResponse(ContractResponse::SubscribeResponse { .. }) => {
            on_status.emit(ContractStatus::Subscribed);
        }
        HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. }) => {
            // On a fresh subscribe, freenet-core delivers the *current
            // full state* of the contract via `GetResponse` (one shot,
            // before any later `UpdateNotification` deltas). The
            // bytes are a bincode-encoded `ContractState` — same
            // codec the publisher uses, just at the state level rather
            // than the per-update delta level. Decode and emit every
            // verified entry, otherwise the dashboard would only
            // ever see updates that happened *after* it subscribed
            // and miss every existing publisher in the contract.
            match try_decode(state.as_ref()) {
                Ok(entries) => {
                    on_entry.emit(entries);
                }
                Err(reason) => {
                    on_status.emit(ContractStatus::Error(format!(
                        "initial state decode: {reason}"
                    )));
                }
            }
            on_status.emit(ContractStatus::Subscribed);
        }
        HostResponse::ContractResponse(ContractResponse::PutResponse { .. }) => {
            // Our own publishes echo back here — confirm subscribed too
            // since the WS round-tripped at least one contract op.
            on_status.emit(ContractStatus::Subscribed);
        }
        HostResponse::ContractResponse(ContractResponse::UpdateNotification { update, .. }) => {
            // The freenet-core update channel can carry deltas, full
            // state snapshots, or both. The topology contract emits
            // deltas on every publish (see
            // `topology-contract::get_state_delta`); state snapshots
            // come on the very first event after a fresh subscribe.
            // We handle every variant by extracting the inner bytes
            // and feeding them to the same signed-entry decoder.
            for bytes in collect_payload_bytes(&update) {
                match try_decode(&bytes) {
                    Ok(entries) => {
                        on_entry.emit(entries);
                        on_status.emit(ContractStatus::Subscribed);
                    }
                    Err(reason) => {
                        // Don't kill the subscription on a single bad
                        // entry — surface via status and keep going.
                        on_status.emit(ContractStatus::Error(format!(
                            "decode failed: {reason}"
                        )));
                    }
                }
            }
        }
        _ => { /* ignore non-contract responses */ }
    }
}

fn collect_payload_bytes(update: &UpdateData<'_>) -> Vec<Vec<u8>> {
    match update {
        UpdateData::Delta(d) => vec![d.as_ref().to_vec()],
        UpdateData::State(s) => vec![s.as_ref().to_vec()],
        UpdateData::StateAndDelta { delta, state } => {
            vec![state.as_ref().to_vec(), delta.as_ref().to_vec()]
        }
        UpdateData::RelatedDelta { delta, .. } => vec![delta.as_ref().to_vec()],
        UpdateData::RelatedState { state, .. } => vec![state.as_ref().to_vec()],
        UpdateData::RelatedStateAndDelta { state, delta, .. } => {
            vec![state.as_ref().to_vec(), delta.as_ref().to_vec()]
        }
        // `UpdateData` is `#[non_exhaustive]` upstream; default to no-op
        // on future variants rather than block the subscription.
        _ => vec![],
    }
}

/// Try both wire shapes the topology contract emits:
///   1. `ContractDelta` — what `get_state_delta` returns.
///   2. `ContractState` — full snapshot, sent on first event after a
///      fresh subscribe.
///
/// Whichever decode succeeds, every entry must verify against its own
/// embedded Ed25519 public key — bad signatures are dropped silently
/// rather than mixing into the graph.
fn try_decode(bytes: &[u8]) -> Result<Vec<RemoteEntry>, String> {
    use shared::contract::{ContractDelta, ContractState};
    if let Ok(delta) = bincode::deserialize::<ContractDelta>(bytes) {
        return Ok(verified_entries(delta.entries));
    }
    if let Ok(state) = bincode::deserialize::<ContractState>(bytes) {
        return Ok(verified_entries(state.entries.into_values().collect()));
    }
    Err("payload is neither ContractDelta nor ContractState".into())
}

fn verified_entries(entries: Vec<SignedEntry>) -> Vec<RemoteEntry> {
    entries
        .into_iter()
        .filter_map(|signed| {
            let payload = signed.verify().ok()?;
            Some(RemoteEntry {
                publisher_pubkey_hex: hex::encode(payload.public_key),
                payload,
            })
        })
        .collect()
}
