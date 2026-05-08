use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::str::FromStr;

use ed25519_dalek::SigningKey;
use freenet_stdlib::prelude::{CodeHash, ContractInstanceId};
use gloo_timers::callback::{Interval, Timeout};
use shared::contract::{decode_contract_entry, EntryPayload, NeighborInfo};
use shared::{ContractMeta, ContractView, FetchStatus, GatewayView, KnownNode, PeerView, Topology};
use wasm_bindgen_futures::JsFuture;
use yew::prelude::*;

mod contract_client;
mod graph;
mod settings;

use contract_client::{ContractClient, ContractStatus, RemoteEntry};
use settings::{LayoutSettings, PersistedFilter, Settings};

#[derive(Clone, Copy, Debug, PartialEq)]
enum ItemKind {
    Node,
    Contract,
}

#[function_component(App)]
fn app() -> Html {
    // ---- persistent state, hydrated from localStorage ----------------
    let settings = use_state(|| settings::load_from_storage().unwrap_or_default());

    // ---- transient state ---------------------------------------------
    let search_query = use_state(String::new);
    let selected: UseStateHandle<Option<String>> = use_state(|| None);
    let is_dragging = use_state(|| false);
    let last_copied: UseStateHandle<Option<String>> = use_state(|| None);
    let drawer_open = use_state(|| false);
    // Verified entries received over the topology-contract subscription.
    // Per-publisher LWW: keyed by hex pubkey, replaced when an entry's
    // `timestamp_ms` is strictly newer. This is the dashboard's *only*
    // data source after the path-A refactor — the backend's
    // `/api/topology` polling is gone.
    let remote_entries: UseStateHandle<HashMap<String, RemoteEntry>> = use_state(HashMap::new);
    let contract_status = use_state(|| ContractStatus::Disabled);
    // Owns the live ContractClient handle so dropping it closes the WS.
    // `use_mut_ref` keeps the handle across renders without triggering
    // them on each mutation.
    let contract_client_holder: Rc<RefCell<Option<ContractClient>>> = use_mut_ref(|| None);

    // ---- contract subscription lifecycle ----------------------------
    // (Re)spawn the ContractClient whenever the contract settings change,
    // and tear it down on disable. Dropping the previous client closes
    // its WebSocket; the new one opens fresh.
    {
        let holder = contract_client_holder.clone();
        let remote_entries = remote_entries.clone();
        let contract_status = contract_status.clone();
        let contract_dep = settings.contract.clone();
        use_effect_with(contract_dep, move |cfg| {
            // Always drop the previous handle before potentially starting
            // a new one — RAII close.
            *holder.borrow_mut() = None;

            if !cfg.enabled || cfg.instance_id.trim().is_empty() {
                contract_status.set(ContractStatus::Disabled);
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }

            let on_entry = {
                let remote_entries = remote_entries.clone();
                // Batched: a single decoded message can carry many
                // `RemoteEntry`s (initial `GetResponse.state` after a
                // fresh subscribe contains every existing publisher's
                // entry). Emitting them one-by-one and relying on
                // `UseStateHandle::set` to compose was racy — each
                // closure invocation cloned the current `remote_entries`
                // value, which Yew's `set` updates *asynchronously*, so
                // 8 sync emits would all see the same starting map and
                // the last one overwrote the rest. Take a Vec, fold
                // it into the existing map under LWW per-pubkey, and
                // call `set` exactly once.
                Callback::from(move |entries: Vec<RemoteEntry>| {
                    if entries.is_empty() {
                        return;
                    }
                    let mut map = (*remote_entries).clone();
                    let mut changed = false;
                    for e in entries {
                        let key = e.publisher_pubkey_hex.clone();
                        let take = match map.get(&key) {
                            Some(existing) => {
                                e.payload.timestamp_ms > existing.payload.timestamp_ms
                            }
                            None => true,
                        };
                        if take {
                            map.insert(key, e);
                            changed = true;
                        }
                    }
                    if changed {
                        remote_entries.set(map);
                    }
                })
            };
            let on_status = {
                let contract_status = contract_status.clone();
                Callback::from(move |s: ContractStatus| contract_status.set(s))
            };

            match ContractClient::start(cfg, on_entry, on_status) {
                Ok(client) => {
                    *holder.borrow_mut() = Some(client);
                }
                Err(e) => {
                    contract_status.set(ContractStatus::Error(e));
                }
            }

            let holder_for_cleanup = holder.clone();
            Box::new(move || {
                *holder_for_cleanup.borrow_mut() = None;
            }) as Box<dyn FnOnce()>
        });
    }

    // ---- identity bootstrap: generate a seed the first time the user
    //      enables publishing. We do this in an effect (rather than at
    //      the toggle-change site) so the same logic also triggers on a
    //      fresh load when localStorage carries `publish_enabled = true`
    //      but no seed somehow.
    {
        let settings = settings.clone();
        let dep = (
            settings.contract.publish_enabled,
            settings.contract.identity_seed_hex.is_empty(),
        );
        use_effect_with(dep, move |&(enabled, missing)| {
            if enabled && missing {
                let mut next = (*settings).clone();
                next.contract.identity_seed_hex = settings::generate_identity_seed_hex();
                settings::save_to_storage(&next);
                settings.set(next);
            }
            || ()
        });
    }

    // ---- periodic publisher worker -----------------------------------
    // Runs only when:
    //   * `publish_enabled = true`
    //   * we have a usable identity seed
    //   * `instance_id` and `code_hash` are both filled in
    //   * a ContractClient is actually open (so we can send Update)
    // Fetches the local node's `/` dashboard each interval, parses peers
    // + contracts via the same scraper module the legacy backend used,
    // builds an EntryPayload, signs with the user's seed, and pushes
    // through the existing WS via `ContractClient::publish`.
    {
        let holder = contract_client_holder.clone();
        let settings_dep = (settings.contract.clone(), settings.known_nodes.clone());
        let contract_status = contract_status.clone();
        use_effect_with(settings_dep, move |dep| {
            let cfg = dep.0.clone();
            let known_nodes = dep.1.clone();
            // Cleanup: a `None` Interval aborts the effect with no-op.
            if !cfg.publish_enabled
                || cfg.identity_seed_hex.is_empty()
                || cfg.instance_id.trim().is_empty()
                || cfg.code_hash.trim().is_empty()
            {
                return Box::new(|| ()) as Box<dyn FnOnce()>;
            }

            let instance_id = match ContractInstanceId::from_str(cfg.instance_id.trim()) {
                Ok(id) => id,
                Err(e) => {
                    contract_status.set(ContractStatus::Error(format!(
                        "publish: bad instance_id: {e}"
                    )));
                    return Box::new(|| ()) as Box<dyn FnOnce()>;
                }
            };
            let code_hash = match decode_code_hash(&cfg.code_hash) {
                Ok(h) => h,
                Err(e) => {
                    contract_status.set(ContractStatus::Error(format!(
                        "publish: bad code_hash: {e}"
                    )));
                    return Box::new(|| ()) as Box<dyn FnOnce()>;
                }
            };
            let signing_key = match decode_signing_key(&cfg.identity_seed_hex) {
                Some(sk) => sk,
                None => {
                    contract_status.set(ContractStatus::Error(
                        "publish: identity seed is malformed".into(),
                    ));
                    return Box::new(|| ()) as Box<dyn FnOnce()>;
                }
            };

            let interval_ms = cfg.publish_interval_secs.saturating_mul(1000);
            let interval = {
                let holder = holder.clone();
                let contract_status = contract_status.clone();
                let signing_key = signing_key.clone();
                let known_nodes = known_nodes.clone();
                Interval::new(interval_ms, move || {
                    let holder = holder.clone();
                    let signing_key = signing_key.clone();
                    let contract_status = contract_status.clone();
                    let known_nodes = known_nodes.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(reason) = publish_one_cycle(
                            &holder,
                            &signing_key,
                            instance_id,
                            code_hash,
                            known_nodes,
                        )
                        .await
                        {
                            contract_status
                                .set(ContractStatus::Error(format!("publish: {reason}")));
                        }
                    });
                })
            };

            // Fire one immediate publish so the user gets feedback right
            // after toggling, without waiting a full interval.
            {
                let holder = holder.clone();
                let signing_key = signing_key.clone();
                let contract_status = contract_status.clone();
                let known_nodes = known_nodes.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(reason) = publish_one_cycle(
                        &holder,
                        &signing_key,
                        instance_id,
                        code_hash,
                        known_nodes,
                    )
                    .await
                    {
                        contract_status.set(ContractStatus::Error(format!("publish: {reason}")));
                    }
                });
            }

            Box::new(move || drop(interval)) as Box<dyn FnOnce()>
        });
    }

    // ---- topology built fresh from the subscription each render -----
    // No polling. The graph reflects whatever entries we've verified
    // through the contract so far, plus the user's `known_nodes` list.
    let topo = Rc::new(build_topology(&remote_entries, &settings.known_nodes));
    // Error banner now shows the contract-subscription failure state
    // (config error, WS closed, decode failure) — there's no second
    // "fetch" channel any more.
    let err = match &*contract_status {
        ContractStatus::Error(msg) => Some(msg.clone()),
        _ => None,
    };

    let (header_meta, publisher_count) = {
        let t: &Topology = &topo;
        let total_peers: usize = t.gateways.iter().map(|g| g.peers.len()).sum();
        let nodes = flat_nodes(t);
        let contracts = flat_contracts(t);
        let publishers = t.gateways.len();
        let meta = format!(
            "{} publisher(s) • {} known • {} peer-edge(s) • {} unique nodes • {} unique contracts",
            publishers,
            t.known_nodes.len(),
            total_peers,
            nodes.len(),
            contracts.len(),
        );
        (meta, publishers)
    };

    // ---- callbacks ---------------------------------------------------
    let on_search_input = {
        let search_query = search_query.clone();
        Callback::from(move |e: InputEvent| {
            let target: web_sys::HtmlInputElement = e.target_unchecked_into();
            search_query.set(target.value());
        })
    };
    let on_search_clear = {
        let search_query = search_query.clone();
        Callback::from(move |_: MouseEvent| search_query.set(String::new()))
    };
    let select_node = {
        let selected = selected.clone();
        Callback::from(move |id: String| {
            if selected.as_deref() == Some(id.as_str()) {
                selected.set(None);
            } else {
                selected.set(Some(id));
            }
        })
    };

    // Sidebar drag-resize. Updates settings AND persists at drag-end.
    let on_resize_start = {
        let is_dragging = is_dragging.clone();
        Callback::from(move |e: MouseEvent| {
            e.prevent_default();
            is_dragging.set(true);
        })
    };
    let on_drag_move = {
        let settings = settings.clone();
        Callback::from(move |e: MouseEvent| {
            let x = e.client_x().clamp(220, 800);
            let mut next = (*settings).clone();
            if next.sidebar_width != x {
                next.sidebar_width = x;
                settings.set(next);
            }
        })
    };
    let on_drag_end = {
        let is_dragging = is_dragging.clone();
        let settings = settings.clone();
        Callback::from(move |_: MouseEvent| {
            settings::save_to_storage(&settings);
            is_dragging.set(false);
        })
    };

    let on_copy = {
        let last_copied = last_copied.clone();
        Callback::from(move |value: String| {
            if let Some(window) = web_sys::window() {
                let promise = window.navigator().clipboard().write_text(&value);
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = JsFuture::from(promise).await;
                });
            }
            last_copied.set(Some(value));
            let last_copied = last_copied.clone();
            Timeout::new(1200, move || last_copied.set(None)).forget();
        })
    };

    let on_filter_change = {
        let settings = settings.clone();
        Callback::from(move |m: PersistedFilter| {
            let mut next = (*settings).clone();
            next.filter_mode = m;
            settings::save_to_storage(&next);
            settings.set(next);
        })
    };

    let on_settings_update = {
        let settings = settings.clone();
        Callback::from(move |new: Settings| {
            settings::save_to_storage(&new);
            settings.set(new);
        })
    };

    let toggle_drawer = {
        let drawer_open = drawer_open.clone();
        Callback::from(move |_: MouseEvent| drawer_open.set(!*drawer_open))
    };

    let main_grid_style = format!("grid-template-columns: {}px 6px 1fr;", settings.sidebar_width);
    let last_copied_value = (*last_copied).clone();
    let current_settings = (*settings).clone();
    let layout = current_settings.layout;

    html! {
        <div id="app">
            <header>
                <h1>{"Freenet "}<span class="accent">{"net-graph"}</span></h1>
                <div class="meta">{ header_meta }</div>
                <button class="header-btn" onclick={toggle_drawer.clone()} title="Settings">{"⚙"}</button>
            </header>
            <main style={main_grid_style}>
                <aside>
                    {
                        if let Some(msg) = err.as_ref() {
                            html! { <p class="err-msg">{ format!("Last error: {}", msg) }</p> }
                        } else { html!{} }
                    }
                    {
                        render_search_panel(
                            &topo,
                            &*search_query,
                            on_search_input.clone(),
                            on_search_clear.clone(),
                            current_settings.filter_mode,
                            on_filter_change.clone(),
                            selected.as_deref(),
                            select_node.clone(),
                            on_copy.clone(),
                            last_copied_value.as_deref(),
                        )
                    }
                </aside>
                <div class="resizer"
                     onmousedown={on_resize_start}
                     title="Drag to resize sidebar"></div>
                <div class="graph-wrap">
                    <graph::Graph topology={topo.clone()} selected={(*selected).clone()} layout={layout} />
                    {
                        if publisher_count == 0 {
                            // No verified entries from the contract yet —
                            // either the user hasn't subscribed/published, or
                            // they're the first dashboard on this contract.
                            // Either way, the graph only shows static
                            // known_nodes; nudge them at the settings.
                            let toggle_drawer_inline = toggle_drawer.clone();
                            html! {
                                <div class="empty-hint">
                                    <h3>{"Graph is sparse — here's how to fill it"}</h3>
                                    <ol>
                                        <li>{"Open ⚙ → "}<b>{"🔗 Network sharing"}</b>
                                            {" and turn on "}<b>{"enabled"}</b>{" + "}
                                            <b>{"publish enabled"}</b>{". You start \
                                            publishing your own entry."}</li>
                                        <li>{"Open ⚙ → "}<b>{"🌐 Data sources → \
                                            Known public nodes"}</b>{" and add the \
                                            peers you want others to see in your \
                                            entry's "}<code>{"neighbors"}</code>{" list."}</li>
                                        <li>{"Get other operators to open this same \
                                            URL on their own freenet nodes and turn \
                                            "}<b>{"publish enabled"}</b>{" on too. \
                                            Each one adds a publisher to the graph."}</li>
                                    </ol>
                                    <p>{"The dashboard runs in a sandbox iframe and \
                                    cannot auto-discover its host node's peers \
                                    (CORS + freenet-core's webapp NodeQueries gate). \
                                    Manual + crowdsourced is the design."}</p>
                                    <button onclick={toggle_drawer_inline}>{"Open settings"}</button>
                                </div>
                            }
                        } else { html! {} }
                    }
                </div>
            </main>
            <footer>
                <div class="legend">
                    <span><span class="swatch" style="background: var(--gateway)"></span>{"gateway"}</span>
                    <span><span class="swatch hue-key"></span>{"node fill = location hue"}</span>
                    <span><span class="swatch" style="background: #6b7280"></span>{"location unknown"}</span>
                </div>
                <span>{ "Live subscription — push, not poll" }</span>
            </footer>
            {
                if *drawer_open {
                    html! { <SettingsDrawer
                        settings={current_settings.clone()}
                        on_update={on_settings_update.clone()}
                        on_close={toggle_drawer.clone()}
                        contract_status={(*contract_status).clone()}
                        remote_entry_count={remote_entries.len()}
                    /> }
                } else { html! {} }
            }
            {
                if *is_dragging {
                    html! {
                        <div class="drag-overlay"
                             onmousemove={on_drag_move}
                             onmouseup={on_drag_end.clone()}
                             onmouseleave={on_drag_end}>
                        </div>
                    }
                } else { html! {} }
            }
        </div>
    }
}

