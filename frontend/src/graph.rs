//! Animated free force-directed node graph.
//!
//! Earlier iterations anchored each node to its `location` on a fixed ring.
//! That preserved DHT-keyspace semantics geometrically, but at our scale
//! (~70+ nodes, asymmetric peer distribution) it produced a hairball through
//! the centre and crowded wedges. We now let nodes find their own positions
//! under three forces:
//!
//! * **Pairwise repulsion** — inverse-square Coulomb-style push. Dominant
//!   declutterer; spaces nodes apart.
//! * **Edge attraction** — Hooke-like spring on each edge. Pulls connected
//!   peers together; communities form their own clusters.
//! * **Mild centre gravity** — a weak pull toward (CENTER, CENTER) so the
//!   graph stays in the viewport instead of drifting off under repulsion.
//!
//! `location` is no longer geometric; it's encoded as the node's *fill hue*
//! (HSL hue = location · 360°), so the keyspace info survives the detach.
//! Gateways and public-default nodes are visually distinguished via stroke
//! and size, not fill.
//!
//! Integration is plain Verlet with velocity damping at ~30 FPS. For ~100
//! nodes this is O(n²) per tick = ~10k pair calcs at 30 Hz = trivial in WASM.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;


use gloo_timers::callback::Interval;
use shared::Topology;
use yew::prelude::*;

use crate::settings::LayoutSettings;

/// SVG coordinate space. Not user-tunable — all positions / radii in this
/// module are written in these units, and `preserveAspectRatio="xMidYMid
/// meet"` scales the canvas to fit. Kept as `const` because changing it
/// would require recoding every magic number in the renderer.
const VIEWBOX: f64 = 1000.0;
const CENTER: f64 = VIEWBOX / 2.0;

struct PhysNode {
    id: String,
    label: String,
    is_gateway: bool,
    is_public_default: bool,
    /// Whether the node has a known ring location (used purely as colour
    /// input now — geometry is location-agnostic).
    has_location: bool,
    /// `location ∈ [0, 1)`, if known. Drives fill hue.
    location: Option<f64>,
    /// Wall-clock ms when the publisher behind this node last reposted.
    /// `None` for transitive peers (no direct timestamp). Drives the
    /// stale-publisher fade so the user can tell which gateways are
    /// actively reporting vs. silent.
    last_seen_ms: Option<u64>,
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
}

/// Publisher entries older than this are rendered faded ("stale"). Five
/// minutes ≈ five publish cycles at the daemon's default 60 s interval,
/// so a single missed heartbeat doesn't trigger; only a sustained
/// silence does. Browser-publish (skeleton) ticks once per
/// settings-save, which is also typically well under this threshold.
const STALE_AFTER_MS: u64 = 5 * 60 * 1000;

impl PhysNode {
    fn new(id: String, label: String, is_gateway: bool, location: Option<f64>) -> Self {
        // Seed near the centre with a deterministic-but-jittered offset so the
        // first-tick repulsion has a non-degenerate gradient. Hashing the id
        // (rather than RNG) keeps the seed stable across hot reloads, which
        // makes visual diffs sane.
        let mut h: u64 = 1469598103934665603;
        for b in id.as_bytes() {
            h = h.wrapping_mul(1099511628211);
            h ^= *b as u64;
        }
        let seed_a = ((h & 0xffff) as f64 / 0xffff as f64) * std::f64::consts::TAU;
        let seed_r = (((h >> 16) & 0xff) as f64 / 0xff as f64) * 80.0 + 30.0;
        Self {
            id,
            label,
            is_gateway,
            is_public_default: false,
            has_location: location.is_some(),
            location,
            last_seen_ms: None,
            x: CENTER + seed_r * seed_a.cos(),
            y: CENTER + seed_r * seed_a.sin(),
            vx: 0.0,
            vy: 0.0,
        }
    }
}

#[derive(Default)]
struct LayoutState {
    nodes: BTreeMap<String, PhysNode>,
    edges: Vec<(String, String)>,
    /// Tracks how many physics ticks have elapsed; used to budget extra
    /// "warmup" steps right after a sync, so newly-inserted clusters reach
    /// their resting position quickly without visible churn.
    ticks_since_sync: u32,
}

