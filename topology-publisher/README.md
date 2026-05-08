# topology-publisher

Off-tab publisher for the Freenet topology contract. Runs as a normal
OS process alongside a freenet node so the contract gets a fresh,
signed entry every minute even when no one has the dashboard open.

Browser dashboards can't auto-discover their host node's actual peer
list (sandbox iframe origin is `null` — `fetch /` is CORS-blocked,
`freenet-core` rejects `NodeQueries` from web apps). This daemon has
no such restrictions, so on a `network`-mode node it pulls real peer
data via `NodeDiagnostics` and publishes it.

It also probes each subscribed contract over local HTTP
(`/v1/contract/web/<key>/?__sandbox=1`) to classify it as a webapp
or data-only contract and lift the page `<title>` — both fields the
sandboxed dashboard iframe can't fetch itself because of the
null-origin CORS gate.

## Build

```bash
cargo build -p topology-publisher --release
# host arch:        ../target/release/topology-publisher
# cross to aarch64: cargo build -p topology-publisher --release \
#                       --target aarch64-unknown-linux-gnu
```

Cross-compiling to `aarch64-unknown-linux-gnu` needs
`gcc-aarch64-linux-gnu` installed; the linker is wired up in
`../.cargo/config.toml`.

## One-shot build + deploy

```bash
./topology-publisher/deploy.sh
```

Builds both architectures, `scp`s the matching binary to **orange**
(aarch64) and **baka** (x86_64), atomically `install`s it into
`/usr/local/bin/topology-publisher`, restarts the systemd unit, and
tails the journal on each node for ~70 s so the first
`published topology entry peers=… contracts=… webapps=… probed=…`
line is visible. `--dry-run` builds without pushing.

## Run

Manual:

```bash
topology-publisher \
    --node-ws-url ws://127.0.0.1:7509 \
    --interval-secs 60 \
    --label "$(hostname)"
```

Systemd:

```bash
sudo install -m 0755 \
    target/release/topology-publisher /usr/local/bin/

sudo install -m 0644 \
    topology-publisher/topology-publisher.service \
    /etc/systemd/system/

sudo systemctl daemon-reload
sudo systemctl enable --now topology-publisher
```

## Adding a new publisher

You're an operator running a fresh freenet node and want it to show
up on the public dashboard. Here's the end-to-end flow.

### 0. Prerequisites

- A `freenet` node running in **`network` mode** (not `local`).
  Local-mode nodes reject `NodeQueries` and the daemon falls back to
  publishing a "skeleton" entry — useful for testing but not what
  the public graph wants. Verify with `journalctl -u freenet | grep
  "running in"` or look for `mode: Network` in the startup banner.
- The node's WebSocket port (default `7509`) reachable from
  `127.0.0.1` on the host. The daemon talks to it locally; it does
  *not* need any inbound port open to the public internet.
- `systemd` (the unit file in this directory uses it). If you run
  something else, you can still run the binary under `tmux` /
  `runit` / whatever — the daemon is a plain foreground process.

### 1. Get the binary

Either build from source (gives you the latest `main`):

```bash
git clone https://github.com/freenet/freenet-net-graph
cd freenet-net-graph
cargo build -p topology-publisher --release
# binary: ./target/release/topology-publisher
```

Or grab a pre-built binary from a release artefact / friend. The
project's [`./topology-publisher/deploy.sh`](./deploy.sh) builds for
both `x86_64` and `aarch64-unknown-linux-gnu` in one go — handy for
mixed-arch fleets.

### 2. Install on the target node

```bash
# from your build host:
scp target/release/topology-publisher \
    YOUR_NODE:/tmp/topology-publisher.new

# on the target node, as root:
install -m 0755 /tmp/topology-publisher.new /usr/local/bin/topology-publisher
rm /tmp/topology-publisher.new
```

If you crank out multiple machines, the [`deploy.sh`](./deploy.sh)
script in this directory does this atomic-install dance for the
project's two operator nodes — copy it as a starting point.

### 3. Drop in the systemd unit

Use [`topology-publisher.service`](./topology-publisher.service) as
a template. The defaults match the public deployment:

```ini
ExecStart=/usr/local/bin/topology-publisher \
    --node-ws-url ws://127.0.0.1:7509 \
    --interval-secs 60 \
    --label %H \
    --neighbor "nova,5.9.111.215:31337" \
    --neighbor "vega,100.27.151.80:31337"
```

Things to consider:

- **`--label`** is a free-text string that ends up in
  `EntryPayload.version`; subscribers use it to tell publishers apart
  in tooltips. `%H` (the hostname) is a fine default.
- **`--neighbor`** entries are *only* used when the daemon is talking
  to a `local`-mode node (NodeQueries unavailable). On a real network
  node you can omit them — the live `NodeDiagnostics` peer list
  takes over.