/// Build a fresh `Topology` from the contract-subscription state.
///
/// Each verified `RemoteEntry` becomes a synthetic `GatewayView` —
/// reusing the same data shape as the (now-deprecated) backend scrape
/// pipeline. The label is `"remote: {pubkey-prefix}"`, and the entry's
/// `neighbors` and `contracts` populate the corresponding fields. The
/// existing `flat_nodes` / `flat_contracts` / dedup-by-address pipeline
/// keeps working unchanged.
///
/// `fetched_at` is the freshest publish timestamp across all entries
/// (in seconds since epoch), giving the user a sense of how stale the
/// graph is when no one is publishing.
///
/// `contract_meta` is populated from the per-entry `is_webapp` / `title`
/// hints the daemon publishes — it probes `/v1/contract/web/<key>/` on
/// its local node (something the sandboxed dashboard iframe can't do
/// because of CORS) and encodes the result inside each contract entry
/// via `shared::contract::encode_contract_entry`. Browser-side skeleton
/// publishers ship bare keys, which decode as `(key, None, None)` and
/// leave the meta unset — those contracts render without a badge until
/// some daemon publisher classifies them.
fn build_topology(
    remote: &HashMap<String, RemoteEntry>,
    known_nodes: &[KnownNode],
) -> Topology {
    let mut gateways = Vec::with_capacity(remote.len());
    let mut newest_ts_ms: u64 = 0;
    let mut contract_meta: BTreeMap<String, ContractMeta> = BTreeMap::new();

    for entry in remote.values() {
        let p = &entry.payload;
        newest_ts_ms = newest_ts_ms.max(p.timestamp_ms);

        let pubkey_prefix: String = entry
            .publisher_pubkey_hex
            .chars()
            .take(8)
            .collect();

        let peers = p
            .neighbors
            .iter()
            .map(|n| PeerView {
                address: n.address.clone(),
                is_gateway: n.is_gateway,
                location: n.location,
                connected: None,
            })
            .collect();

        // Each contract entry is either a bare base58 key or an enriched
        // string from a probe-capable daemon. Strip the enrichment for
        // the `ContractView.key` (downstream code keys on raw base58)
        // and merge the metadata into the `contract_meta` map.
        let contracts = p
            .contracts
            .iter()
            .map(|raw| {
                let (key, is_webapp, title) = decode_contract_entry(raw);
                if is_webapp.is_some() || title.is_some() {
                    let slot = contract_meta.entry(key.clone()).or_insert(ContractMeta {
                        has_web_interface: false,
                        title: None,
                        probed_at: p.timestamp_ms / 1000,
                    });
                    if let Some(w) = is_webapp {
                        // Any positive sighting wins — treat `false` as
                        // a vote that doesn't override an earlier `true`.
                        if w {
                            slot.has_web_interface = true;
                        }
                    }
                    if slot.title.is_none() {
                        if let Some(t) = title {
                            slot.title = Some(t);
                        }
                    }
                    let ts = p.timestamp_ms / 1000;
                    if ts > slot.probed_at {
                        slot.probed_at = ts;
                    }
                }
                ContractView {
                    key,
                    subscribed_ago: None,
                    last_update_ago: None,
                }
            })
            .collect();

        gateways.push(GatewayView {
            label: format!("remote: {pubkey_prefix}"),
            url: format!("(contract • {})", entry.publisher_pubkey_hex),
            status: FetchStatus::Ok,
            own_location: p.own_location,
            external_address: if p.external_address.is_empty() {
                None
            } else {
                Some(p.external_address.clone())
            },
            version: p.version.clone(),
            peers,
            contracts,
        });
    }

    Topology {
        gateways,
        known_nodes: known_nodes.to_vec(),
        contract_meta,
        fetched_at: newest_ts_ms / 1000,
    }
}