impl LayoutState {
    fn sync(&mut self, t: &Topology) {
        let mut new_ids: HashSet<String> = HashSet::new();
        let mut new_edges_set: HashSet<(String, String)> = HashSet::new();
        let mut new_edges: Vec<(String, String)> = Vec::new();

        for gw in &t.gateways {
            let gw_id = gw
                .external_address
                .clone()
                .unwrap_or_else(|| format!("gw::{}", gw.label));
            new_ids.insert(gw_id.clone());

            self.nodes
                .entry(gw_id.clone())
                .and_modify(|n| {
                    n.is_gateway = true;
                    if !n.label.contains(&gw.label) {
                        n.label = format!("{} / {}", n.label, gw.label);
                    }
                    if gw.own_location.is_some() {
                        n.location = gw.own_location;
                        n.has_location = true;
                    }
                    // Always adopt the freshest publish timestamp —
                    // staleness is the freshness of the *most recent*
                    // report, not the first one we saw.
                    if let Some(ts) = gw.last_seen_ms {
                        n.last_seen_ms = Some(
                            n.last_seen_ms.map_or(ts, |cur| cur.max(ts)),
                        );
                    }
                })
                .or_insert_with(|| {
                    let mut node =
                        PhysNode::new(gw_id.clone(), gw.label.clone(), true, gw.own_location);
                    node.last_seen_ms = gw.last_seen_ms;
                    node
                });

            for peer in &gw.peers {
                new_ids.insert(peer.address.clone());

                self.nodes
                    .entry(peer.address.clone())
                    .and_modify(|n| {
                        if peer.is_gateway {
                            n.is_gateway = true;
                        }
                        // Adopt a freshly-learned location, but never overwrite
                        // an already-known one (peers can momentarily report
                        // `null` before the location-exchange completes).
                        if !n.has_location && peer.location.is_some() {
                            n.location = peer.location;
                            n.has_location = true;
                        }
                    })
                    .or_insert_with(|| {
                        PhysNode::new(
                            peer.address.clone(),
                            peer.address.clone(),
                            peer.is_gateway,
                            peer.location,
                        )
                    });

                let key = if gw_id < peer.address {
                    (gw_id.clone(), peer.address.clone())
                } else {
                    (peer.address.clone(), gw_id.clone())
                };
                if new_edges_set.insert(key) {
                    new_edges.push((gw_id.clone(), peer.address.clone()));
                }
            }
        }

        // Inject statically-known nodes (e.g. public default gateways).
        // Dedup by `address`: if a scraped gateway already reported this
        // node as a peer, merge — keep the existing physics state, just
        // upgrade the label to the friendly name and mark as public.
        for kn in &t.known_nodes {
            new_ids.insert(kn.address.clone());
            self.nodes
                .entry(kn.address.clone())
                .and_modify(|n| {
                    n.is_gateway = true;
                    n.is_public_default = true;
                    if !n.label.contains(&kn.label) {
                        n.label = format!("{} ({})", kn.label, kn.address);
                    }
                    if !n.has_location && kn.location.is_some() {
                        n.location = kn.location;
                        n.has_location = true;
                    }
                })
                .or_insert_with(|| {
                    let mut node = PhysNode::new(
                        kn.address.clone(),
                        format!("{} ({})", kn.label, kn.address),
                        true,
                        kn.location,
                    );
                    node.is_public_default = true;
                    node
                });
        }

        // Drop nodes that disappeared from the topology so the graph shrinks
        // when the user removes a gateway from `--gateway` flags.
        self.nodes.retain(|id, _| new_ids.contains(id));
        self.edges = new_edges;
        self.ticks_since_sync = 0;
    }

    fn step(&mut self, l: &LayoutSettings) {
        if self.nodes.is_empty() {
            return;
        }

        let positions: Vec<(String, f64, f64)> = self
            .nodes
            .iter()
            .map(|(id, n)| (id.clone(), n.x, n.y))
            .collect();

        let mut forces: HashMap<String, (f64, f64)> = HashMap::with_capacity(self.nodes.len());
        for (id, _) in &self.nodes {
            forces.insert(id.clone(), (0.0, 0.0));
        }

        // Centre gravity, with a soft viewport clamp: linear inside the
        // soft-clamp radius, quadratic outside.
        for (id, n) in &self.nodes {
            let dx = n.x - CENTER;
            let dy = n.y - CENTER;
            let r2 = dx * dx + dy * dy;
            let r = r2.sqrt();
            let scale = if r > l.soft_clamp_radius {
                let over = r - l.soft_clamp_radius;
                l.k_gravity + over * 0.0008
            } else {
                l.k_gravity
            };
            let fx = -scale * dx;
            let fy = -scale * dy;
            let f = forces.get_mut(id).unwrap();
            f.0 += fx;
            f.1 += fy;
        }

        for i in 0..positions.len() {
            for j in (i + 1)..positions.len() {
                let dx = positions[j].1 - positions[i].1;
                let dy = positions[j].2 - positions[i].2;
                let d2 = dx * dx + dy * dy;
                let d = d2.sqrt().max(l.repel_min_dist);
                let mag = l.k_repel / (d * d);
                let ux = dx / d;
                let uy = dy / d;
                let fx = mag * ux;
                let fy = mag * uy;
                if let Some(f) = forces.get_mut(&positions[i].0) {
                    f.0 -= fx;
                    f.1 -= fy;
                }
                if let Some(f) = forces.get_mut(&positions[j].0) {
                    f.0 += fx;
                    f.1 += fy;
                }
            }
        }

        for (a, b) in &self.edges {
            let (ax, ay, bx, by) = match (self.nodes.get(a), self.nodes.get(b)) {
                (Some(na), Some(nb)) => (na.x, na.y, nb.x, nb.y),
                _ => continue,
            };
            let dx = bx - ax;
            let dy = by - ay;
            let d = (dx * dx + dy * dy).sqrt().max(0.01);
            let extension = d - l.edge_rest_length;
            let mag = l.k_edge * extension;
            let ux = dx / d;
            let uy = dy / d;
            let fx = mag * ux;
            let fy = mag * uy;
            if let Some(f) = forces.get_mut(a) {
                f.0 += fx;
                f.1 += fy;
            }
            if let Some(f) = forces.get_mut(b) {
                f.0 -= fx;
                f.1 -= fy;
            }
        }

        for (id, n) in self.nodes.iter_mut() {
            if let Some(&(fx, fy)) = forces.get(id) {
                n.vx = (n.vx + fx) * l.damping;
                n.vy = (n.vy + fy) * l.damping;
                let speed = (n.vx * n.vx + n.vy * n.vy).sqrt();
                if speed > l.max_speed {
                    n.vx *= l.max_speed / speed;
                    n.vy *= l.max_speed / speed;
                }
                n.x += n.vx;
                n.y += n.vy;
            }
        }

        self.ticks_since_sync = self.ticks_since_sync.saturating_add(1);
    }
}

