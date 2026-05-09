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
use web_sys::{MouseEvent, WheelEvent};
use yew::prelude::*;

use crate::settings::LayoutSettings;

/// Affine transform applied to the entire scene `<g>`. Pure visual —
/// the physics simulation runs in untransformed viewBox coords, so
/// pan/zoom never disturbs node positions or springs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewState {
    tx: f64,
    ty: f64,
    scale: f64,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { tx: 0.0, ty: 0.0, scale: 1.0 }
    }
}

/// Cursor-drag bookkeeping kept in a `RefCell` so onmousemove can
/// update it without re-rendering. Re-render happens when we commit
/// a new transform via `view.set(...)`.
#[derive(Default)]
struct DragState {
    active: bool,
    last_client_x: f64,
    last_client_y: f64,
}

/// Zoom factor per wheel-tick. ~15 % per notch is the sweet spot
/// between "feels responsive" and "single tick blew past the area I
/// was looking at". Bounded by `MIN_SCALE`/`MAX_SCALE` below.
const ZOOM_STEP: f64 = 1.15;
const MIN_SCALE: f64 = 0.1;
const MAX_SCALE: f64 = 10.0;

/// Convert client-space pixel coords (from a `MouseEvent`) into the
/// SVG's `viewBox` units, accounting for `xMidYMid meet` letterboxing.
/// Returns `(VIEWBOX/2, VIEWBOX/2)` if the SVG isn't laid out yet —
/// the caller treats that as "centre".
fn client_to_viewbox(svg: &web_sys::Element, client_x: f64, client_y: f64) -> (f64, f64) {
    let rect = svg.get_bounding_client_rect();
    let bw = rect.width();
    let bh = rect.height();
    if bw <= 0.0 || bh <= 0.0 {
        return (CENTER, CENTER);
    }
    let scale_factor = bw.min(bh) / VIEWBOX;
    let off_x = (bw - VIEWBOX * scale_factor) / 2.0;
    let off_y = (bh - VIEWBOX * scale_factor) / 2.0;
    let vbx = (client_x - rect.left() - off_x) / scale_factor;
    let vby = (client_y - rect.top() - off_y) / scale_factor;
    (vbx, vby)
}

/// Multiplier that turns a one-pixel client-space delta into viewBox
/// units. Same letterbox math as `client_to_viewbox`, just the scale
/// component — used by the pan handler so a 10 px drag moves the
/// scene by exactly the same amount in any zoom level.
fn px_to_viewbox_scale(svg: &web_sys::Element) -> f64 {
    let rect = svg.get_bounding_client_rect();
    let bw = rect.width();
    let bh = rect.height();
    if bw <= 0.0 || bh <= 0.0 {
        return 1.0;
    }
    VIEWBOX / bw.min(bh)
}

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
    /// Node currently being dragged by the user, if any. Set by the
    /// `onmousedown` handler on a circle and cleared on `mouseup`. While
    /// `Some`, `step()` skips the physics integration for that node and
    /// pins its position to `(target_x, target_y)` so the cursor leads
    /// it without fighting against repulsion / spring forces. Other
    /// nodes still feel forces from it, so the surrounding cluster
    /// reshapes around the drag in real time.
    pinned: Option<PinnedDrag>,
}

#[derive(Clone, Debug)]
struct PinnedDrag {
    node_id: String,
    target_x: f64,
    target_y: f64,
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
        // If the user was dragging a node that just got removed (e.g. its
        // publisher went silent), drop the pin so step() doesn't keep
        // looking up a vanished id.
        if let Some(p) = &self.pinned {
            if !self.nodes.contains_key(&p.node_id) {
                self.pinned = None;
            }
        }
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