// ============================ flat node + contract aggregation ============================

#[derive(Clone, Debug)]
struct FlatNode {
    id: String,
    label: String,
    is_gateway: bool,
    is_public_default: bool,
    location: Option<f64>,
    seen_by: Vec<String>,
    connected: Option<String>,
    scrape_status: Option<FetchStatus>,
    version: Option<String>,
    scrape_url: Option<String>,
    peer_count: Option<usize>,
}

fn flat_nodes(t: &Topology) -> Vec<FlatNode> {
    let mut by_addr: HashMap<String, FlatNode> = HashMap::new();

    let upsert = |map: &mut HashMap<String, FlatNode>, n: FlatNode| {
        map.entry(n.id.clone())
            .and_modify(|existing| {
                if n.is_gateway { existing.is_gateway = true; }
                if n.is_public_default { existing.is_public_default = true; }
                if existing.location.is_none() { existing.location = n.location; }
                for label in &n.seen_by {
                    if !existing.seen_by.contains(label) {
                        existing.seen_by.push(label.clone());
                    }
                }
                if let Some(c) = &n.connected {
                    if existing.connected.as_deref().map(|e| c.len() > e.len()).unwrap_or(true) {
                        existing.connected = Some(c.clone());
                    }
                }
                if existing.label == existing.id { existing.label = n.label.clone(); }
                if existing.scrape_status.is_none() { existing.scrape_status = n.scrape_status.clone(); }
                if existing.version.is_none() { existing.version = n.version.clone(); }
                if existing.scrape_url.is_none() { existing.scrape_url = n.scrape_url.clone(); }
                if existing.peer_count.is_none() { existing.peer_count = n.peer_count; }
            })
            .or_insert(n);
    };

    for gw in &t.gateways {
        let gw_addr = gw.external_address.clone().unwrap_or_else(|| format!("gw::{}", gw.label));
        upsert(&mut by_addr, FlatNode {
            id: gw_addr.clone(),
            label: gw.label.clone(),
            is_gateway: true,
            is_public_default: false,
            location: gw.own_location,
            seen_by: Vec::new(),
            connected: None,
            scrape_status: Some(gw.status.clone()),
            version: gw.version.clone(),
            scrape_url: Some(gw.url.clone()),
            peer_count: Some(gw.peers.len()),
        });
        for peer in &gw.peers {
            upsert(&mut by_addr, FlatNode {
                id: peer.address.clone(),
                label: peer.address.clone(),
                is_gateway: peer.is_gateway,
                is_public_default: false,
                location: peer.location,
                seen_by: vec![gw.label.clone()],
                connected: peer.connected.clone(),
                scrape_status: None,
                version: None,
                scrape_url: None,
                peer_count: None,
            });
        }
    }

    for kn in &t.known_nodes {
        upsert(&mut by_addr, FlatNode {
            id: kn.address.clone(),
            label: format!("{} ({})", kn.label, kn.address),
            is_gateway: true,
            // `is_public_default` used to flag the
            // baked-in operator anchors and label them as "public".
            // The field name was misleading — these are gateways the
            // user (or operator) chose to anchor the graph with, not
            // a privacy classification — so we keep the flag at
            // `false` and let the row render as a plain gateway.
            is_public_default: false,
            location: kn.location,
            seen_by: Vec::new(),
            connected: None,
            scrape_status: None,
            version: None,
            scrape_url: None,
            peer_count: None,
        });
    }

    let mut out: Vec<FlatNode> = by_addr.into_values().collect();
    out.sort_by(|a, b| {
        let bucket = |n: &FlatNode| {
            if n.is_public_default { 1 } else if n.is_gateway { 0 } else { 2 }
        };
        bucket(a).cmp(&bucket(b)).then_with(|| a.id.cmp(&b.id))
    });
    out
}

