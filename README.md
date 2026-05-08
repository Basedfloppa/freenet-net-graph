# freenet-net-graph

A browser-side dashboard that visualises the Freenet network's topology
by subscribing to a shared **topology contract**. Each visitor's
dashboard pulls verified neighbour-list entries from the contract,
optionally publishes its own, and renders a live force-directed graph.

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
freenet node over its WebSocket client API; the publisher daemon
runs alongside the node as a regular OS process.

## Three roles

| | Subscribe | Browser publish | Daemon publish |
| ---: | --- | --- | --- |
| Where it runs | any browser tab | dashboard tab | systemd unit on the node |
| What | reads `SignedEntry`s from the topology contract, verifies Ed25519, folds into the graph | signs a "skeleton" entry built from the user's `known_nodes` | signs a *full* entry: `NodeDiagnostics` peers + classified contracts (webapp / data-only + `<title>`) |
| Triggered by | `settings.contract.enabled` | `settings.contract.publish_enabled` | always — `topology-publisher.service` |
| Identity | n/a | Ed25519 seed in `localStorage` / URL fragment | Ed25519 seed in `~/.config/freenet-net-graph/publisher-key.toml` (`0600`) |
| Origin requirement | any (cross-origin WS works) | same-origin to its node — sandboxed dashboards have origin `null` and ride the outer shell's WS | n/a — native process, no CORS, no NodeQueries gate |

Subscribe + browser-publish share one WebSocket. The daemon owns its
own. See [`topology-publisher/`](./topology-publisher/) for build,
deploy, and probe-cache details.

## Build

```bash
# WASM contract (~17 KB after release+wasm-opt)
cd topology-contract
cargo build --release --target wasm32-unknown-unknown

# SPA → frontend/dist (index.html + .js + .wasm)
cd ../frontend
trunk build --release

# Native daemon (host arch + aarch64 cross — see topology-publisher/README.md)
cd ..
cargo build -p topology-publisher --release
cargo build -p topology-publisher --release --target aarch64-unknown-linux-gnu
```

For one-shot daemon redeploy to both operator nodes:
[`./topology-publisher/deploy.sh`](./topology-publisher/deploy.sh).

## Tests

```bash
cargo test -p shared             # 7 — sign/verify, LWW, cross-key isolation, entry encoding
cargo test -p scraper-lib        # 5 — HTML parsing
cargo test -p topology-publisher # 3 — HTTP probe parsing, title extraction
(cd topology-contract && cargo test)   # 4 — validate / commutativity
```

## Running locally for development

The frontend can be served by Trunk on its own; until it's published
as a webapp contract you need the freenet node + the contract already
deployed for the subscription to actually return data.

```bash
cd frontend
trunk serve   # → http://127.0.0.1:9000/
```

Open the dashboard, click ⚙, fill in:

- **Network sharing → enabled** ✓
- **node WS URL**: e.g. `ws://localhost:7509`
- **contract instance id**: base58 id printed by `fdev publish`
- (optional) **publish enabled** ✓ + **code hash** to push your own view

The dashboard reflects the entries it receives. Empty graph means no
publishers have written into the contract yet (or the contract isn't
deployed at the configured instance id).

## Settings persistence

Everything user-controllable lives in `localStorage` under one key
(`freenet-net-graph:settings:v1`). When this dashboard ships as a
webapp contract on Freenet itself, every visitor's browser keeps its
own copy of these settings — no shared backend, no global config.

Settings are grouped in the drawer:

* **🌐 Data sources** — known public nodes (static graph anchors)
* **⏱ Timing** — animation tick rate
* **🎨 Display** — sidebar width, default tab
* **🧲 Layout physics** — force-directed simulation parameters
* **🔗 Network sharing** — contract subscription + publish worker

## Publishing the contract / dashboard to Freenet

See `freenet-core`'s `fdev` tooling:

* `fdev publish --code … --release contract --state …` for the
  topology data contract
* `fdev website init <name>` then `fdev website publish ./dist
  --key <name>` for the dashboard webapp contract

Once both are published, distribute the dashboard's
`/v1/contract/web/<instance-id>/` URL and the topology contract's
`instance_id`.