        // Centre gravity, *without* the radius-based clamp. Pan/zoom
        // makes the on-screen viewport portable — pulling distant
        // clusters back to the canvas centre would fight against the
        // user actively panning to look at them. Linear gravity at
        // `k_gravity` still keeps the swarm cohesive when no force
        // is dragging it elsewhere; users who want a tighter pull
        // can crank `k_gravity` from the settings drawer.
        for (id, n) in &self.nodes {
            let dx = n.x - CENTER;
            let dy = n.y - CENTER;
            let fx = -l.k_gravity * dx;
            let fy = -l.k_gravity * dy;
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

        let pinned_id = self.pinned.as_ref().map(|p| p.node_id.clone());
        for (id, n) in self.nodes.iter_mut() {
            if pinned_id.as_deref() == Some(id.as_str()) {
                // Pinned node: forces still apply to *other* nodes
                // (they react around the cursor), but this one's
                // motion is overridden — we set its position to the
                // drag target and zero its velocity so on release it
                // doesn't fly away from accumulated impulse.
                continue;
            }
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

        // Snap the pinned node to its drag target after the force
        // pass, so other nodes see it at the cursor position rather
        // than its pre-step location.
        if let Some(p) = &self.pinned {
            if let Some(n) = self.nodes.get_mut(&p.node_id) {
                n.x = p.target_x;
                n.y = p.target_y;
                n.vx = 0.0;
                n.vy = 0.0;
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

    // Pan/zoom state. `view` triggers re-render on commit; `drag` lives
    // in a RefCell so onmousemove can update its bookkeeping without a
    // re-render storm — only the actual transform set via `view.set()`
    // re-paints the SVG.
    let view = use_state(ViewState::default);
    let drag: Rc<RefCell<DragState>> = use_mut_ref(DragState::default);
    let svg_ref = use_node_ref();

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

    // Render in three layered passes so text never gets clipped by
    // edges or other circles. SVG paint order is document order, so
    // the last group drawn wins:
    //   1. edges (bottom)        — `edges_html` above
    //   2. circles + halos       — `circles_html`
    //   3. text labels (top)     — `labels_html`
    // Earlier we packed each node's circle + label into one `<g>`,
    // which let circle-of-node-B paint over label-of-node-A whenever
    // they overlapped on the canvas. Splitting fixes that: every
    // label is drawn after every circle.
    let mut circles_html: Vec<Html> = Vec::with_capacity(l.nodes.len());
    let mut labels_html: Vec<Html> = Vec::new();

    for n in l.nodes.values() {
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
        let circle_class = classes!(
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

        let halo = if selected_id.as_deref() == Some(n.id.as_str()) {
            html! {
                <circle class="node-halo"
                        cx={n.x.to_string()} cy={n.y.to_string()}
                        r={(r + 8.0).to_string()} />
            }
        } else {
            html! {}
        };

        // Per-circle mousedown to start a node-drag. We `stop_propagation`
        // so the SVG-level pan handler doesn't *also* fire (a single
        // mousedown means "either pan OR drag a node, not both"). The
        // cursor → world transform applies the inverse of the pan/zoom
        // ViewState so a drag works the same at any zoom level.
        let on_node_mousedown = {
            let layout = layout.clone();
            let svg_ref = svg_ref.clone();
            let view_handle = view.clone();
            let node_id = n.id.clone();
            Callback::from(move |e: MouseEvent| {
                if e.button() != 0 {
                    return;
                }
                e.stop_propagation();
                e.prevent_default();
                let Some(svg) = svg_ref.cast::<web_sys::Element>() else { return };
                let (vbx, vby) =
                    client_to_viewbox(&svg, e.client_x() as f64, e.client_y() as f64);
                let v = *view_handle;
                let world_x = (vbx - v.tx) / v.scale;
                let world_y = (vby - v.ty) / v.scale;
                layout.borrow_mut().pinned = Some(PinnedDrag {
                    node_id: node_id.clone(),
                    target_x: world_x,
                    target_y: world_y,
                });
            })
        };

        circles_html.push(html! {
            <g key={n.id.clone()}>
                { halo }
                <circle class={circle_class}
                        cx={n.x.to_string()} cy={n.y.to_string()}
                        r={r.to_string()}
                        style={fill_style}
                        onmousedown={on_node_mousedown}>
                    <title>{ tooltip(n, now_ms) }</title>
                </circle>
            </g>
        });

        let show_label = n.is_gateway || n.is_public_default;
        if show_label {
            let label_dx = if n.x >= CENTER { 12.0 } else { -12.0 };
            let label_anchor = if n.x >= CENTER { "start" } else { "end" };
            let trimmed = trim_label(&n.label);
            let label_class = if n.is_public_default {
                "node-label node-label-public"
            } else {
                "node-label node-label-gw"
            };
            // Dim labels of out-of-focus nodes the same way their
            // circles dim, so the focused subgraph reads as one piece
            // (label + circle move together).
            let label_class = classes!(
                label_class,
                is_focus_dimmed.then_some("node-dimmed"),
            );
            labels_html.push(html! {
                <text class={label_class}
                      x={(n.x + label_dx).to_string()}
                      y={(n.y + 4.0).to_string()}
                      text-anchor={label_anchor}>
                    { trimmed }
                </text>
            });
        }
    }

    let _ = *tick; // depend on tick so the component re-renders each frame

    // ---- pan + zoom handlers --------------------------------------
    // The transform is applied via `<g transform="translate(tx, ty)
    // scale(s)">` wrapping the entire scene. Pan delta arrives in
    // client px and is converted to viewBox units so a drag of N px
    // moves the scene by exactly N px regardless of current zoom.
    // Zoom is anchored to the cursor: the world-space point under
    // the mouse stays under the mouse after the transform changes.

    let onmousedown = {
        let drag = drag.clone();
        Callback::from(move |e: MouseEvent| {
            // Only the primary button initiates pan — middle/right
            // are reserved for native browser behaviour (paste,
            // context menu) which we don't want to swallow.
            if e.button() != 0 {
                return;
            }
            e.prevent_default();
            let mut d = drag.borrow_mut();
            d.active = true;
            d.last_client_x = e.client_x() as f64;
            d.last_client_y = e.client_y() as f64;
        })
    };

    let onmousemove = {
        let drag = drag.clone();
        let view = view.clone();
        let svg_ref = svg_ref.clone();
        let layout = layout.clone();
        Callback::from(move |e: MouseEvent| {
            // Node drag takes priority over pan: the circle's own
            // mousedown set `layout.pinned`, and every subsequent
            // mousemove updates that pin's target to follow the
            // cursor in world coords. Pan only kicks in when no
            // node is being dragged.
            let pinned_active = layout.borrow().pinned.is_some();
            if pinned_active {
                let Some(svg) = svg_ref.cast::<web_sys::Element>() else { return };
                let (vbx, vby) =
                    client_to_viewbox(&svg, e.client_x() as f64, e.client_y() as f64);
                let v = *view;
                let world_x = (vbx - v.tx) / v.scale;
                let world_y = (vby - v.ty) / v.scale;
                if let Some(p) = layout.borrow_mut().pinned.as_mut() {
                    p.target_x = world_x;
                    p.target_y = world_y;
                }
                return;
            }
            let (dx_px, dy_px) = {
                let mut d = drag.borrow_mut();
                if !d.active {
                    return;
                }
                let dx = e.client_x() as f64 - d.last_client_x;
                let dy = e.client_y() as f64 - d.last_client_y;
                d.last_client_x = e.client_x() as f64;
                d.last_client_y = e.client_y() as f64;
                (dx, dy)
            };
            let Some(svg) = svg_ref.cast::<web_sys::Element>() else { return };
            let k = px_to_viewbox_scale(&svg);
            let mut v = *view;
            v.tx += dx_px * k;
            v.ty += dy_px * k;
            view.set(v);
        })
    };

    let stop_drag = {
        let drag = drag.clone();
        let layout = layout.clone();
        Callback::from(move |_: MouseEvent| {
            drag.borrow_mut().active = false;
            // Releasing the mouse hands the node back to physics. We
            // don't keep it pinned because subsequent drags would have
            // to dislodge an invisible anchor — surprising UX.
            layout.borrow_mut().pinned = None;
        })
    };

    let onwheel = {
        let view = view.clone();
        let svg_ref = svg_ref.clone();
        Callback::from(move |e: WheelEvent| {
            // Without prevent_default the wheel event bubbles up and
            // scrolls the surrounding page; on a full-viewport graph
            // that ejects the user from the dashboard.
            e.prevent_default();
            let factor = if e.delta_y() < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
            let Some(svg) = svg_ref.cast::<web_sys::Element>() else { return };
            let (vbx, vby) =
                client_to_viewbox(&svg, e.client_x() as f64, e.client_y() as f64);
            let v = *view;
            let new_scale = (v.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
            let f_eff = new_scale / v.scale;
            // Anchor the world-space point under the cursor: solving
            // `vb = new_tx + new_scale * world` for new_tx given the
            // pre-zoom mapping `world = (vb - tx) / scale`.
            let new_tx = vbx - f_eff * (vbx - v.tx);
            let new_ty = vby - f_eff * (vby - v.ty);
            view.set(ViewState { tx: new_tx, ty: new_ty, scale: new_scale });
        })
    };

    let on_reset_view = {
        let view = view.clone();
        Callback::from(move |_: MouseEvent| view.set(ViewState::default()))
    };

    // `graph-grabbing` flips the cursor to `grabbing` for either kind
    // of drag (pan = the whole scene, or node = a single circle), so
    // the user gets the same visual feedback during both gestures.
    let dragging_now = drag.borrow().active || layout.borrow().pinned.is_some();
    let svg_class = classes!(
        "graph",
        dragging_now.then_some("graph-grabbing"),
    );
    let transform = format!(
        "translate({:.2} {:.2}) scale({:.4})",
        view.tx, view.ty, view.scale
    );
    // Show the reset chip only when the view has been moved away
    // from identity — otherwise it's clutter on first paint.
    let view_dirty = view.tx != 0.0 || view.ty != 0.0 || (view.scale - 1.0).abs() > 1e-6;

    html! {
        <>
            <svg class={svg_class}
                 ref={svg_ref}
                 viewBox={format!("0 0 {VIEWBOX} {VIEWBOX}")}
                 preserveAspectRatio="xMidYMid meet"
                 onmousedown={onmousedown}
                 onmousemove={onmousemove}
                 onmouseup={stop_drag.clone()}
                 onmouseleave={stop_drag}
                 onwheel={onwheel}>
                <g transform={transform}>
                    { for edges_html }
                    { for circles_html }
                    { for labels_html }
                </g>
            </svg>
            {
                if view_dirty {
                    html! {
                        <button class="graph-reset-view"
                                onclick={on_reset_view}
                                title={format!("zoom {:.2}× — click to reset", view.scale)}>
                            { format!("⟲ reset ({:.1}×)", view.scale) }
                        </button>
                    }
                } else { html! {} }
            }
        </>
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