#[derive(Clone, Debug)]
struct ListItem {
    kind: ItemKind,
    node: Option<FlatNode>,
    contract: Option<FlatContract>,
}

#[derive(Clone, Debug)]
struct FlatContract {
    /// Canonical key — first instance encountered for this row. Used as
    /// the row's selection id and the value of the copy button.
    key: String,
    short: String,
    seen_by: Vec<String>,
    subscribed_ago: Option<String>,
    last_update_ago: Option<String>,
    has_web_interface: Option<bool>,
    title: Option<String>,
    /// Every distinct contract instance id collapsed into this row.
    /// Always includes `key`. `len() > 1` means several instances share
    /// the same `<title>` (e.g. 11 "Freenet File" contracts pointing at
    /// the same webapp template) — the row shows them as one with an
    /// "{N} instances" badge and lets search match any of them.
    instance_keys: Vec<String>,
}

fn flat_contracts(t: &Topology) -> Vec<FlatContract> {
    // First pass: dedup by raw contract key — the same key seen via
    // multiple publishers is one contract.
    let mut by_key: HashMap<String, FlatContract> = HashMap::new();
    for gw in &t.gateways {
        for c in &gw.contracts {
            let meta = t.contract_meta.get(&c.key);
            let entry = by_key.entry(c.key.clone()).or_insert_with(|| FlatContract {
                key: c.key.clone(),
                short: short_key(&c.key),
                seen_by: Vec::new(),
                subscribed_ago: None,
                last_update_ago: None,
                has_web_interface: meta.map(|m| m.has_web_interface),
                title: meta.and_then(|m| m.title.clone()),
                instance_keys: vec![c.key.clone()],
            });
            if !entry.seen_by.contains(&gw.label) {
                entry.seen_by.push(gw.label.clone());
            }
            if entry.subscribed_ago.is_none() {
                entry.subscribed_ago = c.subscribed_ago.clone();
            }
            if entry.last_update_ago.is_none() {
                entry.last_update_ago = c.last_update_ago.clone();
            } else if let (Some(existing), Some(incoming)) = (&entry.last_update_ago, &c.last_update_ago) {
                if incoming.len() < existing.len() {
                    entry.last_update_ago = Some(incoming.clone());
                }
            }
        }
    }

    // Second pass: collapse rows that share a webapp `<title>`. Many
    // webapps (e.g. "River", "Freenet File", "Freenet Field Guide")
    // ship the same HTML template across hundreds of state instances —
    // every parameterised instance has its own ContractInstanceId but
    // identical `<title>`, so they look like duplicates to the user.
    // We group by lowercased title; each group keeps its first row as
    // canonical and accumulates the rest as `instance_keys`. Untitled
    // contracts (data-only or unprobed) stay one-row-per-key.
    //
    // We only collapse when `has_web_interface == Some(true)` — without
    // a confirmed webapp signal, two contracts with no title share an
    // empty group key, which would fold all unprobed contracts into one
    // row. The webapp gate keeps the grouping safe even before probes
    // complete.
    let mut grouped: Vec<FlatContract> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut singletons: Vec<FlatContract> = Vec::new();

    for entry in by_key.into_values() {
        let group_key = match (entry.has_web_interface, entry.title.as_deref()) {
            (Some(true), Some(t)) if !t.trim().is_empty() => Some(t.trim().to_lowercase()),
            _ => None,
        };
        match group_key {
            None => singletons.push(entry),
            Some(gk) => match group_index.get(&gk) {
                Some(&idx) => merge_into(&mut grouped[idx], entry),
                None => {
                    group_index.insert(gk, grouped.len());
                    grouped.push(entry);
                }
            },
        }
    }

    let mut out: Vec<FlatContract> = grouped;
    out.append(&mut singletons);
    // Sort: confirmed webapps first (by title when available, else key),
    // then not-yet-probed (subscribers may still be filling the cache),
    // then confirmed data-only contracts. Within each bucket: alphabetic
    // on the user-visible label so the order is stable across polls.
    out.sort_by(|a, b| {
        let bucket = |c: &FlatContract| match c.has_web_interface {
            Some(true) => 0,
            None => 1,
            Some(false) => 2,
        };
        let label = |c: &FlatContract| {
            c.title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or(c.key.as_str())
                .to_lowercase()
        };
        bucket(a)
            .cmp(&bucket(b))
            .then_with(|| label(a).cmp(&label(b)))
    });
    out
}

/// Fold one same-title contract into an existing group row.
fn merge_into(dst: &mut FlatContract, src: FlatContract) {
    for k in src.instance_keys {
        if !dst.instance_keys.contains(&k) {
            dst.instance_keys.push(k);
        }
    }
    for label in src.seen_by {
        if !dst.seen_by.contains(&label) {
            dst.seen_by.push(label);
        }
    }
    if dst.title.is_none() {
        dst.title = src.title;
    }
    if dst.has_web_interface != Some(true) {
        dst.has_web_interface = src.has_web_interface;
    }
}

fn short_key(key: &str) -> String {
    if key.chars().count() > 16 {
        let trunc: String = key.chars().take(16).collect();
        format!("{trunc}…")
    } else {
        key.to_string()
    }
}

// ============================ search panel + rows ============================

