# freenet-net-graph

A browser-side dashboard that visualises the Freenet network's topology
by subscribing to a shared **topology contract**. Operator-side daemons
push signed snapshots of their node's peer/contract state into the
contract; every visitor's dashboard subscribes, verifies the Ed25519
signatures, and renders a live force-directed graph.

## Layout

```
freenet-net-graph/
├── shared/              # Wire types: Topology + contract namespace
├── scraper/             # HTML parser for the freenet-core dashboard
├── frontend/            # Yew SPA — the dashboard itself
├── topology-publisher/  # Native daemon — auto-discovers peers/contracts
└── topology-contract/   # WASM contract: signed-entry LWW merge
```

There is no backend. The frontend talks directly to the user's local
freenet node over its WebSocket client API; the publisher daemon runs
alongside the node as a regular OS process.

## Two roles

| | Subscribe | Daemon publish |
| ---: | --- | --- |
| Where it runs | any browser tab | systemd unit on the node |
| What | reads `SignedEntry`s from the topology contract, verifies Ed25519, folds into the graph | signs a *full* entry: `NodeDiagnostics` peers + classified contracts (webapp / data-only + `<title>` + WASM code hash) |
| Triggered by | `settings.contract.enabled` (on by default) | always — `topology-publisher.service` |
| Identity | n/a | Ed25519 seed in `~/.config/freenet-net-graph/publisher-key.toml` (`0600`) |
| Origin requirement | any (cross-origin WS works) | n/a — native process, no CORS, no NodeQueries gate |

The dashboard does **not** publish. The sandboxed webapp iframe runs
at the opaque `null` origin — `fetch /` is CORS-blocked and freenet
gates `NodeQueries` for webapps — so even if it tried, its entry would
have no real peer/contract data. Operators contribute by running the
daemon alongside their node. See [`topology-publisher/`](./topology-publisher/)
for build, deploy, probe-cache, and `--display-name` details.

## Build

```bash
# WASM contract (~17 KB after release+wasm-opt)
cd topology-contract
cargo build --release --target wasm32-unknown-unknown

# SPA → frontend/dist (index.html + .js + .wasm + .css)
cd ../frontend
trunk build --release

# Native daemon (host arch + aarch64 cross — see topology-publisher/README.md)
cd ..
cargo build -p topology-publisher --release
cargo build -p topology-publisher --release --target aarch64-unknown-linux-gnu
```

## Tests

```bash
cargo test -p shared             # 7 — sign/verify, LWW, cross-key isolation, entry encoding
cargo test -p scraper-lib        # 5 — HTML parsing
cargo test -p topology-publisher # 3 — HTTP probe parsing, title extraction
(cd topology-contract && cargo test)   # 4 — validate / commutativity
```

## Running locally for development

`trunk serve` hosts the SPA standalone. The dashboard talks to the
local freenet node's WS API (`ws://localhost:7509` by default) and
subscribes to the production topology contract — no per-user setup.

```bash
cd frontend
trunk serve   # → http://127.0.0.1:9000/
```

The dashboard auto-subscribes on first paint. Empty graph means no
publishers have written into the contract yet (or the contract isn't
deployed at the configured instance id; see
[`frontend/src/settings.rs`](./frontend/src/settings.rs) constants).

## Dashboard features (brief tour)

* **Force-directed graph** with pan (drag empty space), zoom (mouse
  wheel anchored to cursor), and node drag (grab any circle to pin
  it temporarily — physics keeps reacting around your hand). A
  reset chip appears in the bottom-left whenever the view has moved.
* **Per-publisher drilldown** appears when you click a gateway:
  pubkey, address, location, peers, hosted contracts (top 8), plus
  a 🔍 button that filters the Contracts tab to that publisher only.
* **Header sparklines** track recent peer-edge and publisher counts
  over a rolling ~hour window, so a slow drift is visible at a
  glance.
* **Contracts tab** with facet chips (kind / count / ≥subs), sort
  selector (name, subscribers, instances, last update), collapsible
  group headers (Webapps / Unprobed / Data only), substring
  highlight on matched rows, and an active-filter pill for the
  publisher filter. Webapp rows expose a "↗" link that opens the
  contract in a new browser tab.
* **App grouping** is title-first for webapps, with a `⬢ N versions`
  badge when multiple WASM hashes share a name (e.g. several
  redeploys of "River" merge into one row whose tooltip lists the
  hashes).

## Settings persistence

Everything user-controllable lives in `localStorage` under one key
(`freenet-net-graph:settings:v1`). When this dashboard ships as a
webapp contract on Freenet itself, every visitor's browser keeps its
own copy of these settings — no shared backend, no global config.
A few cross-reload-relevant fields (sidebar width) also mirror to
the URL fragment so a sandbox-iframe hard reload doesn't reset them.

Settings groups in the drawer (⚙):

* **🌐 Data sources** — short explainer; all data is operator-signed.
* **⏱ Timing** — animation tick rate.
* **🎨 Display** — sidebar width, default tab.
* **🧲 Layout physics** — force-directed simulation parameters.
* **🔗 Network sharing** — contract subscription toggle + status,
  publisher count, link to the publisher daemon README.

The previously-visible "node WS URL", "contract instance id", and
"Known public nodes" inputs were removed — they're now baked
defaults derived from the page origin, since there's no scenario
where a public webapp visitor needs to override them. The contract
identifies publishers by Ed25519 pubkey, so visitors can't spoof
them by editing local fields anyway.

## Publishing the contract / dashboard to Freenet

See `freenet-core`'s `fdev` tooling:

* `fdev publish --code … --release contract --state …` — for the
  topology data contract.
* `fdev website init <name>` then `fdev website publish ./dist
  --key <name>` — for the dashboard webapp contract.
* `fdev website update --key <name> ./dist` — to push a new dashboard
  bundle to the same contract id (versioned by minute timestamp).

After `update`, the freenet node's `webapp_cache/<key>/` directory and
its sibling `<key>.hash` sentinel must be deleted manually for the
new bundle to be served — the cache layer otherwise keeps the previous
extraction. See [`topology-publisher/README.md`](./topology-publisher/README.md)
for operator deploy notes.

Once both contracts are published, distribute the dashboard's
`/v1/contract/web/<instance-id>/` URL and the topology contract's
`instance_id`.
