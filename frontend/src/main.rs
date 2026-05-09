use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use gloo_timers::callback::Timeout;
use shared::contract::{decode_contract_entry, DecodedContractEntry};
use shared::{ContractMeta, ContractView, FetchStatus, GatewayView, KnownNode, PeerView, Topology};
use wasm_bindgen_futures::JsFuture;
use yew::prelude::*;

mod contract_client;
mod graph;
mod settings;
mod ws_shim;

use contract_client::{ContractClient, ContractStatus, RemoteEntry};
use settings::{LayoutSettings, PersistedFilter, Settings};

/// User-controllable filter facets applied on top of the text query.
/// Each variant defaults to "any" so the filter is opt-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContractFilter {
    /// Contract kind filter — webapp / data-only / unprobed / any.
    kind: ContractKindFilter,
    /// Solo (1 instance) vs multi-instance (>1) vs any.
    instance: InstanceFilter,
    /// Minimum number of distinct publishers reporting this contract;
    /// 1 = no constraint. Useful to surface "widely-replicated" rows.
    min_subscribers: u32,
}

impl Default for ContractFilter {
    fn default() -> Self {
        Self {
            kind: ContractKindFilter::Any,
            instance: InstanceFilter::Any,
            min_subscribers: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContractKindFilter {
    Any,
    /// `has_web_interface == Some(true)`.
    Web,
    /// `has_web_interface == Some(false)`.
    Data,
    /// `has_web_interface == None` (probe pending or unsupported).
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceFilter {
    Any,
    /// `instance_keys.len() > 1` — same code/title across many ids.
    Multi,
    /// `instance_keys.len() == 1`.
    Solo,
}

/// User-selectable sort axis for the Contracts list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ContractSort {
    /// Default: group by kind (webapp/unknown/data) then alpha-by-title.
    #[default]
    KindThenName,
    /// Most subscribers first (most-replicated apps surface to the top).
    SubscribersDesc,
    /// Most instances first (templates with the largest fan-out).
    InstancesDesc,
    /// Most recently published first.
    LastUpdateDesc,
    /// Alpha by title/key.
    NameAsc,
}

/// One sample for the header sparkline. Cheap to clone (16 bytes).
#[derive(Clone, Copy, Debug, PartialEq)]
struct HistorySample {
    ts: u64,
    publishers: u32,
    edges: u32,
    nodes: u32,
}

/// Capacity of the rolling sparkline buffer. ~60 samples ≈ 1 hour at the
/// daemon's default 60 s publish cadence — long enough to spot a slow
/// drift, short enough that the SVG stays readable at 80 px wide.
const HISTORY_CAP: usize = 60;

/// Convert a `ws://host:port` (or `wss://`) URL to the equivalent
/// `http://host:port` base. Returns `None` for non-ws schemes — the
/// caller should treat that as "can't open contract URL from here".
fn ws_to_http_base(ws: &str) -> Option<String> {
    let trimmed = ws.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        Some(format!("https://{rest}"))
    } else if let Some(rest) = trimmed.strip_prefix("ws://") {
        Some(format!("http://{rest}"))
    } else {
        None
    }
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
    // Contracts-tab filter facets (kind, instance count, min subs).
    let contract_filter: UseStateHandle<ContractFilter> = use_state(ContractFilter::default);
    // Contracts-tab sort axis.
    let contract_sort: UseStateHandle<ContractSort> = use_state(ContractSort::default);
    // Active "hosted by" filter — pin the list to one publisher's
    // contracts. Stored as the publisher's gateway label (matches
    // `seen_by` strings produced by `flat_contracts`). `None` = no
    // filter active.
    let publisher_filter: UseStateHandle<Option<String>> = use_state(|| None);
    // Per-group collapse state, keyed by group label ("webapps", "data",
    // "unknown"). Default = expanded (false) for everything.
    let collapsed_groups: UseStateHandle<HashSet<String>> = use_state(HashSet::new);
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

    // ---- topology built fresh from the subscription each render -----
    // No polling. The graph reflects whatever entries we've verified
    // through the contract so far, plus the user's `known_nodes` list.
    // The dashboard is read-only — only the operator-side daemons
    // ([topology-publisher]) write into the contract; visitors here
    // never sign or publish anything.
    let topo = Rc::new(build_topology(&remote_entries, &settings.known_nodes));

    // Rolling history of (timestamp, publisher_count, peer_edge_count)
    // for the header sparkline. Bounded to HISTORY_CAP so the buffer
    // stays cheap. Only sampled when `topo.fetched_at` advances —
    // empty topologies don't pollute the line.
    let history: UseStateHandle<Vec<HistorySample>> = use_state(Vec::new);
    // Selection-driven highlight set: which graph node ids should stay
    // at full opacity. None = no selection / no match → default render.
    let highlight_set: Rc<Option<HashSet<String>>> = Rc::new(compute_highlight_set(
        &topo,
        selected.as_deref(),
    ));
    // Error banner now shows the contract-subscription failure state
    // (config error, WS closed, decode failure) — there's no second
    // "fetch" channel any more.
    let err = match &*contract_status {
        ContractStatus::Error(msg) => Some(msg.clone()),
        _ => None,
    };

    let (header_meta, publisher_count, total_peer_edges, unique_node_count) = {
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
        (meta, publishers, total_peers, nodes.len())
    };

    // Sample the rolling history once per `fetched_at` advance. Pushing
    // every render would explode the buffer; gating on the deduplicated
    // (ts, publishers, edges, nodes) tuple ensures we only keep frames
    // that actually changed. `use_effect_with` debounces dependency
    // changes for us.
    {
        let history = history.clone();
        let fetched_at = topo.fetched_at;
        let publishers = publisher_count as u32;
        let edges = total_peer_edges as u32;
        let nodes = unique_node_count as u32;
        use_effect_with(
            (fetched_at, publishers, edges, nodes),
            move |&(ts, p, e, n)| {
                // Skip the empty-topology start-of-day so the line doesn't
                // anchor at zero forever.
                if ts == 0 {
                    return;
                }
                let mut next = (*history).clone();
                if next.last().map(|s| s.ts == ts).unwrap_or(false) {
                    return;
                }
                next.push(HistorySample {
                    ts,
                    publishers: p,
                    edges: e,
                    nodes: n,
                });
                if next.len() > HISTORY_CAP {
                    let drop = next.len() - HISTORY_CAP;
                    next.drain(..drop);
                }
                history.set(next);
            },
        );
    }

    // HTTP base URL used to construct webapp open-links. The dashboard
    // runs inside the sandboxed iframe at the opaque "null" origin, so
    // relative URLs would resolve to `null` — we derive the outer
    // freenet node's HTTP base from the configured WS URL instead.
    // `Rc<Option<String>>` so the value is cheap to clone into every
    // contract-row render (one alloc per topology change, not per row).
    let http_base: Rc<Option<String>> = Rc::new(ws_to_http_base(&settings.contract.node_ws_url));

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

    let on_contract_filter_change = {
        let contract_filter = contract_filter.clone();
        Callback::from(move |f: ContractFilter| contract_filter.set(f))
    };
    let on_contract_sort_change = {
        let contract_sort = contract_sort.clone();
        Callback::from(move |s: ContractSort| contract_sort.set(s))
    };
    let on_publisher_filter_clear = {
        let publisher_filter = publisher_filter.clone();
        Callback::from(move |_: MouseEvent| publisher_filter.set(None))
    };
    // Setter form: drilldown panel calls this to "Show only this
    // publisher's contracts". Also flips the active tab to Contracts
    // so the user immediately sees the effect.
    let on_publisher_filter_set = {
        let publisher_filter = publisher_filter.clone();
        let settings = settings.clone();
        Callback::from(move |label: String| {
            publisher_filter.set(Some(label));
            let mut next = (*settings).clone();
            if next.filter_mode != PersistedFilter::Contracts {
                next.filter_mode = PersistedFilter::Contracts;
                settings::save_to_storage(&next);
                settings.set(next);
            }
        })
    };
    let on_group_toggle = {
        let collapsed_groups = collapsed_groups.clone();
        Callback::from(move |group_id: String| {
            let mut next = (*collapsed_groups).clone();
            if !next.remove(&group_id) {
                next.insert(group_id);
            }
            collapsed_groups.set(next);
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
                { render_header_sparklines(&history) }
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
                            http_base.clone(),
                            *contract_filter,
                            on_contract_filter_change.clone(),
                            *contract_sort,
                            on_contract_sort_change.clone(),
                            (*publisher_filter).clone(),
                            on_publisher_filter_clear.clone(),
                            (*collapsed_groups).clone(),
                            on_group_toggle.clone(),
                        )
                    }
                </aside>
                <div class="resizer"
                     onmousedown={on_resize_start}
                     title="Drag to resize sidebar"></div>
                <div class="graph-wrap">
                    <graph::Graph
                        topology={topo.clone()}
                        selected={(*selected).clone()}
                        highlight_set={highlight_set.clone()}
                        layout={layout}
                    />
                    {
                        match selected.as_deref() {
                            Some(sel) => render_publisher_drilldown(
                                &remote_entries,
                                sel,
                                on_copy.clone(),
                                last_copied_value.as_deref(),
                                http_base.clone(),
                                on_publisher_filter_set.clone(),
                            ),
                            None => html!{},
                        }
                    }
                    {
                        if publisher_count == 0 {
                            // No verified entries from the contract yet —
                            // the dashboard is read-only; show a hint
                            // pointing at the daemon side, which is the
                            // only thing that fills the graph.
                            html! {
                                <div class="empty-hint">
                                    <h3>{"Graph is empty — no daemons publishing"}</h3>
                                    <p>{"This dashboard subscribes to the topology contract \
                                    but doesn't publish anything itself (sandbox + \
                                    NodeQueries gates make it impossible). Operators \
                                    contribute by running the "}
                                    <code>{"topology-publisher"}</code>
                                    {" daemon alongside their freenet node — see the "}
                                    <a href="https://github.com/Basedfloppa/freenet-net-graph/blob/main/topology-publisher/README.md"
                                       target="_blank" rel="noopener noreferrer">{"README"}</a>
                                    {" for a one-page setup guide."}</p>
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
                <a
                    class="repo-link"
                    href="https://github.com/Basedfloppa/freenet-net-graph"
                    target="_blank"
                    rel="noopener noreferrer"
                    title="Source on GitHub: github.com/Basedfloppa/freenet-net-graph"
                >{ "GitHub ↗" }</a>
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
/// via `shared::contract::encode_contract_entry`. Bare keys (no probe
/// data) decode as `(key, None, None)` and leave the meta unset —
/// those contracts render without a badge until some daemon publisher
/// classifies them.
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

        // Operator-chosen display name from `--display-name` is shipped
        // in `EntryPayload.version`. When set, prefer it as the gateway
        // label so users see "baka" instead of "remote: c5b03be5".
        // Falls back to the pubkey prefix when no operator name is set.
        // See `topology-publisher/--display-name` for the source field.
        let display_name = p.version.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let label = match display_name {
            Some(name) => format!("{name} ({pubkey_prefix})"),
            None => format!("remote: {pubkey_prefix}"),
        };

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
                let DecodedContractEntry { key, is_webapp, title, code_hash } =
                    decode_contract_entry(raw);
                if is_webapp.is_some() || title.is_some() || code_hash.is_some() {
                    let slot = contract_meta.entry(key.clone()).or_insert(ContractMeta {
                        has_web_interface: false,
                        title: None,
                        probed_at: p.timestamp_ms / 1000,
                        code_hash: None,
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
                    if slot.code_hash.is_none() {
                        if let Some(c) = code_hash {
                            slot.code_hash = Some(c);
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
            label,
            url: format!("(contract • {})", entry.publisher_pubkey_hex),
            status: FetchStatus::Ok,
            own_location: p.own_location,
            external_address: if p.external_address.is_empty() {
                None
            } else {
                Some(p.external_address.clone())
            },
            // `version` field is currently overloaded as the operator's
            // display_name carrier (no real freenet-core version exposed
            // by `NodeDiagnostics` yet). Pass `None` so the row doesn't
            // render a misleading "vbaka" tag.
            version: None,
            peers,
            contracts,
            last_seen_ms: Some(p.timestamp_ms),
        });
    }

    Topology {
        gateways,
        known_nodes: known_nodes.to_vec(),
        contract_meta,
        fetched_at: newest_ts_ms / 1000,
    }
}

// ============================ selection-context highlight ============================

/// Compute the "focus set" — graph node ids the graph should keep at
/// full opacity while everything else dims out — based on the
/// currently-selected entity. Two cases:
///
/// 1. **Selection is a contract key** (clicked in the Contracts list).
///    Returns the set of publisher gateway-ids that report hosting it.
///    Reverse-lookup: the user picks a contract row, the graph reveals
///    "who has this".
///
/// 2. **Selection is a graph node id** (clicked in the Nodes list, or
///    on the graph). Returns the node itself plus every neighbour
///    connected via at least one edge — i.e. its 1-hop ring.
///
/// `None` means no highlight is active and the graph renders normally.
fn compute_highlight_set(t: &Topology, selected: Option<&str>) -> Option<HashSet<String>> {
    let sel = selected?;

    // Case 1: contract key. Match the canonical (decoded) key only;
    // grouped instances roll up to one canonical, so a single match
    // surfaces the right publisher set even when the row collapsed
    // many same-title instances.
    let mut hosting: HashSet<String> = HashSet::new();
    for gw in &t.gateways {
        if gw.contracts.iter().any(|c| c.key == sel) {
            let gw_id = gw
                .external_address
                .clone()
                .unwrap_or_else(|| format!("gw::{}", gw.label));
            hosting.insert(gw_id);
        }
    }
    if !hosting.is_empty() {
        return Some(hosting);
    }

    // Case 2: node id. Walk every gateway's peer list once; if `sel`
    // names this gateway, all its peers join the set, and if `sel` is
    // a peer of any gateway, that gateway joins the set. Either way
    // the selected node itself is always included.
    let mut set: HashSet<String> = HashSet::new();
    set.insert(sel.to_string());
    let mut matched = false;
    for gw in &t.gateways {
        let gw_id = gw
            .external_address
            .clone()
            .unwrap_or_else(|| format!("gw::{}", gw.label));
        if gw_id == sel {
            matched = true;
            for p in &gw.peers {
                set.insert(p.address.clone());
            }
        }
        for p in &gw.peers {
            if p.address == sel {
                matched = true;
                set.insert(gw_id.clone());
            }
        }
    }
    if matched {
        Some(set)
    } else {
        None
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
    /// Last time the gateway behind this entry was heard from (wall-clock
    /// ms since epoch). Used by the graph to fade publishers that
    /// haven't reposted recently. `None` for non-gateway peers (we only
    /// see them through someone else's report, never directly).
    last_seen_ms: Option<u64>,
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
                // Keep the freshest reporting timestamp — multiple
                // publishers may all claim the same gateway, but we
                // care about the most recent one for staleness.
                if let Some(ts) = n.last_seen_ms {
                    existing.last_seen_ms = Some(
                        existing.last_seen_ms.map_or(ts, |cur| cur.max(ts)),
                    );
                }
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
            last_seen_ms: gw.last_seen_ms,
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
                last_seen_ms: None,
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
            last_seen_ms: None,
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
    /// Distinct page titles observed across this group. Webapps that
    /// store their display name in state (e.g. each "Notes" instance
    /// titled differently) report different `<title>`s for the same
    /// underlying app — code_hash collapses those into one row, and
    /// this list captures the variation for tooltips / search.
    title_variants: Vec<String>,
    /// All distinct WASM code hashes observed across the instances in
    /// this row. Sorted + deduped. Empty when no publisher shipped a
    /// code hash for any instance. `len() > 1` means the row collapses
    /// multiple *versions* of the same-named app (e.g. "River v0.5"
    /// and "River v0.6" with different WASM but identical title) —
    /// surfaced as a "(N versions)" badge.
    code_hashes: Vec<String>,
    /// Every distinct contract instance id collapsed into this row.
    /// Always includes `key`. `len() > 1` means several instances share
    /// the same code (or, fallback, the same `<title>`) — the row shows
    /// them as one with an "{N} instances" badge and lets search match
    /// any of them.
    instance_keys: Vec<String>,
    /// Wall-clock ms when any publisher last reported one of this
    /// row's instances. Drives the "last update" sort and powers the
    /// recency tag in the row UI.
    last_seen_ms: Option<u64>,
}

fn flat_contracts(t: &Topology) -> Vec<FlatContract> {
    // First pass: dedup by raw contract key — the same instance id
    // seen via multiple publishers is one contract row to start with.
    // We bind `last_seen_ms` from each publishing gateway here so the
    // grouping pass can pick the freshest timestamp across instances.
    let mut by_key: HashMap<String, FlatContract> = HashMap::new();
    for gw in &t.gateways {
        for c in &gw.contracts {
            let meta = t.contract_meta.get(&c.key);
            let entry = by_key.entry(c.key.clone()).or_insert_with(|| {
                let initial_hashes = meta
                    .and_then(|m| m.code_hash.clone())
                    .map(|h| vec![h])
                    .unwrap_or_default();
                FlatContract {
                    key: c.key.clone(),
                    short: short_key(&c.key),
                    seen_by: Vec::new(),
                    subscribed_ago: None,
                    last_update_ago: None,
                    has_web_interface: meta.map(|m| m.has_web_interface),
                    title: meta.and_then(|m| m.title.clone()),
                    title_variants: meta
                        .and_then(|m| m.title.clone())
                        .map(|t| vec![t])
                        .unwrap_or_default(),
                    code_hashes: initial_hashes,
                    instance_keys: vec![c.key.clone()],
                    last_seen_ms: gw.last_seen_ms,
                }
            });
            if !entry.seen_by.contains(&gw.label) {
                entry.seen_by.push(gw.label.clone());
            }
            // Adopt the freshest publisher timestamp across all gateways
            // that reported this contract — that's "last update for any
            // instance in this group".
            if let Some(ts) = gw.last_seen_ms {
                entry.last_seen_ms = Some(entry.last_seen_ms.map_or(ts, |cur| cur.max(ts)));
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

    // Second pass: collapse rows that are the same *app from the
    // user's perspective*. Grouping key priority:
    //   1. lowercased webapp `<title>` — title is what humans see and
    //      compare. "River v0.5" and "River v0.6" both render as
    //      "River" in the UI, so they're one app even though their
    //      WASM hashes differ. Gated on `has_web_interface == Some(true)`
    //      so unprobed-or-data entries don't all collapse into one.
    //   2. `code_hash` — fallback for webapps with empty/no title and
    //      for unprobed entries. Same WASM still guarantees same app.
    //   3. raw key — singletons that have neither signal.
    //
    // Multiple code-hash *versions* of the same-titled app fold into
    // one row; the resulting `code_hashes` list (deduped) drives the
    // "(N versions)" badge in the UI so the version split stays
    // visible. The earlier rule (code_hash first) stranded same-name
    // apps into separate rows whenever the operator redeployed with
    // a fresh WASM — which surfaces every time anyone publishes a new
    // version of a popular contract like "River" or "Freenet File".
    let mut grouped: Vec<FlatContract> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut singletons: Vec<FlatContract> = Vec::new();

    for entry in by_key.into_values() {
        let group_key: Option<String> = if let (Some(true), Some(t)) =
            (entry.has_web_interface, entry.title.as_deref())
        {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!("t:{}", trimmed.to_lowercase()))
            }
        } else if let Some(code) = entry.code_hashes.first() {
            Some(format!("c:{code}"))
        } else {
            None
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
    // Pin each row's `key` to its *minimum* instance id for
    // determinism. Without this, the canonical key was whichever
    // instance the upstream `HashMap` iteration happened to land on
    // first — different per render, so `key`-based tiebreakers below
    // (and copy-button payloads, and selection round-trips) drifted.
    // Same idea for `code_hashes`: stable sort makes the version
    // badge tooltip render the hashes in the same order every time.
    for row in out.iter_mut() {
        row.instance_keys.sort();
        if let Some(first) = row.instance_keys.first() {
            row.key = first.clone();
            row.short = short_key(first);
        }
        row.code_hashes.sort();
    }
    // Default sort: webapps → unprobed → data-only, alpha within bucket,
    // **with `key` as the deterministic tiebreaker**. Without that final
    // key compare, items sharing the same lowercase label (e.g. two
    // webapps with identical `<title>` but different code hashes)
    // resolved their order from `HashMap` iteration — which is
    // randomised per-render, so every facet click reshuffled the list.
    // The UI exposes a `ContractSort` selector that overrides this —
    // see [`render_search_panel`].
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
            .then_with(|| a.key.cmp(&b.key))
    });
    out
}

/// Fold one contract into an existing group row.
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
    // Merge title variants from the incoming row; surfaces "this app
    // has 6 instances with these distinct names" in the row tooltip.
    for t in src.title_variants {
        if !t.trim().is_empty() && !dst.title_variants.contains(&t) {
            dst.title_variants.push(t);
        }
    }
    if dst.title.is_none() {
        dst.title = src.title;
    }
    if dst.has_web_interface != Some(true) {
        dst.has_web_interface = src.has_web_interface;
    }
    // Accumulate every distinct WASM code hash. Multiple hashes in
    // one group means the same-named app got redeployed with new
    // bytes — the "(N versions)" badge surfaces that.
    for h in src.code_hashes {
        if !dst.code_hashes.contains(&h) {
            dst.code_hashes.push(h);
        }
    }
    if let Some(ts) = src.last_seen_ms {
        dst.last_seen_ms = Some(dst.last_seen_ms.map_or(ts, |cur| cur.max(ts)));
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

// ============================ ring-location histogram ============================

/// Bucket count for the location histogram strip. 36 ≈ one bucket per
/// 10° of the ring — fine-grained enough to show clusters, coarse
/// enough to absorb single-publisher noise without looking spiky.
const HIST_BUCKETS: usize = 36;
/// Strip dimensions in CSS pixels; matches `.ring-histogram` width
/// constraints and the height set by the bar SVG. Kept as constants
/// so visual tweaks live in one place.
const HIST_WIDTH: f64 = 280.0;
const HIST_HEIGHT: f64 = 28.0;

/// Render a stacked-bar strip showing how publisher `own_location`
/// values distribute across the ring `[0, 1)`. Helps spot clustering
/// (which often correlates with poor routing) at a glance.
///
/// Source: every gateway in `t.gateways` whose `own_location` is set.
/// Transitive peer locations are *not* counted — they're inferred,
/// often stale, and would dilute the signal.
fn render_ring_histogram(t: &Topology) -> Html {
    let mut buckets = [0u32; HIST_BUCKETS];
    let mut total = 0u32;
    for gw in &t.gateways {
        if let Some(loc) = gw.own_location {
            let idx = ((loc.clamp(0.0, 1.0 - f64::EPSILON)) * HIST_BUCKETS as f64) as usize;
            buckets[idx] = buckets[idx].saturating_add(1);
            total = total.saturating_add(1);
        }
    }
    if total == 0 {
        return html! {};
    }
    let max = *buckets.iter().max().unwrap_or(&1).max(&1) as f64;
    let bar_w = HIST_WIDTH / HIST_BUCKETS as f64;
    let bars: Vec<Html> = buckets
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            let x = i as f64 * bar_w;
            let h = (count as f64 / max) * HIST_HEIGHT;
            let y = HIST_HEIGHT - h;
            // Hue mirrors the node-fill colour scheme so the bar
            // colour matches the location it represents — visual
            // cross-reference between graph and histogram.
            let loc_mid = (i as f64 + 0.5) / HIST_BUCKETS as f64;
            let hue = (loc_mid * 360.0).round() as u32;
            let style = format!("fill: hsl({hue}, 65%, 55%);");
            html! {
                <rect class="hist-bar"
                      x={x.to_string()} y={y.to_string()}
                      width={(bar_w - 0.5).to_string()}
                      height={h.to_string()}
                      style={style}>
                    <title>{ format!("loc {:.2}–{:.2}: {count} publisher(s)",
                        i as f64 / HIST_BUCKETS as f64,
                        (i + 1) as f64 / HIST_BUCKETS as f64) }</title>
                </rect>
            }
        })
        .collect();
    html! {
        <div class="ring-histogram" title="Publisher distribution across the ring (own_location)">
            <svg viewBox={format!("0 0 {HIST_WIDTH} {HIST_HEIGHT}")}
                 preserveAspectRatio="none">
                { for bars }
            </svg>
            <div class="ring-histogram-axis">
                <span>{"0"}</span><span>{"0.5"}</span><span>{"1"}</span>
            </div>
        </div>
    }
}

// ============================ header sparklines ============================

/// Render two compact SVG sparklines side by side: peer-edge count and
/// publisher count over the recent past (`HISTORY_CAP` samples). Empty
/// or single-sample history collapses to a flat line — never a layout-
/// shifting blank, since the header has fixed slot width.
fn render_header_sparklines(history: &[HistorySample]) -> Html {
    if history.is_empty() {
        return html! {
            <div class="header-sparklines" title="awaiting first publish">
                <div class="sparkline-empty">{"—"}</div>
            </div>
        };
    }

    let edges_max = history.iter().map(|s| s.edges).max().unwrap_or(1).max(1);
    let pubs_max = history.iter().map(|s| s.publishers).max().unwrap_or(1).max(1);

    let edges_path = sparkline_path(history, edges_max, |s| s.edges);
    let pubs_path = sparkline_path(history, pubs_max, |s| s.publishers);

    let last = history.last().copied().unwrap_or(HistorySample {
        ts: 0,
        publishers: 0,
        edges: 0,
        nodes: 0,
    });

    html! {
        <div class="header-sparklines"
             title={format!(
                "last {} sample(s) • now: {} edges, {} pubs",
                history.len(), last.edges, last.publishers
             )}>
            <div class="sparkline" title="peer-edge count over time">
                <svg viewBox="0 0 80 20" preserveAspectRatio="none">
                    <path d={edges_path} class="sparkline-edges" />
                </svg>
                <span class="sparkline-label">{ format!("{} edges", last.edges) }</span>
            </div>
            <div class="sparkline" title="publisher count over time">
                <svg viewBox="0 0 80 20" preserveAspectRatio="none">
                    <path d={pubs_path} class="sparkline-pubs" />
                </svg>
                <span class="sparkline-label">{ format!("{} pubs", last.publishers) }</span>
            </div>
        </div>
    }
}

/// Build a polyline `d=` attribute from `history` using `extract` to
/// pluck the value out of each sample. Y axis is inverted (SVG origin
/// is top-left) and scaled so the peak hits y=2 (small top margin).
fn sparkline_path(history: &[HistorySample], max: u32, extract: impl Fn(&HistorySample) -> u32) -> String {
    let n = history.len();
    if n == 0 {
        return String::new();
    }
    let max_f = max.max(1) as f64;
    let dx = if n > 1 { 80.0 / (n - 1) as f64 } else { 0.0 };
    let mut d = String::with_capacity(16 * n);
    for (i, s) in history.iter().enumerate() {
        let x = i as f64 * dx;
        // y in [2..18]; pad 2px top + bottom so the stroke isn't clipped.
        let y = 18.0 - (extract(s) as f64 / max_f) * 16.0;
        let cmd = if i == 0 { "M" } else { "L" };
        if i > 0 {
            d.push(' ');
        }
        d.push_str(&format!("{cmd}{x:.1},{y:.1}"));
    }
    d
}

// ============================ publisher drilldown ============================

/// If `selected` corresponds to a publisher we have a verified
/// `RemoteEntry` for, render an absolutely-positioned card on the graph
/// canvas with the publisher's full identity (pubkey, version, location,
/// peer/contract counts, last seen). Returns `html!{}` when the
/// selection is a non-publisher node (e.g. a transitive peer).
fn render_publisher_drilldown(
    remote: &HashMap<String, RemoteEntry>,
    selected: &str,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
    http_base: Rc<Option<String>>,
    on_publisher_filter_set: Callback<String>,
) -> Html {
    let Some(entry) = find_remote_entry_for_selection(remote, selected) else {
        return html! {};
    };
    let p = &entry.payload;
    let pubkey = entry.publisher_pubkey_hex.clone();
    let pubkey_short: String = pubkey.chars().take(16).collect();
    let now_ms = web_sys::js_sys::Date::now() as u64;
    let age_ms = now_ms.saturating_sub(p.timestamp_ms);
    let age = format_ago_ms(age_ms);
    let stale = age_ms > 5 * 60 * 1000;

    let location_str = p
        .own_location
        .map(|l| format!("{l:.4}"))
        .unwrap_or_else(|| "—".into());
    // `version` field carries the operator's `--display-name`. Render
    // it under the "name" key so the drilldown reflects what the field
    // actually holds today (real freenet-core version isn't exposed
    // through `NodeDiagnostics` yet).
    let display_name_str = p.version.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "—".into());
    let address_str = if p.external_address.is_empty() {
        "—".to_string()
    } else {
        p.external_address.clone()
    };

    let webapp_count = p
        .contracts
        .iter()
        .filter(|raw| shared::contract::decode_contract_entry(raw).is_webapp == Some(true))
        .count();

    // Reconstruct the publisher's gateway label exactly the way
    // `build_topology` writes it, so emitting it as a publisher-filter
    // value matches the `seen_by` strings stored on each FlatContract.
    let pubkey_prefix: String = entry.publisher_pubkey_hex.chars().take(8).collect();
    let pub_filter_label = match p.version.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => format!("{name} ({pubkey_prefix})"),
        None => format!("remote: {pubkey_prefix}"),
    };
    let on_filter_click = {
        let cb = on_publisher_filter_set.clone();
        let label = pub_filter_label.clone();
        Callback::from(move |_: MouseEvent| cb.emit(label.clone()))
    };

    // Top-3 contracts by title presence (webapps with names first), with
    // a "↗ open" link for each webapp. Covers the common case of "what
    // is this node hosting?" without bloating the panel for nodes that
    // host hundreds of contracts.
    let mut sorted_contracts: Vec<DecodedContractEntry> = p
        .contracts
        .iter()
        .map(|raw| shared::contract::decode_contract_entry(raw))
        .collect();
    sorted_contracts.sort_by(|a, b| {
        let bucket = |c: &DecodedContractEntry| match c.is_webapp {
            Some(true) => 0,
            None => 1,
            Some(false) => 2,
        };
        bucket(a)
            .cmp(&bucket(b))
            .then_with(|| a.title.as_deref().unwrap_or("").cmp(b.title.as_deref().unwrap_or("")))
    });
    let preview: Vec<Html> = sorted_contracts
        .iter()
        .take(8)
        .map(|d| {
            let key = &d.key;
            let label = d.title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    let short: String = key.chars().take(12).collect();
                    format!("{short}…")
                });
            let badge = match d.is_webapp {
                Some(true) => html! { <span class="web-badge web-yes">{"✓"}</span> },
                Some(false) => html! { <span class="web-badge web-no">{"d"}</span> },
                None => html! {},
            };
            // `<a target="_blank">` rather than a button: lets the
            // browser handle every native open-in-new-tab gesture
            // (left-click / middle-click / ctrl/cmd+click / right-click
            // → "Open in new tab") without us implementing each one in
            // JS. `stop_propagation` keeps a stray click off the link
            // itself from bubbling up to the row's selection handler.
            let open_btn = match (d.is_webapp, http_base.as_ref()) {
                (Some(true), Some(base)) => {
                    let url = format!("{base}/v1/contract/web/{key}/");
                    let stop = Callback::from(|e: MouseEvent| e.stop_propagation());
                    html! {
                        <a class="contract-open-btn"
                           href={url}
                           target="_blank"
                           rel="noopener noreferrer"
                           title="open webapp"
                           onclick={stop.clone()}
                           onauxclick={stop}>{"↗"}</a>
                    }
                }
                _ => html! {},
            };
            html! {
                <div class="drilldown-contract">
                    { badge }
                    <span class="drilldown-contract-label">{ label }</span>
                    { open_btn }
                </div>
            }
        })
        .collect();

    html! {
        <div class="publisher-drilldown">
            <div class="drilldown-head">
                <span class="drilldown-title">{"publisher"}</span>
                {
                    if stale {
                        html! { <span class="row-tag row-tag-stale" title="no fresh entry in >5 min">{"stale"}</span> }
                    } else { html! {} }
                }
                <span class="drilldown-age">{ age }</span>
            </div>
            <div class="drilldown-row">
                <span class="drilldown-key">{"pubkey"}</span>
                <span class="drilldown-val mono" title={pubkey.clone()}>{ format!("{pubkey_short}…") }</span>
                { copy_button(pubkey.clone(), on_copy.clone(), last_copied) }
            </div>
            <div class="drilldown-row">
                <span class="drilldown-key">{"address"}</span>
                <span class="drilldown-val mono">{ address_str }</span>
            </div>
            <div class="drilldown-row">
                <span class="drilldown-key">{"location"}</span>
                <span class="drilldown-val">{ location_str }</span>
                <span class="drilldown-key">{"name"}</span>
                <span class="drilldown-val">{ display_name_str }</span>
            </div>
            <div class="drilldown-row">
                <span class="drilldown-key">{"peers"}</span>
                <span class="drilldown-val">{ p.neighbors.len() }</span>
                <span class="drilldown-key">{"contracts"}</span>
                <span class="drilldown-val">
                    { p.contracts.len() }
                    {
                        if webapp_count > 0 {
                            html! { <span class="drilldown-sub">{ format!(" ({webapp_count} web)") }</span> }
                        } else { html! {} }
                    }
                </span>
            </div>
            {
                if preview.is_empty() {
                    html! {}
                } else {
                    html! {
                        <div class="drilldown-contracts">
                            <div class="drilldown-key">{"hosted (top 8)"}</div>
                            { for preview }
                        </div>
                    }
                }
            }
            // Bridge between graph selection and the Contracts list:
            // emits the gateway label that `flat_contracts` uses in
            // each row's `seen_by`, so the filter matches exactly.
            <button class="drilldown-action" onclick={on_filter_click}
                    title="filter the Contracts tab to only this publisher's hosted contracts">
                {"🔍 Filter contracts by this publisher"}
            </button>
        </div>
    }
}

/// Reverse-lookup: turn a graph node id back into the `RemoteEntry`
/// that synthesised it, mirroring the same id derivation
/// `build_topology` uses. Returns `None` when the selection is a
/// transitive peer (no direct entry) or a known-node anchor.
fn find_remote_entry_for_selection<'a>(
    remote: &'a HashMap<String, RemoteEntry>,
    selected: &str,
) -> Option<&'a RemoteEntry> {
    remote.values().find(|e| {
        let pubkey_prefix: String = e.publisher_pubkey_hex.chars().take(8).collect();
        let synth_label = format!("remote: {pubkey_prefix}");
        let gw_id = if e.payload.external_address.is_empty() {
            format!("gw::{synth_label}")
        } else {
            e.payload.external_address.clone()
        };
        gw_id == selected
    })
}

fn format_ago_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h{}m ago", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d ago", secs / 86400)
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
    http_base: Rc<Option<String>>,
    contract_filter: ContractFilter,
    on_contract_filter_change: Callback<ContractFilter>,
    contract_sort: ContractSort,
    on_contract_sort_change: Callback<ContractSort>,
    publisher_filter: Option<String>,
    on_publisher_filter_clear: Callback<MouseEvent>,
    collapsed_groups: HashSet<String>,
    on_group_toggle: Callback<String>,
) -> Html {
    let nodes = flat_nodes(t);
    let contracts = flat_contracts(t);

    let q = query.trim().to_lowercase();

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

    let histogram = render_ring_histogram(t);

    html! {
        <>
            <h2>{ header_text }</h2>
            <div class="filter-tabs">
                { tab("Nodes", PersistedFilter::Nodes) }
                { tab("Contracts", PersistedFilter::Contracts) }
            </div>
            { histogram }
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
            {
                match filter {
                    PersistedFilter::Nodes => render_nodes_list(
                        nodes, &q, selected, on_pick.clone(), on_copy.clone(), last_copied,
                    ),
                    PersistedFilter::Contracts => render_contracts_list(
                        contracts,
                        &q,
                        selected,
                        on_pick.clone(),
                        on_copy.clone(),
                        last_copied,
                        http_base.clone(),
                        contract_filter,
                        on_contract_filter_change.clone(),
                        contract_sort,
                        on_contract_sort_change.clone(),
                        publisher_filter,
                        on_publisher_filter_clear,
                        collapsed_groups,
                        on_group_toggle,
                    ),
                }
            }
        </>
    }
}

fn render_nodes_list(
    nodes: Vec<FlatNode>,
    q: &str,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
) -> Html {
    let mut filtered: Vec<FlatNode> = Vec::new();
    let total = nodes.len();
    for n in nodes {
        if !q.is_empty()
            && !(n.id.to_lowercase().contains(q)
                || n.label.to_lowercase().contains(q)
                || n.seen_by.iter().any(|s| s.to_lowercase().contains(q)))
        {
            continue;
        }
        filtered.push(n);
    }
    html! {
        <>
            <div class="result-count">
                { format!("{} of {}", filtered.len(), total) }
            </div>
            <div class="node-list">
                {
                    if filtered.is_empty() {
                        html! { <p class="empty">{"no matches"}</p> }
                    } else {
                        filtered.iter().map(|n| {
                            render_node_row(n, selected, on_pick.clone(), on_copy.clone(), last_copied, q)
                        }).collect::<Html>()
                    }
                }
            </div>
        </>
    }
}

#[allow(clippy::too_many_arguments)]
fn render_contracts_list(
    contracts: Vec<FlatContract>,
    q: &str,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
    http_base: Rc<Option<String>>,
    filter: ContractFilter,
    on_filter_change: Callback<ContractFilter>,
    sort: ContractSort,
    on_sort_change: Callback<ContractSort>,
    publisher_filter: Option<String>,
    on_publisher_filter_clear: Callback<MouseEvent>,
    collapsed_groups: HashSet<String>,
    on_group_toggle: Callback<String>,
) -> Html {
    let total = contracts.len();
    // Apply text query + facet filters + publisher filter in one pass.
    let mut filtered: Vec<FlatContract> = contracts
        .into_iter()
        .filter(|c| {
            // Text query: match against title, all instance keys, all
            // publishers, and code_hash so a partial hash works too.
            if !q.is_empty() {
                let in_keys = c.instance_keys.iter().any(|k| k.to_lowercase().contains(q));
                let in_pubs = c.seen_by.iter().any(|s| s.to_lowercase().contains(q));
                let in_title = c
                    .title
                    .as_deref()
                    .map(|t| t.to_lowercase().contains(q))
                    .unwrap_or(false)
                    || c.title_variants.iter().any(|t| t.to_lowercase().contains(q));
                let in_hash = c.code_hashes.iter().any(|h| h.to_lowercase().contains(q));
                if !(in_keys || in_pubs || in_title || in_hash) {
                    return false;
                }
            }
            // Kind facet.
            match (filter.kind, c.has_web_interface) {
                (ContractKindFilter::Any, _) => {}
                (ContractKindFilter::Web, Some(true)) => {}
                (ContractKindFilter::Data, Some(false)) => {}
                (ContractKindFilter::Unknown, None) => {}
                _ => return false,
            }
            // Instance count facet.
            match filter.instance {
                InstanceFilter::Any => {}
                InstanceFilter::Multi if c.instance_keys.len() > 1 => {}
                InstanceFilter::Solo if c.instance_keys.len() == 1 => {}
                _ => return false,
            }
            // Min subscribers facet.
            if (c.seen_by.len() as u32) < filter.min_subscribers {
                return false;
            }
            // Publisher filter (if active): must include this publisher.
            if let Some(p) = publisher_filter.as_deref() {
                if !c.seen_by.iter().any(|s| s == p) {
                    return false;
                }
            }
            true
        })
        .collect();

    // Apply user-selected sort. `KindThenName` keeps the existing
    // bucketed order from `flat_contracts` — fall through with a stable
    // identity sort so we don't undo it.
    sort_contracts(&mut filtered, sort);

    let visible = filtered.len();

    html! {
        <>
            { render_facet_chips(filter, on_filter_change.clone()) }
            { render_sort_dropdown(sort, on_sort_change) }
            {
                if let Some(p) = publisher_filter.as_deref() {
                    html! {
                        <div class="active-filter">
                            <span>{"hosted by "}<b>{ p.to_string() }</b></span>
                            <button class="active-filter-clear"
                                    onclick={on_publisher_filter_clear}
                                    title="clear publisher filter">{"✕"}</button>
                        </div>
                    }
                } else { html! {} }
            }
            <div class="result-count">
                { format!("{visible} of {total}") }
                {
                    if filter != ContractFilter::default() || sort != ContractSort::default() || publisher_filter.is_some() {
                        html! { <span class="result-count-modifier">{" • filtered"}</span> }
                    } else { html! {} }
                }
            </div>
            { render_grouped_contracts(filtered, selected, on_pick, on_copy, last_copied, http_base, sort, &collapsed_groups, on_group_toggle, q) }
        </>
    }
}

fn sort_contracts(items: &mut [FlatContract], sort: ContractSort) {
    // Every branch ends with `then_with(|| a.key.cmp(&b.key))` — the
    // canonical instance id is unique per row and breaks every tie
    // deterministically. Without it, two rows with identical visible
    // values (same title or same subscriber count) flipped order
    // between renders because `flat_contracts` iterates a `HashMap`,
    // whose iteration order is randomised per session.
    match sort {
        ContractSort::KindThenName => { /* `flat_contracts` already does this */ }
        ContractSort::SubscribersDesc => {
            items.sort_by(|a, b| {
                b.seen_by.len().cmp(&a.seen_by.len()).then_with(|| {
                    let la = a.title.as_deref().unwrap_or(&a.key).to_lowercase();
                    let lb = b.title.as_deref().unwrap_or(&b.key).to_lowercase();
                    la.cmp(&lb)
                }).then_with(|| a.key.cmp(&b.key))
            });
        }
        ContractSort::InstancesDesc => {
            items.sort_by(|a, b| {
                b.instance_keys.len().cmp(&a.instance_keys.len()).then_with(|| {
                    let la = a.title.as_deref().unwrap_or(&a.key).to_lowercase();
                    let lb = b.title.as_deref().unwrap_or(&b.key).to_lowercase();
                    la.cmp(&lb)
                }).then_with(|| a.key.cmp(&b.key))
            });
        }
        ContractSort::LastUpdateDesc => {
            items.sort_by(|a, b| {
                b.last_seen_ms
                    .unwrap_or(0)
                    .cmp(&a.last_seen_ms.unwrap_or(0))
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
        ContractSort::NameAsc => {
            items.sort_by(|a, b| {
                let la = a.title.as_deref().unwrap_or(&a.key).to_lowercase();
                let lb = b.title.as_deref().unwrap_or(&b.key).to_lowercase();
                la.cmp(&lb).then_with(|| a.key.cmp(&b.key))
            });
        }
    }
}

fn render_facet_chips(filter: ContractFilter, on_change: Callback<ContractFilter>) -> Html {
    let chip = |label: &'static str, active: bool, next_filter: ContractFilter| -> Html {
        let cb = on_change.clone();
        let onclick = Callback::from(move |_: MouseEvent| cb.emit(next_filter));
        let class = classes!("facet-chip", active.then_some("active"));
        html! { <button class={class} onclick={onclick}>{ label }</button> }
    };
    let mut without_kind = filter; without_kind.kind = ContractKindFilter::Any;
    let mut to_web = filter; to_web.kind = ContractKindFilter::Web;
    let mut to_data = filter; to_data.kind = ContractKindFilter::Data;
    let mut to_unknown = filter; to_unknown.kind = ContractKindFilter::Unknown;

    let mut without_inst = filter; without_inst.instance = InstanceFilter::Any;
    let mut to_multi = filter; to_multi.instance = InstanceFilter::Multi;
    let mut to_solo = filter; to_solo.instance = InstanceFilter::Solo;

    let mut min1 = filter; min1.min_subscribers = 1;
    let mut min2 = filter; min2.min_subscribers = 2;
    let mut min3 = filter; min3.min_subscribers = 3;

    html! {
        <div class="facet-rows">
            <div class="facet-row">
                <span class="facet-label">{"kind"}</span>
                { chip("any",  filter.kind == ContractKindFilter::Any,     without_kind) }
                { chip("web",  filter.kind == ContractKindFilter::Web,     to_web) }
                { chip("data", filter.kind == ContractKindFilter::Data,    to_data) }
                { chip("?",    filter.kind == ContractKindFilter::Unknown, to_unknown) }
            </div>
            <div class="facet-row">
                <span class="facet-label">{"count"}</span>
                { chip("any",   filter.instance == InstanceFilter::Any,   without_inst) }
                { chip("multi", filter.instance == InstanceFilter::Multi, to_multi) }
                { chip("solo",  filter.instance == InstanceFilter::Solo,  to_solo) }
            </div>
            <div class="facet-row">
                <span class="facet-label">{"≥subs"}</span>
                { chip("1",  filter.min_subscribers <= 1, min1) }
                { chip("2+", filter.min_subscribers == 2, min2) }
                { chip("3+", filter.min_subscribers >= 3, min3) }
            </div>
        </div>
    }
}

fn render_sort_dropdown(sort: ContractSort, on_change: Callback<ContractSort>) -> Html {
    let onchange = Callback::from(move |e: web_sys::Event| {
        let target: web_sys::HtmlSelectElement = e.target_unchecked_into();
        let next = match target.value().as_str() {
            "subs" => ContractSort::SubscribersDesc,
            "instances" => ContractSort::InstancesDesc,
            "recent" => ContractSort::LastUpdateDesc,
            "name" => ContractSort::NameAsc,
            _ => ContractSort::KindThenName,
        };
        on_change.emit(next);
    });
    let val = match sort {
        ContractSort::KindThenName => "default",
        ContractSort::SubscribersDesc => "subs",
        ContractSort::InstancesDesc => "instances",
        ContractSort::LastUpdateDesc => "recent",
        ContractSort::NameAsc => "name",
    };
    html! {
        <div class="sort-row">
            <label class="sort-label">{"sort"}</label>
            <select class="sort-select" value={val} onchange={onchange}>
                <option value="default"   selected={sort == ContractSort::KindThenName}>{"default (kind → name)"}</option>
                <option value="subs"      selected={sort == ContractSort::SubscribersDesc}>{"subscribers ↓"}</option>
                <option value="instances" selected={sort == ContractSort::InstancesDesc}>{"instances ↓"}</option>
                <option value="recent"    selected={sort == ContractSort::LastUpdateDesc}>{"last update ↓"}</option>
                <option value="name"      selected={sort == ContractSort::NameAsc}>{"name ↑"}</option>
            </select>
        </div>
    }
}

#[allow(clippy::too_many_arguments)]
fn render_grouped_contracts(
    items: Vec<FlatContract>,
    selected: Option<&str>,
    on_pick: Callback<String>,
    on_copy: Callback<String>,
    last_copied: Option<&str>,
    http_base: Rc<Option<String>>,
    sort: ContractSort,
    collapsed: &HashSet<String>,
    on_toggle: Callback<String>,
    query: &str,
) -> Html {
    if items.is_empty() {
        return html! { <div class="node-list"><p class="empty">{"no matches"}</p></div> };
    }
    // For non-default sort axes we don't section the list — the whole
    // point of "subscribers ↓" or "last update ↓" is one continuous
    // ranking. Default sort keeps the kind-bucketed grouping so users
    // can collapse "data only" when it dominates.
    if sort != ContractSort::KindThenName {
        return html! {
            <div class="node-list">
                {
                    items.iter().map(|c| {
                        render_contract_row(c, selected, on_pick.clone(), on_copy.clone(), last_copied, http_base.clone(), query)
                    }).collect::<Html>()
                }
            </div>
        };
    }
    let mut webs: Vec<FlatContract> = Vec::new();
    let mut unknowns: Vec<FlatContract> = Vec::new();
    let mut datas: Vec<FlatContract> = Vec::new();
    for c in items {
        match c.has_web_interface {
            Some(true) => webs.push(c),
            None => unknowns.push(c),
            Some(false) => datas.push(c),
        }
    }
    let group = |id: &'static str, label: &'static str, rows: Vec<FlatContract>| -> Html {
        if rows.is_empty() {
            return html! {};
        }
        let count = rows.len();
        let is_collapsed = collapsed.contains(id);
        let cb = on_toggle.clone();
        let id_owned = id.to_string();
        let onclick = Callback::from(move |_: MouseEvent| cb.emit(id_owned.clone()));
        let chevron = if is_collapsed { "▶" } else { "▼" };
        html! {
            <div class="contract-group">
                <button class="contract-group-header" onclick={onclick}>
                    <span class="contract-group-chevron">{ chevron }</span>
                    <span class="contract-group-label">{ label }</span>
                    <span class="contract-group-count">{ format!("({count})") }</span>
                </button>
                {
                    if is_collapsed {
                        html! {}
                    } else {
                        html! {
                            <div class="node-list">
                                {
                                    rows.iter().map(|c| {
                                        render_contract_row(c, selected, on_pick.clone(), on_copy.clone(), last_copied, http_base.clone(), query)
                                    }).collect::<Html>()
                                }
                            </div>
                        }
                    }
                }
            </div>
        }
    };
    html! {
        <div class="contract-groups">
            { group("webapps", "Webapps",      webs) }
            { group("unknown", "Unprobed",     unknowns) }
            { group("data",    "Data only",    datas) }
        </div>
    }
}

/// Wrap every occurrence of `q` (case-insensitive) inside `text` with
/// `<mark class="hl">` so the user sees *why* a row matched. Returns
/// the original text wrapped in a single fragment when `q` is empty.
/// The match is byte-position-based (works on multi-byte UTF-8 too).
fn highlight(text: &str, q: &str) -> Html {
    if q.is_empty() {
        return html! { <>{ text.to_string() }</> };
    }
    let lower = text.to_lowercase();
    let q_lower = q.to_lowercase();
    if !lower.contains(&q_lower) {
        return html! { <>{ text.to_string() }</> };
    }
    let mut out: Vec<Html> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        match lower[i..].find(&q_lower) {
            Some(rel) => {
                let abs = i + rel;
                if abs > i {
                    out.push(html! { <>{ text[i..abs].to_string() }</> });
                }
                let end = abs + q_lower.len();
                out.push(html! { <mark class="hl">{ text[abs..end].to_string() }</mark> });
                i = end;
            }
            None => {
                out.push(html! { <>{ text[i..].to_string() }</> });
                break;
            }
        }
    }
    html! { <>{ for out }</> }
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
    query: &str,
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
    // 5-minute stale threshold matches `graph::STALE_AFTER_MS`. Kept
    // local rather than imported so this module doesn't bring in the
    // graph module just for one constant.
    const STALE_AFTER_MS: u64 = 5 * 60 * 1000;
    let now_ms = web_sys::js_sys::Date::now() as u64;
    let is_stale = n
        .last_seen_ms
        .map(|ts| now_ms.saturating_sub(ts) > STALE_AFTER_MS)
        .unwrap_or(false);
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
                    <span class="row-label">{ highlight(label_main, query) }</span>
                    { if let Some(v) = &n.version { html! { <span class="row-tag">{ format!("v{v}") }</span> } } else { html!{} } }
                    { if is_stale { html! { <span class="row-tag row-tag-stale" title="no fresh entry in >5 min">{"stale"}</span> } } else { html!{} } }
                </div>
                { if let Some(addr) = secondary { html! { <div class="row-sub">{ highlight(addr, query) }</div> } } else { html!{} } }
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
    http_base: Rc<Option<String>>,
    query: &str,
) -> Html {
    let is_selected = selected == Some(c.key.as_str());
    let row_class = classes!("node-row", is_selected.then_some("selected"));
    let id = c.key.clone();
    let onclick = Callback::from(move |_: MouseEvent| on_pick.emit(id.clone()));
    // Webapp open URL — built once per row when both ws→http base
    // resolved and the contract is a confirmed webapp. `None` disables
    // both the "↗" link and the row's middle-click open gesture.
    let webapp_url: Option<String> = match (c.has_web_interface, http_base.as_ref()) {
        (Some(true), Some(base)) => Some(format!("{base}/v1/contract/web/{}/", c.key)),
        _ => None,
    };
    // Middle-click anywhere on a webapp row opens it in a new tab.
    // Mirrors the browser-native middle-click-on-link gesture so users
    // don't have to aim at the small "↗" target. `auxclick` fires on
    // *any* non-primary mouse button — gate on `button == 1` (middle)
    // so right-click still gets the standard context menu.
    let onauxclick = match webapp_url.as_ref() {
        Some(url) => {
            let url = url.clone();
            Callback::from(move |e: MouseEvent| {
                if e.button() != 1 {
                    return;
                }
                e.prevent_default();
                e.stop_propagation();
                if let Some(window) = web_sys::window() {
                    let _ = window.open_with_url_and_target(&url, "_blank");
                }
            })
        }
        None => Callback::noop(),
    };
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
    let main_label_clone = main_label.clone();
    let key_line_clone = key_line.clone();
    // Code-hash badge. One hash → show its 6-char prefix.
    // Multiple hashes (same-named app, different WASM) →
    // "⬢ N versions" with a tooltip listing every distinct hash.
    let code_badge: Option<Html> = match c.code_hashes.as_slice() {
        [] => None,
        [h] => {
            let short: String = h.chars().take(6).collect();
            Some(html! {
                <span class="code-hash-badge" title={format!("code hash: {h}")}>
                    { format!("⬢ {short}") }
                </span>
            })
        }
        many => {
            let n = many.len();
            let tooltip = format!(
                "{n} distinct code hashes — same-named app, different WASM bytes:\n• {}",
                many.join("\n• ")
            );
            Some(html! {
                <span class="code-hash-badge code-hash-badge-multi" title={tooltip}>
                    { format!("⬢ {n} versions") }
                </span>
            })
        }
    };
    // If different titles were observed for the same app (typical
    // when state is included in `<title>`), expose a tooltip showing
    // the variants. Useful for the now-rare case where code_hash
    // grouping merges instances that show different UI names.
    let variants_tooltip = if c.title_variants.len() > 1 {
        Some(format!(
            "{} title variants observed:\n• {}",
            c.title_variants.len(),
            c.title_variants.join("\n• ")
        ))
    } else {
        None
    };
    html! {
        <div class={row_class} onclick={onclick} onauxclick={onauxclick}>
            <span class="hue-dot" style="background: #14b8a6;"></span>
            <div class="row-text">
                <div class="row-main">
                    <span class={classes!("kind-tag", "kind-contract")}>{"contract"}</span>
                    {
                        if let Some(t) = variants_tooltip.as_ref() {
                            html! { <span class="row-label" title={t.clone()}>{ highlight(&main_label_clone, query) }</span> }
                        } else {
                            html! { <span class="row-label">{ highlight(&main_label_clone, query) }</span> }
                        }
                    }
                    { if let Some((cls, text)) = web_badge { html! { <span class={classes!(cls)}>{ text }</span> } } else { html!{} } }
                    { if let Some(b) = code_badge { b } else { html!{} } }
                    {
                        if instance_count > 1 {
                            html! { <span class="row-tag">{ format!("{instance_count} instances") }</span> }
                        } else { html!{} }
                    }
                    <span class="row-tag">{ format!("{} subs", c.seen_by.len()) }</span>
                </div>
                <div class="row-sub">{ highlight(&key_line_clone, query) }</div>
                <div class="row-sub">
                    { if let Some(s) = &c.subscribed_ago { html! { <span>{"subscribed "}{ s }</span> } } else { html!{} } }
                    { if let Some(u) = &c.last_update_ago { html! { <span>{" • last update "}{ u }</span> } } else { html!{} } }
                    { if !seen.is_empty() { html! { <span>{" • "}{ highlight(&seen, query) }</span> } } else { html!{} } }
                </div>
            </div>
            {
                // `<a target="_blank">` — browser handles every native
                // open-in-new-tab gesture (left/middle/ctrl+click,
                // right-click → "Open in new tab") with no extra JS.
                // `stop_propagation` keeps clicks on the link itself
                // from also triggering the row's selection handler.
                if let Some(url) = webapp_url.as_ref() {
                    let stop = Callback::from(|e: MouseEvent| e.stop_propagation());
                    html! {
                        <a class="contract-open-btn"
                           href={url.clone()}
                           target="_blank"
                           rel="noopener noreferrer"
                           title="open webapp in new tab"
                           onclick={stop.clone()}
                           onauxclick={stop}>{"↗"}</a>
                    }
                } else {
                    html! {}
                }
            }
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
                    {"All graph content comes from operator-run "}
                    <code>{"topology-publisher"}</code>
                    {" daemons that subscribe to the same Freenet topology \
                    contract (see "}<code>{"Network sharing"}</code>{" below). \
                    Each publisher signs its own snapshot with an Ed25519 key, \
                    so labels and peer lists are operator-controlled — no \
                    visitor of this dashboard can rename a node or fake a \
                    publisher entry."}
                </p>
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
                </details>
                </section>

                <section class="settings-group">
                <h3>{"🔗 Network sharing"}</h3>
                <p class="hint">
                    {"This dashboard subscribes to a Freenet topology contract \
                    on your local node and renders every signed "}
                    <code>{"EntryPayload"}</code>{" it receives. Entries are \
                    verified against their embedded Ed25519 key before merging \
                    — bad signatures are dropped silently."}
                </p>
                <p class="hint">
                    {"The dashboard itself does "}<em>{"not"}</em>{" publish: \
                    the sandboxed iframe can't query the local node's real \
                    peer list (CORS + "}<code>{"NodeQueries"}</code>{" gating). \
                    Real data comes from "}<code>{"topology-publisher"}</code>
                    {", a small daemon that runs alongside your freenet node \
                    and pushes a signed snapshot every 60 s."}
                </p>
                <p class="hint">
                    {"📦 To contribute your node's view to the graph, set up \
                    the daemon — see the "}
                    <a href="https://github.com/Basedfloppa/freenet-net-graph/blob/main/topology-publisher/README.md"
                       target="_blank" rel="noopener noreferrer">{"README"}</a>
                    {" for a one-page guide (build, key file, systemd unit, "}
                    <code>{"/healthz"}</code>{" + "}<code>{"/metrics"}</code>
                    {"). Source on "}
                    <a href="https://github.com/Basedfloppa/freenet-net-graph"
                       target="_blank" rel="noopener noreferrer">{"GitHub"}</a>{"."}
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
                <p class="hint">
                    { format!("{} publisher(s) seen in this session.", props.remote_entry_count) }
                    {" Each remote entry is verified against its embedded \
                    Ed25519 public key before merging — bad signatures are dropped."}
                </p>
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