#[allow(clippy::too_many_arguments)]
fn render_search_panel(
    t: &Topology,
    query: &str,
    on_input: Callback<InputEvent>,
    on_clear: Callback<MouseEvent>,
    filter: PersistedFilter,
    on_filter_change: Callback<PersistedFilter>,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
) -> Html {
    let nodes = flat_nodes(t);
    let contracts = flat_contracts(t);

    let q = query.trim().to_lowercase();
    let mut items: Vec<ListItem> = Vec::new();

    match filter {
        PersistedFilter::Nodes => {
            for n in nodes {
                if !q.is_empty()
                    && !(n.id.to_lowercase().contains(&q)
                        || n.label.to_lowercase().contains(&q)
                        || n.seen_by.iter().any(|s| s.to_lowercase().contains(&q)))
                { continue; }
                items.push(ListItem { kind: ItemKind::Node, node: Some(n), contract: None });
            }
        }
        PersistedFilter::Contracts => {
            for c in contracts {
                if !q.is_empty()
                    && !(c.instance_keys.iter().any(|k| k.to_lowercase().contains(&q))
                        || c.seen_by.iter().any(|s| s.to_lowercase().contains(&q))
                        || c.title.as_deref().map(|t| t.to_lowercase().contains(&q)).unwrap_or(false))
                { continue; }
                items.push(ListItem { kind: ItemKind::Contract, node: None, contract: Some(c) });
            }
        }
    }

    let header_text = match filter {
        PersistedFilter::Nodes => "Nodes",
        PersistedFilter::Contracts => "Contracts",
    };

    let tab = |label: &'static str, mode: PersistedFilter| -> Html {
        let active = filter == mode;
        let cb = on_filter_change.clone();
        let onclick = Callback::from(move |_: MouseEvent| cb.emit(mode));
        let class = classes!("filter-tab", active.then_some("active"));
        html! { <button class={class} onclick={onclick}>{ label }</button> }
    };

    html! {
        <>
            <h2>{ header_text }</h2>
            <div class="filter-tabs">
                { tab("Nodes", PersistedFilter::Nodes) }
                { tab("Contracts", PersistedFilter::Contracts) }
            </div>
            <div class="search-row">
                <input
                    class="search-input" type="text"
                    placeholder="filter by key, address, label, gateway…"
                    value={query.to_string()}
                    oninput={on_input}
                />
                {
                    if !query.is_empty() {
                        html! { <button class="search-clear" onclick={on_clear} title="clear">{"✕"}</button> }
                    } else { html!{} }
                }
            </div>
            <div class="node-list">
                {
                    if items.is_empty() {
                        html! { <p class="empty">{"no matches"}</p> }
                    } else {
                        items.iter().map(|it| render_list_row(it, selected, on_pick.clone(), on_copy.clone(), last_copied)).collect::<Html>()
                    }
                }
            </div>
        </>
    }
}

fn render_list_row(
    item: &ListItem,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
) -> Html {
    match item.kind {
        ItemKind::Node => render_node_row(item.node.as_ref().unwrap(), selected, on_pick, on_copy, last_copied),
        ItemKind::Contract => render_contract_row(item.contract.as_ref().unwrap(), selected, on_pick, on_copy, last_copied),
    }
}

fn copy_button(value_to_copy: String, on_copy: Callback<String>, last_copied: Option<&str>) -> Html {
    let just_copied = last_copied == Some(value_to_copy.as_str());
    let class = classes!("copy-btn", just_copied.then_some("just-copied"));
    let onclick = {
        let on_copy = on_copy.clone();
        let value = value_to_copy.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            on_copy.emit(value.clone());
        })
    };
    let title = if just_copied { "copied" } else { "copy to clipboard" };
    let label = if just_copied { "✓" } else { "⧉" };
    html! { <button class={class} onclick={onclick} title={title}>{ label }</button> }
}

fn render_node_row(
    n: &FlatNode,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
) -> Html {
    let kind_class = if n.is_public_default { "kind-public" } else if n.is_gateway { "kind-gw" } else { "kind-peer" };
    let kind_text = if n.is_public_default { "public" } else if n.is_gateway { "gateway" } else { "peer" };
    let loc_str = n.location.map(|l| format!("loc {l:.4}")).unwrap_or_else(|| "loc —".to_string());
    let seen = if n.seen_by.is_empty() { String::new() } else { format!("via {}", n.seen_by.join(", ")) };
    let is_selected = selected == Some(n.id.as_str());
    let row_class = classes!("node-row", is_selected.then_some("selected"));
    let id = n.id.clone();
    let onclick = Callback::from(move |_: MouseEvent| on_pick.emit(id.clone()));
    let label_main = if n.label != n.id { n.label.as_str() } else { n.id.as_str() };
    let secondary = if n.label != n.id { Some(n.id.as_str()) } else { None };
    let hue_dot_style = match n.location {
        Some(loc) => {
            let hue = (loc.clamp(0.0, 1.0) * 360.0).round() as u32;
            format!("background: hsl({hue}, 65%, 55%);")
        }
        None => "background: #6b7280;".to_string(),
    };
    let (status_class, status_tooltip) = match n.scrape_status.as_ref() {
        Some(FetchStatus::Ok) => (Some("status-ok"), "scrape ok".to_string()),
        Some(FetchStatus::ParseFailed { message }) => (Some("status-warn"), format!("parse failed: {message}")),
        Some(FetchStatus::Unreachable { message }) => (Some("status-err"), format!("unreachable: {message}")),
        None => (None, String::new()),
    };
    html! {
        <div class={row_class} onclick={onclick}>
            <span class="hue-dot" style={hue_dot_style}></span>
            <div class="row-text">
                <div class="row-main">
                    <span class={classes!("kind-tag", kind_class)}>{ kind_text }</span>
                    {
                        if let Some(c) = status_class {
                            html! { <span class={classes!("status-dot", c)} title={status_tooltip.clone()}></span> }
                        } else { html!{} }
                    }
                    <span class="row-label">{ label_main }</span>
                    { if let Some(v) = &n.version { html! { <span class="row-tag">{ format!("v{v}") }</span> } } else { html!{} } }
                </div>
                { if let Some(addr) = secondary { html! { <div class="row-sub">{ addr }</div> } } else { html!{} } }
                <div class="row-sub">
                    <span>{ loc_str }</span>
                    { if let Some(pc) = n.peer_count { html! { <span>{" • "}{ format!("{pc} peer(s)") }</span> } } else { html!{} } }
                    { if let Some(c) = &n.connected { html! { <span>{" • "}{ c }</span> } } else { html!{} } }
                    { if !seen.is_empty() { html! { <span>{" • "}{ seen }</span> } } else { html!{} } }
                </div>
                { if let Some(url) = &n.scrape_url { html! { <div class="row-sub muted">{ url }</div> } } else { html!{} } }
            </div>
            { copy_button(n.id.clone(), on_copy, last_copied) }
        </div>
    }
}

fn render_contract_row(
    c: &FlatContract,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
) -> Html {
    let is_selected = selected == Some(c.key.as_str());
    let row_class = classes!("node-row", is_selected.then_some("selected"));
    let id = c.key.clone();
    let onclick = Callback::from(move |_: MouseEvent| on_pick.emit(id.clone()));
    let seen = if c.seen_by.is_empty() { String::new() } else { format!("via {}", c.seen_by.join(", ")) };
    let web_badge = match c.has_web_interface {
        Some(true) => Some(("web-badge web-yes", "✓ web")),
        Some(false) => Some(("web-badge web-no", "data only")),
        None => None,
    };
    let main_label: String = c.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| c.short.clone());
    let instance_count = c.instance_keys.len();
    let key_line = if instance_count > 1 {
        // Group of contracts collapsed by shared `<title>`. Show first
        // key, then `(+N more)` so the row stays compact but the user
        // can still copy / inspect each instance via the expanded sub.
        format!("{} (+{} more)", c.key, instance_count - 1)
    } else {
        c.key.clone()
    };
    html! {
        <div class={row_class} onclick={onclick}>
            <span class="hue-dot" style="background: #14b8a6;"></span>
            <div class="row-text">
                <div class="row-main">
                    <span class={classes!("kind-tag", "kind-contract")}>{"contract"}</span>
                    <span class="row-label">{ main_label }</span>
                    { if let Some((cls, text)) = web_badge { html! { <span class={classes!(cls)}>{ text }</span> } } else { html!{} } }
                    {
                        if instance_count > 1 {
                            html! { <span class="row-tag">{ format!("{instance_count} instances") }</span> }
                        } else { html!{} }
                    }
                    <span class="row-tag">{ format!("{} subs", c.seen_by.len()) }</span>
                </div>
                <div class="row-sub">{ key_line }</div>
                <div class="row-sub">
                    { if let Some(s) = &c.subscribed_ago { html! { <span>{"subscribed "}{ s }</span> } } else { html!{} } }
                    { if let Some(u) = &c.last_update_ago { html! { <span>{" • last update "}{ u }</span> } } else { html!{} } }
                    { if !seen.is_empty() { html! { <span>{" • "}{ seen }</span> } } else { html!{} } }
                </div>
            </div>
            { copy_button(c.key.clone(), on_copy, last_copied) }
        </div>
    }
}