- **`User=`** is commented out in the template. Pick the same UID
  that owns the freenet node's `~/.config` / `~/.cache` paths so the
  daemon can write its own key file there. If your freenet runs as
  `root`, leave `User=` unset.

Install:

```bash
sudo install -m 0644 \
    topology-publisher/topology-publisher.service \
    /etc/systemd/system/

sudo systemctl daemon-reload
sudo systemctl enable --now topology-publisher
```

### 4. Verify

The first cycle fires immediately on startup; the next one is
`--interval-secs` later. Watch the journal:

```bash
journalctl -u topology-publisher -f
```

You want to see, in order:

```
loaded publisher identity pubkey=…  key_path=…
connecting to local freenet node url=ws://127.0.0.1:7509/…
subscribed to topology contract
published topology entry peers=N contracts=M webapps=K probed=L
```

If you see `node rejected NodeQueries (`not supported`); falling
back to skeleton publishing` instead — the local node is in
`local` mode. Switch it to `network` mode, otherwise the daemon
keeps shipping bare identity entries with no peer/contract data.

If you see ws-connect errors:

- `connection refused` — the freenet node isn't running on the port
  you pointed at, or it's bound to a different interface.
- `handshake failed` — the WS path is wrong; the daemon appends
  `/v1/contract/command?encodingProtocol=native` automatically, so
  pass just `ws://host:port`, no path.

### 5. (Optional) Confirm in the public dashboard

The dashboard's "Network sharing" panel shows the count of distinct
publishers it's heard from. Within ~1 minute of the daemon's first
publish, the counter increments by one and your node's peer list
shows up in the graph.

The publisher identity is keyed by an Ed25519 pubkey persisted at
`~/.config/freenet-net-graph/publisher-key.toml`. Same key on
restart → same dashboard slot. Delete the file to start over with
a fresh identity (the contract garbage-collects stale slots
eventually).

## Identity

On first run a fresh Ed25519 seed is generated and written to
`~/.config/freenet-net-graph/publisher-key.toml` with `0600` perms.
Subsequent runs keep that pubkey, so the daemon owns one stable
slot in the contract's per-publisher map. Override the path with
`--key-file <path>`.

## Local-mode node (no NodeQueries)

`freenet local` rejects `NodeQueries` (`node.rs:2880` —
`_ => "not supported"`). The daemon detects this and degrades to a
"skeleton" payload that contains only the publisher's identity and
whatever neighbours you pass via `--neighbor LABEL,HOST:PORT` (repeat
for each peer). Use it for dev-mode testing — production should run
against a `network`-mode node.

## What gets published

```
EntryPayload {
    public_key: <ed25519 pubkey of this daemon>,
    external_address: <node's listening address, if reported>,
    own_location: <ring location 0..1, if reported>,
    version: Some(<--label value, defaults to None>),
    neighbors: <peers from NodeDiagnostics, or --neighbor fallback>,
    contracts: <enriched per-contract entries — see below>,
    timestamp_ms: <wall clock>,
}
```

The contract keys on `public_key` and keeps the most recent entry
per publisher. Subscribers (any dashboard tab) see the merged view
across every active publisher in the network.

## Contract entry encoding

`EntryPayload.contracts` is a `Vec<String>`. Each string is one of:

| Form | Meaning |
| --- | --- |
| `<base58>` | bare key — legacy, skeleton publisher, or probe failed |
| `<base58>\|w` | daemon-confirmed webapp, no usable `<title>` |
| `<base58>\|w\|t=<pct>` | webapp with title (percent-encoded) |
| `<base58>\|d` | confirmed *not* a webapp (probe returned non-200) |

Base58 keys never contain `|`, so the delimiter is unambiguous. Old
subscribers that don't know the encoding still see a valid contract
key in the prefix — the metadata segments are silently ignored.
Encoder/decoder live in
[`shared::contract::encode_contract_entry`][1] /
[`decode_contract_entry`][1].

[1]: ../shared/src/contract.rs

## Probe cache

Probes are HTTP GETs to `127.0.0.1:<ws-port>/v1/contract/web/<key>/?__sandbox=1`
on the local node. The `?__sandbox=1` route is what serves the
contract's *own* HTML — without it, freenet wraps every webapp in a
generic outer shell whose `<title>` is always literally `Freenet`,
which is useless for distinguishing contracts.

Results are cached for the daemon's lifetime so the per-cycle cost
stays bounded at "new contracts since last cycle". Restart the
service to refresh classifications (e.g. after a webapp redeploy).
Probing is parallel-bounded at 16 with a 3-second per-request timeout
so first-cycle latency on a node hosting hundreds of contracts is a
few seconds, not minutes.