#[derive(Properties, PartialEq)]
pub struct GraphProps {
    pub topology: Rc<Topology>,
    /// Address of the currently-selected node, if any. Drawn with an extra
    /// halo so the user can locate it visually after picking it from the
    /// search list.
    #[prop_or_default]
    pub selected: Option<String>,
    /// Optional focus set of graph node ids derived from the current
    /// selection: clicking a publisher highlights its 1-hop ring,
    /// clicking a contract highlights every publisher that hosts it.
    /// When `Some`, anything *outside* the set dims; `None` renders all
    /// nodes/edges at full opacity. `Rc` because the parent recomputes
    /// it per render and we want cheap clones, not deep copies.
    #[prop_or_default]
    pub highlight_set: Rc<Option<HashSet<String>>>,
    /// User-tunable physics constants. Read every tick; changing them in
    /// the settings drawer rebalances the layout in real time without a
    /// full re-render.
    pub layout: LayoutSettings,
}

#[function_component(Graph)]
pub fn graph(props: &GraphProps) -> Html {
    let layout: Rc<RefCell<LayoutState>> = use_mut_ref(LayoutState::default);
    let tick = use_state(|| 0u64);

    {
        let layout = layout.clone();
        use_effect_with(props.topology.clone(), move |topo| {
            layout.borrow_mut().sync(topo);
            || ()
        });
    }

    // The physics tuning lives in props; we mirror it into a `Rc<RefCell>`
    // so the timer closure can read the *latest* values without stale
    // captures. Changing a slider re-renders the component, which writes
    // the new tuning into this ref via the `use_effect_with` below.
    let live_tuning: Rc<RefCell<LayoutSettings>> = use_mut_ref(LayoutSettings::default);
    *live_tuning.borrow_mut() = props.layout;

    {
        let layout = layout.clone();
        let tick = tick.clone();
        let live_tuning = live_tuning.clone();
        // Re-arm the timer whenever `tick_ms` changes — gloo's `Interval`
        // can't be re-targeted, so we drop+recreate. Other LayoutSettings
        // fields are read live from `live_tuning`, no rearm needed.
        use_effect_with(props.layout.tick_ms, move |&tick_ms| {
            let interval = Interval::new(tick_ms, move || {
                let tuning = *live_tuning.borrow();
                layout.borrow_mut().step(&tuning);
                tick.set(*tick + 1);
            });
            move || drop(interval)
        });
    }

    let l = layout.borrow();

    // Dimming gate: when a focus set is active, anything *not* in the
    // set fades. Edges fade unless *both* endpoints are in focus —
    // edges across the boundary are arguably interesting but they
    // visually clutter the focused subgraph.
    let focus: Option<&HashSet<String>> = match props.highlight_set.as_ref() {
        Some(set) if !set.is_empty() => Some(set),
        _ => None,
    };

    let edges_html: Vec<Html> = l
        .edges
        .iter()
        .filter_map(|(a, b)| {
            let na = l.nodes.get(a)?;
            let nb = l.nodes.get(b)?;
            let mut classes_buf: Vec<&'static str> = Vec::with_capacity(2);
            classes_buf.push(if na.is_gateway || nb.is_gateway {
                "edge edge-gw-peer"
            } else {
                "edge"
            });
            if let Some(fset) = focus {
                if !(fset.contains(a) && fset.contains(b)) {
                    classes_buf.push("edge-dimmed");
                }
            }
            let class = classes_buf.join(" ");
            Some(html! {
                <line class={class}
                      x1={na.x.to_string()} y1={na.y.to_string()}
                      x2={nb.x.to_string()} y2={nb.y.to_string()} />
            })
        })
        .collect();

    let selected_id = props.selected.clone();

    // Use wall-clock at render time to score staleness. Recomputed per
    // tick (cheap), so a publisher that goes silent fades smoothly as
    // its timestamp ages instead of flipping at the next sync.
    let now_ms = web_sys::js_sys::Date::now() as u64;

    let nodes_html: Vec<Html> = l
        .nodes
        .values()
        .map(|n| {
            let (class, r) = if n.is_public_default {
                ("node-public-gw", 11.0)
            } else if n.is_gateway {
                ("node-gw", 10.0)
            } else if n.has_location {
                ("node-peer", 6.5)
            } else {
                ("node-floating", 5.5)
            };

            // Stale fade: only applied when we have a timestamp at all
            // (transitive peers stay normal-opacity — we have no signal
            // on their freshness, dimming them would be misleading).
            let is_stale = n
                .last_seen_ms
                .map(|ts| now_ms.saturating_sub(ts) > STALE_AFTER_MS)
                .unwrap_or(false);
            let is_focus_dimmed = match focus {
                Some(fset) => !fset.contains(&n.id),
                None => false,
            };
            let class = classes!(
                class,
                is_stale.then_some("node-stale"),
                is_focus_dimmed.then_some("node-dimmed"),
            );

            // Fill is the location hue. No-location nodes get a neutral grey.
            let fill_style = match n.location {
                Some(loc) => {
                    let hue = (loc.clamp(0.0, 1.0) * 360.0).round() as u32;
                    format!("fill: hsl({hue}, 65%, 55%);")
                }
                None => "fill: #6b7280;".to_string(),
            };

            let show_label = n.is_gateway || n.is_public_default;
            let label_node = if show_label {
                let label_dx = if n.x >= CENTER { 12.0 } else { -12.0 };
                let label_anchor = if n.x >= CENTER { "start" } else { "end" };
                let trimmed = trim_label(&n.label);
                let label_class = if n.is_public_default {
                    "node-label node-label-public"
                } else {
                    "node-label node-label-gw"
                };
                html! {
                    <text class={label_class}
                          x={(n.x + label_dx).to_string()}
                          y={(n.y + 4.0).to_string()}
                          text-anchor={label_anchor}>
                        { trimmed }
                    </text>
                }
            } else {
                html! {}
            };

            let halo = if selected_id.as_deref() == Some(n.id.as_str()) {
                html! {
                    <circle class="node-halo"
                            cx={n.x.to_string()} cy={n.y.to_string()}
                            r={(r + 8.0).to_string()} />
                }
            } else {
                html! {}
            };

            html! {
                <g key={n.id.clone()}>
                    { halo }
                    <circle class={class}
                            cx={n.x.to_string()} cy={n.y.to_string()}
                            r={r.to_string()}
                            style={fill_style}>
                        <title>{ tooltip(n, now_ms) }</title>
                    </circle>
                    { label_node }
                </g>
            }
        })
        .collect();

    let _ = *tick; // depend on tick so the component re-renders each frame

    html! {
        <svg class="graph"
             viewBox={format!("0 0 {VIEWBOX} {VIEWBOX}")}
             preserveAspectRatio="xMidYMid meet">
            { for edges_html }
            { for nodes_html }
        </svg>
    }
}

fn tooltip(n: &PhysNode, now_ms: u64) -> String {
    let loc = n
        .location
        .map(|l| format!("{l:.4}"))
        .unwrap_or_else(|| "unknown".into());
    let kind = if n.is_public_default {
        "public gateway"
    } else if n.is_gateway {
        "gateway"
    } else {
        "peer"
    };
    let freshness = match n.last_seen_ms {
        Some(ts) => format!(" • last seen {}", human_ago(now_ms.saturating_sub(ts))),
        None => String::new(),
    };
    format!("{}\n{kind} • location: {loc}{freshness}", n.label)
}

/// Render a duration-ago in a compact human form. Stops at minutes —
/// the tooltip is short, sub-second precision adds nothing.
fn human_ago(ms_ago: u64) -> String {
    let secs = ms_ago / 1000;
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

fn trim_label(s: &str) -> String {
    if s.len() > 22 {
        format!("{}…", &s[..21])
    } else {
        s.to_string()
    }
}