// ============================ Settings drawer ============================

#[derive(Properties, PartialEq)]
struct SettingsDrawerProps {
    settings: Settings,
    on_update: Callback<Settings>,
    on_close: Callback<MouseEvent>,
    contract_status: ContractStatus,
    /// Number of distinct publishers we've heard from over the
    /// subscription. Drives the "X publishers seen" counter in the UI.
    remote_entry_count: usize,
}

#[function_component(SettingsDrawer)]
fn settings_drawer(props: &SettingsDrawerProps) -> Html {
    let s = props.settings.clone();

    // Mutators: each one builds a fresh Settings by editing one field, then
    // emits via `on_update`. The parent saves to localStorage.
    let on_update = props.on_update.clone();

    let mutate = {
        let on_update = on_update.clone();
        let s = s.clone();
        std::rc::Rc::new(move |f: &dyn Fn(&mut Settings)| {
            let mut next = s.clone();
            f(&mut next);
            on_update.emit(next.normalize());
        })
    };

    let on_sidebar_change = {
        let mutate = mutate.clone();
        Callback::from(move |e: InputEvent| {
            let target: web_sys::HtmlInputElement = e.target_unchecked_into();
            if let Ok(v) = target.value().parse::<i32>() {
                mutate(&|s| s.sidebar_width = v);
            }
        })
    };

    // Layout sliders. Each binds a numeric range to one LayoutSettings field.
    let layout_field = |label: &'static str,
                        min: f64, max: f64, step: f64,
                        value: f64,
                        set: std::rc::Rc<dyn Fn(&mut LayoutSettings, f64)>| {
        let mutate = mutate.clone();
        let on_change = Callback::from(move |e: InputEvent| {
            let target: web_sys::HtmlInputElement = e.target_unchecked_into();
            if let Ok(v) = target.value().parse::<f64>() {
                let set = set.clone();
                mutate(&move |s| set(&mut s.layout, v));
            }
        });
        html! {
            <div class="setting-row">
                <label>{ label }</label>
                <input type="range"
                    min={min.to_string()} max={max.to_string()} step={step.to_string()}
                    value={value.to_string()}
                    oninput={on_change}
                />
                <span class="setting-value">{ format!("{value:.4}") }</span>
            </div>
        }
    };

    let on_reset = {
        let on_update = on_update.clone();
        Callback::from(move |_: MouseEvent| {
            settings::clear_storage();
            // Emit defaults; parent will re-save them via on_update.
            on_update.emit(Settings::default());
        })
    };

    // Stop drawer-body clicks from bubbling up to the backdrop's close handler.
    let stop = Callback::from(|e: MouseEvent| e.stop_propagation());
    let backdrop_close = props.on_close.clone();

    // Identity row needs multiple let-bindings; Yew's html! `{ ... }`
    // accepts only single expressions, so we build the markup outside
    // and embed by reference.
    let identity_row: Html = {
        let pubkey = settings::derive_pubkey_hex(&s.contract.identity_seed_hex)
            .unwrap_or_else(|| "—".to_string());
        let on_regen = {
            let mutate = mutate.clone();
            Callback::from(move |_: MouseEvent| {
                mutate(&|s| {
                    s.contract.identity_seed_hex = settings::generate_identity_seed_hex();
                });
            })
        };
        let pubkey_short = if pubkey.len() > 24 {
            format!("{}…", &pubkey[..24])
        } else {
            pubkey.clone()
        };
        html! {
            <div class="setting-row">
                <label>{"identity (pubkey)"}</label>
                <code class="identity-pubkey" title={pubkey.clone()}>{ pubkey_short }</code>
                <button class="regen-btn" onclick={on_regen}
                    title="Regenerate — abandons this contract slot, claims a new one">
                    {"regenerate"}
                </button>
            </div>
        }
    };

    html! {
        <div class="drawer-backdrop" onclick={backdrop_close}>
            <div class="drawer" onclick={stop}>
                <div class="drawer-head">
                    <h2>{"Settings"}</h2>
                    <button class="drawer-close" onclick={props.on_close.clone()}>{"✕"}</button>
                </div>

                <section class="settings-group">
                <h3>{"🌐 Data sources"}</h3>
                <p class="hint">
                    {"Two streams populate this graph:"}
                </p>
                <ul class="hint">
                    <li>{"Live subscription to a topology contract (see "}
                        <code>{"Network sharing"}</code>{"). Each open dashboard \
                        with "}<code>{"publish enabled"}</code>{" \
                        contributes a signed entry; subscribers see them all merged."}</li>
                    <li>{"Your own "}<code>{"Known public nodes"}</code>
                        {" list (below). Anchors that appear in the graph \
                        regardless of whether anyone has published about them, \
                        AND are emitted as "}<code>{"neighbors"}</code>
                        {" of every entry you publish — so other subscribers \
                        also see them."}</li>
                </ul>
                <h4>{"Known public nodes"}</h4>
                <p class="hint">{"Each row is one peer. "}
                <code>{"address"}</code>{" is the network-side UDP endpoint \
                (e.g. "}<code>{"78.27.236.159:31337"}</code>{") — same format \
                a freenet-core node uses to dial a peer. "}
                <code>{"location"}</code>{" is the optional ring location \
                (0..1) you've observed for that peer."}</p>
                <p class="hint">
                    {"Sandbox limitation: this webapp can't auto-discover the \
                    local node's connected peers ("}<code>{"fetch /"}</code>
                    {" is CORS-blocked; "}<code>{"NodeQueries"}</code>
                    {" is rejected for webapps). Anything not entered here \
                    will be missing from your published entry."}
                </p>
                {
                    s.known_nodes.iter().enumerate().map(|(idx, kn)| {
                        let on_label = {
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlInputElement>().value();
                                mutate(&|s| if let Some(k) = s.known_nodes.get_mut(idx) { k.label = v.clone(); });
                            })
                        };
                        let on_addr = {
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlInputElement>().value();
                                mutate(&|s| if let Some(k) = s.known_nodes.get_mut(idx) { k.address = v.clone(); });
                            })
                        };
                        let on_loc = {
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlInputElement>().value();
                                let parsed = v.trim().parse::<f64>().ok().filter(|x| (0.0..1.0).contains(x));
                                mutate(&|s| if let Some(k) = s.known_nodes.get_mut(idx) { k.location = parsed; });
                            })
                        };
                        let on_remove = {
                            let mutate = mutate.clone();
                            Callback::from(move |_: MouseEvent| {
                                mutate(&|s| { if idx < s.known_nodes.len() { s.known_nodes.remove(idx); } });
                            })
                        };
                        let loc_str = kn.location.map(|l| format!("{l:.4}")).unwrap_or_default();
                        html! {
                            <div class="gw-row" key={idx}>
                                <input class="gw-label" type="text" placeholder="label" value={kn.label.clone()} oninput={on_label} />
                                <input class="gw-url"   type="text" placeholder="host:port" value={kn.address.clone()} oninput={on_addr} />
                                <input class="gw-loc"   type="text" placeholder="loc" value={loc_str} oninput={on_loc} />
                                <button class="gw-remove" onclick={on_remove}>{"✕"}</button>
                            </div>
                        }
                    }).collect::<Html>()
                }
                <button class="add-row" onclick={
                    let mutate = mutate.clone();
                    Callback::from(move |_: MouseEvent| {
                        mutate(&|s| s.known_nodes.push(KnownNode {
                            label: "new".to_string(),
                            address: "host:31337".to_string(),
                            location: None,
                            is_gateway: true,
                            source: "cli".to_string(),
                        }));
                    })
                }>{"+ add known node"}</button>

                </section>

                <section class="settings-group">
                <h3>{"⏱ Timing"}</h3>
                <div class="setting-row">
                    <label>{"animation tick (ms)"}</label>
                    <input type="number" min="16" max="200" step="1"
                        value={s.layout.tick_ms.to_string()}
                        oninput={
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let target: web_sys::HtmlInputElement = e.target_unchecked_into();
                                if let Ok(v) = target.value().parse::<u32>() {
                                    mutate(&|s| s.layout.tick_ms = v);
                                }
                            })
                        }
                    />
                    <span class="setting-value">
                        { format!("≈ {} fps", (1000.0 / s.layout.tick_ms.max(1) as f64).round() as i32) }
                    </span>
                </div>
                </section>

                <section class="settings-group">
                <h3>{"🎨 Display"}</h3>
                <div class="setting-row">
                    <label>{"sidebar width (px)"}</label>
                    <input type="number" min="220" max="800" step="10"
                        value={s.sidebar_width.to_string()}
                        oninput={on_sidebar_change} />
                </div>
                <div class="setting-row">
                    <label>{"default tab"}</label>
                    <select value={match s.filter_mode {
                            PersistedFilter::Nodes => "nodes",
                            PersistedFilter::Contracts => "contracts",
                        }}
                        oninput={
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlSelectElement>().value();
                                let parsed = match v.as_str() {
                                    "contracts" => PersistedFilter::Contracts,
                                    _ => PersistedFilter::Nodes,
                                };
                                mutate(&move |s| s.filter_mode = parsed);
                            })
                        }
                    >
                        <option value="nodes" selected={s.filter_mode == PersistedFilter::Nodes}>{"Nodes"}</option>
                        <option value="contracts" selected={s.filter_mode == PersistedFilter::Contracts}>{"Contracts"}</option>
                    </select>
                </div>
                </section>

                <section class="settings-group">
                <h3>{"🧲 Layout physics"}</h3>
                <p class="hint">{"Force-directed simulation parameters. Changes apply live."}</p>
                {
                    layout_field("repulsion (K_REPEL)", 50.0, 4000.0, 50.0, s.layout.k_repel,
                        std::rc::Rc::new(|l: &mut LayoutSettings, v| l.k_repel = v))
                }
                {
                    layout_field("edge spring (K_EDGE)", 0.0, 0.05, 0.001, s.layout.k_edge,
                        std::rc::Rc::new(|l: &mut LayoutSettings, v| l.k_edge = v))
                }
                {
                    layout_field("edge rest length (px)", 30.0, 300.0, 5.0, s.layout.edge_rest_length,
                        std::rc::Rc::new(|l: &mut LayoutSettings, v| l.edge_rest_length = v))
                }
                {
                    layout_field("centre gravity (K_GRAVITY)", 0.0, 0.05, 0.0005, s.layout.k_gravity,
                        std::rc::Rc::new(|l: &mut LayoutSettings, v| l.k_gravity = v))
                }
                {
                    layout_field("damping", 0.5, 0.99, 0.01, s.layout.damping,
                        std::rc::Rc::new(|l: &mut LayoutSettings, v| l.damping = v))
                }
                <details class="advanced-details">
                    <summary>{"Advanced"}</summary>
                    {
                        layout_field("max speed (px/tick)", 2.0, 80.0, 1.0, s.layout.max_speed,
                            std::rc::Rc::new(|l: &mut LayoutSettings, v| l.max_speed = v))
                    }
                    {
                        layout_field("repulsion min dist (px)", 2.0, 60.0, 1.0, s.layout.repel_min_dist,
                            std::rc::Rc::new(|l: &mut LayoutSettings, v| l.repel_min_dist = v))
                    }
                    {
                        layout_field("soft clamp radius (px)", 200.0, 600.0, 5.0, s.layout.soft_clamp_radius,
                            std::rc::Rc::new(|l: &mut LayoutSettings, v| l.soft_clamp_radius = v))
                    }
                </details>
                </section>

                <section class="settings-group">
                <h3>{"🔗 Network sharing"}</h3>
                <p class="hint">
                    {"Subscribe to a Freenet topology contract on your local node. \
                    The dashboard receives every signed "}<code>{"EntryPayload"}</code>
                    {" any other publisher pushes into that contract; entries \
                    are verified against their embedded Ed25519 key before \
                    merging. The more dashboards in the network turn "}
                    <code>{"publish enabled"}</code>{" on, the richer the graph."}
                </p>
                <div class="setting-row">
                    <label>{"enabled"}</label>
                    <input type="checkbox" checked={s.contract.enabled} oninput={
                        let mutate = mutate.clone();
                        Callback::from(move |e: InputEvent| {
                            let v = e.target_unchecked_into::<web_sys::HtmlInputElement>().checked();
                            mutate(&|s| s.contract.enabled = v);
                        })
                    } />
                    <span class={classes!("contract-status", contract_status_class(&props.contract_status))}>
                        { contract_status_label(&props.contract_status) }
                    </span>
                </div>
                <div class="setting-row">
                    <label>{"node WS URL"}</label>
                    <input type="text" placeholder="ws://localhost:7509"
                        value={s.contract.node_ws_url.clone()}
                        oninput={
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlInputElement>().value();
                                mutate(&|s| s.contract.node_ws_url = v.clone());
                            })
                        }
                    />
                    <span></span>
                </div>
                <div class="setting-row">
                    <label>{"contract instance id"}</label>
                    <input type="text" placeholder="base58 ContractInstanceId"
                        value={s.contract.instance_id.clone()}
                        oninput={
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlInputElement>().value();
                                mutate(&|s| s.contract.instance_id = v.clone());
                            })
                        }
                    />
                    <span></span>
                </div>
                <p class="hint">
                    { format!("{} publisher(s) seen in this session.", props.remote_entry_count) }
                    {" Each remote entry is verified against its embedded \
                    Ed25519 public key before merging — bad signatures are dropped."}
                </p>

                <h4>{"Publish your view"}</h4>
                <p class="hint">
                    {"Contribute one signed entry every "}
                    { s.contract.publish_interval_secs }
                    {"s. The entry carries:"}
                </p>
                <ul class="hint">
                    <li><code>{"public_key"}</code>{" — derived from "}
                        <code>{"identity (pubkey)"}</code>{" below; stable per browser."}</li>
                    <li><code>{"neighbors"}</code>{" — your "}
                        <code>{"Known public nodes"}</code>
                        {" list. Add rows there to widen what you publish."}</li>
                    <li><code>{"timestamp_ms"}</code>{" — wall clock; the contract \
                        keeps the most recent entry per "}
                        <code>{"public_key"}</code>{"."}</li>
                </ul>
                <p class="hint">
                    {"Auto-discovery of own peers / location / version is "}
                    <em>{"not"}</em>{" possible from a sandbox iframe in the \
                    current freenet-core (CORS + NodeQueries gates). For now, \
                    enrich your entry by hand via "}
                    <code>{"+ add known node"}</code>{" above. To grow the global \
                    graph, get other operators to open this dashboard on their \
                    own nodes and turn "}<code>{"publish enabled"}</code>{" on."}
                </p>
                <div class="setting-row">
                    <label>{"publish enabled"}</label>
                    <input type="checkbox" checked={s.contract.publish_enabled} oninput={
                        let mutate = mutate.clone();
                        Callback::from(move |e: InputEvent| {
                            let v = e.target_unchecked_into::<web_sys::HtmlInputElement>().checked();
                            mutate(&|s| s.contract.publish_enabled = v);
                        })
                    } />
                    <span></span>
                </div>
                <div class="setting-row">
                    <label>{"publish interval (s)"}</label>
                    <input type="number" min="5" max="3600" step="5"
                        value={s.contract.publish_interval_secs.to_string()}
                        oninput={
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let target: web_sys::HtmlInputElement = e.target_unchecked_into();
                                if let Ok(v) = target.value().parse::<u32>() {
                                    mutate(&|s| s.contract.publish_interval_secs = v);
                                }
                            })
                        }
                    />
                    <span></span>
                </div>
                <div class="setting-row">
                    <label>{"contract code hash"}</label>
                    <input type="text" placeholder="base58 CodeHash (publish-only)"
                        value={s.contract.code_hash.clone()}
                        oninput={
                            let mutate = mutate.clone();
                            Callback::from(move |e: InputEvent| {
                                let v: String = e.target_unchecked_into::<web_sys::HtmlInputElement>().value();
                                mutate(&|s| s.contract.code_hash = v.clone());
                            })
                        }
                    />
                    <span></span>
                </div>
                { identity_row }
                </section>

                <button class="reset-btn" onclick={on_reset}>{"reset all to defaults"}</button>
            </div>
        </div>
    }
}

fn contract_status_label(s: &ContractStatus) -> String {
    match s {
        ContractStatus::Disabled => "disabled".to_string(),
        ContractStatus::Connecting => "connecting…".to_string(),
        ContractStatus::Subscribing => "subscribing…".to_string(),
        ContractStatus::Subscribed => "subscribed".to_string(),
        ContractStatus::Error(msg) => format!("error: {msg}"),
    }
}

fn contract_status_class(s: &ContractStatus) -> &'static str {
    match s {
        ContractStatus::Disabled => "status-disabled",
        ContractStatus::Connecting | ContractStatus::Subscribing => "status-connecting",
        ContractStatus::Subscribed => "status-ok",
        ContractStatus::Error(_) => "status-err",
    }
}

// ============================ publisher worker ============================

/// One full publish cycle.
///
/// Sandbox iframe limitations rule out the two paths a publisher would
/// normally take to learn about its local node:
///   - `fetch('/')` is CORS-blocked (iframe origin is "null").
///   - `ClientRequest::NodeQueries` is rejected server-side for
///     web-app clients (`client_events/websocket.rs:1386`).
///
/// We can't change `freenet-core`. So this publisher emits a *skeleton*
/// `EntryPayload` carrying only the publisher's identity and the
/// statically-configured `known_nodes` from settings. The contract
/// state grows (a per-publisher slot exists), and any subscriber sees
/// "publisher P is alive at timestamp T", but the rich peer/contract
/// graph data has to come from elsewhere — currently the user's
/// "Known public nodes" list. Future work: re-introduce auto-discovery
/// when freenet-core exposes a webapp-safe peer-list endpoint.
async fn publish_one_cycle(
    holder: &Rc<RefCell<Option<ContractClient>>>,
    sk: &SigningKey,
    instance_id: ContractInstanceId,
    code_hash: CodeHash,
    known_nodes: Vec<KnownNode>,
) -> Result<(), String> {
    let neighbors = known_nodes
        .into_iter()
        .map(|n| NeighborInfo {
            address: n.address,
            location: n.location,
            is_gateway: n.is_gateway,
        })
        .collect::<Vec<_>>();
    let payload = EntryPayload {
        public_key: sk.verifying_key().to_bytes(),
        external_address: String::new(),
        own_location: None,
        version: None,
        neighbors,
        contracts: vec![],
        timestamp_ms: now_ms(),
    };
    web_sys::console::log_1(
        &format!(
            "[net-graph publish] skeleton payload: neighbors={}",
            payload.neighbors.len()
        )
        .into(),
    );

    let guard = holder.borrow();
    let client = guard
        .as_ref()
        .ok_or_else(|| "subscription is not open; enable it first".to_string())?;
    web_sys::console::log_1(&"[net-graph publish] sending Update via client.publish()".into());
    let r = client
        .publish(&payload, sk, instance_id, code_hash)
        .await;
    web_sys::console::log_1(&format!("[net-graph publish] result: {r:?}").into());
    r
}

fn now_ms() -> u64 {
    web_sys::js_sys::Date::now() as u64
}

fn decode_signing_key(seed_hex: &str) -> Option<SigningKey> {
    let bytes = hex::decode(seed_hex.trim()).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    Some(SigningKey::from_bytes(&seed))
}

fn decode_code_hash(s: &str) -> Result<CodeHash, String> {
    let bytes = bs58::decode(s.trim())
        .into_vec()
        .map_err(|e| format!("base58: {e}"))?;
    CodeHash::try_from(bytes.as_slice()).map_err(|e| format!("length: {e}"))
}

fn main() {
    // `Config::default()` sets `Level::Trace`, which makes html5ever
    // (used by scraper-lib) flood the browser console with thousands
    // of `tree_builder/mod.rs` debug lines on every dashboard parse.
    // We don't ship a logging UI, and the relevant signal lives in the
    // explicit `[net-graph]` console.log_1 calls, so cap at INFO.
    wasm_logger::init(wasm_logger::Config::new(log::Level::Info));
    set_outer_shell_title("Freenet Net-Graph");
    yew::Renderer::<App>::new().render();
}

/// Ask the freenet outer shell to set its own browser-tab title.
///
/// The dashboard runs inside a sandboxed iframe at the opaque `null`
/// origin, so `parent.document.title = …` is cross-origin-blocked and
/// without help the browser tab reads the outer shell's hardcoded
/// `<title>Freenet</title>`. The shell registers a `__freenet_shell__:
/// type:'title'` postMessage handler (path_handlers.rs:661) that
/// truncates to 128 chars and writes to its own document.title — a
/// channel that exists specifically for this case. We send once at
/// startup; the shell keeps the title until the page reloads.
fn set_outer_shell_title(title: &str) {
    let Some(window) = web_sys::window() else { return };
    let parent = match window.parent() {
        Ok(Some(p)) if !p.is_undefined() => p,
        _ => return,
    };
    if web_sys::js_sys::Object::is(&parent, &window) {
        // Top-level (no outer shell) — set our own document title and
        // we're done.
        if let Some(doc) = window.document() {
            doc.set_title(title);
        }
        return;
    }
    let payload = web_sys::js_sys::Object::new();
    let _ = web_sys::js_sys::Reflect::set(
        &payload,
        &"__freenet_shell__".into(),
        &wasm_bindgen::JsValue::TRUE,
    );
    let _ = web_sys::js_sys::Reflect::set(&payload, &"type".into(), &"title".into());
    let _ = web_sys::js_sys::Reflect::set(&payload, &"title".into(), &title.into());
    let _ = parent.post_message(&payload, "*");
}
