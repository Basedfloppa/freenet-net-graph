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
(`/v1/contract/web/<key>/?__sandbox=1`) to classify it as a webapp or
data-only contract, lifts the page `<title>`, and emits the contract's
WASM code hash — both fields the sandboxed dashboard iframe can't
fetch itself because of the null-origin CORS gate.

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

## Run

Manual:

```bash
topology-publisher \
    --node-ws-url ws://127.0.0.1:7509 \
    --interval-secs 60 \
    --display-name "my-node-nickname"
```

Systemd (template lives in [`./topology-publisher.service`](./topology-publisher.service)):

```bash
sudo install -m 0755 \
    target/release/topology-publisher /usr/local/bin/

sudo install -m 0644 \
    topology-publisher/topology-publisher.service \
    /etc/systemd/system/

sudo systemctl daemon-reload
sudo systemctl enable --now topology-publisher
```

The unit ships with `Type=notify` + `WatchdogSec=180`; the daemon
pings `WATCHDOG=1` after each successful publish, so a hung cycle
restarts the process within ~3 minutes.

## CLI flags

| Flag | Default | Notes |
| --- | --- | --- |
| `--node-ws-url <URL>` | `ws://127.0.0.1:7509` | Local freenet node's WS endpoint. Path is appended automatically. |
| `--instance-id <BASE58>` | `BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE` | Topology contract instance id. Override only for a staging/test contract. |
| `--code-hash <BASE58>` | `3Ug134jfYzEMkwJeRbTEgY33kgXHKEWnZLvmWi3eoDXV` | WASM code hash for the topology contract above. |
| `--interval-secs <N>` | `60` | Publish cadence. |
| `--key-file <PATH>` | `~/.config/freenet-net-graph/publisher-key.toml` | Where the publisher's Ed25519 seed lives. |
| `--display-name <NAME>` | none | Operator-chosen public nickname. Shows in the dashboard as the gateway label (e.g. "baka", "orange"). `--label` is kept as a deprecated alias. |
| `--neighbor LABEL,HOST:PORT` | repeatable, none | Fallback peer list when `NodeQueries` is unsupported (local-mode nodes). Ignored on a network-mode node — real peers come from diagnostics. |
| `--metrics-port <N>` | `0` (off) | If non-zero, bind a tiny HTTP server on `127.0.0.1:<N>` exposing `/healthz` (JSON) and `/metrics` (Prometheus text). |

⚠️ **Privacy note on `--display-name`**: this string is shipped
publicly in `EntryPayload.version` and visible to every dashboard
visitor. Do **not** pass `$(hostname)`, `%H`, or anything
auto-detected — pick a friendly nickname you're comfortable with
appearing on a public graph. Skip the flag entirely and the
dashboard falls back to `remote: <pubkey-prefix>` for that
gateway.

## Adding a new publisher

You're an operator running a fresh freenet node and want it to show
up on the public dashboard. End-to-end:

### 0. Prerequisites

- A `freenet` node running in **`network` mode** (not `local`).
  Local-mode nodes reject `NodeQueries` and the daemon falls back to
  a "skeleton" entry — fine for testing, but not what the public
  graph wants. Verify with `journalctl -u freenet | grep "running in"`
  or look for `mode: Network` in the startup banner.
- The node's WebSocket port (default `7509`) reachable from
  `127.0.0.1` on the host. The daemon talks to it locally; it does
  *not* need any inbound port open to the public internet.
- `systemd` if you want the supplied unit. Other supervisors
  (runit, tmux, …) work — the daemon is a plain foreground process.

### 1. Get the binary

Build from source (latest `main`):

```bash
git clone https://github.com/Basedfloppa/freenet-net-graph
cd freenet-net-graph
cargo build -p topology-publisher --release
# binary: ./target/release/topology-publisher
```

Or grab a pre-built binary from a release artefact / friend.

### 2. Install on the target node

```bash
# from your build host:
scp target/release/topology-publisher \
    YOUR_NODE:/tmp/topology-publisher.new

# on the target node, as root:
install -m 0755 /tmp/topology-publisher.new /usr/local/bin/topology-publisher
rm /tmp/topology-publisher.new
```

### 3. Drop in the systemd unit

Use [`topology-publisher.service`](./topology-publisher.service) as a
template. Adjust `ExecStart` to taste — the operator-relevant knobs
are `--display-name` (your public nickname) and `--metrics-port` if
you want `/healthz` + `/metrics` available on `127.0.0.1`:

```ini
ExecStart=/usr/local/bin/topology-publisher \
    --node-ws-url ws://127.0.0.1:7509 \
    --interval-secs 60 \
    --metrics-port 17071 \
    --display-name "my-node-nickname"
```

Things to consider:

- **`--display-name`** — see the privacy note above. Public string,
  pick something nice; or omit entirely.
- **`--neighbor`** — only relevant when the daemon is talking to a
  `local`-mode node (NodeQueries unavailable). On a network node it's
  ignored.
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

The first cycle fires within seconds; subsequent cycles every
`--interval-secs`. Watch the journal:

```bash
journalctl -u topology-publisher -f
```

Expected lines, in order:

```
loaded publisher identity pubkey=…  key_path=…
connecting to local freenet node url=ws://127.0.0.1:7509/…
subscribed to topology contract
published topology entry peers=N contracts=M webapps=K probed=L
```

If you see `node rejected NodeQueries (`not supported`); falling
back to skeleton publishing` — the local node is in `local` mode.
Switch it to `network` mode, otherwise the daemon keeps shipping bare
identity entries with no peer/contract data.

WS-connect troubleshooting:

- `connection refused` — the freenet node isn't running on the port
  you pointed at, or it's bound to a different interface.
- `handshake failed` — the WS path is wrong; the daemon appends
  `/v1/contract/command?encodingProtocol=native` automatically, so
  pass just `ws://host:port`, no path.

If `--metrics-port` is set, also confirm:

```bash
curl http://127.0.0.1:17071/healthz   # JSON snapshot
curl http://127.0.0.1:17071/metrics   # Prometheus text exposition
```

### 5. (Optional) Confirm in the public dashboard

The dashboard's "Network sharing" panel shows the count of distinct
publishers it's heard from. Within ~1 minute of the daemon's first
publish, the counter increments and your node's peer list shows up
in the graph. If you set `--display-name`, your gateway row reads
`<name> (<pubkey-prefix>)` instead of `remote: <pubkey-prefix>`.

The publisher identity is keyed by the Ed25519 pubkey persisted at
`~/.config/freenet-net-graph/publisher-key.toml`. Same key on
restart → same dashboard slot. Delete the file to start over with a
fresh identity (the contract garbage-collects stale slots eventually).

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
    public_key:       <ed25519 pubkey of this daemon>,
    external_address: <node's listening address, if reported>,
    own_location:     <ring location 0..1, if reported>,
    version:          <--display-name value, or None>,
    neighbors:        <peers from NodeDiagnostics, or --neighbor fallback>,
    contracts:        <enriched per-contract entries — see below>,
    timestamp_ms:     <wall clock>,
}
```

The contract keys on `public_key` and keeps the most recent entry
per publisher (LWW by `timestamp_ms`). Subscribers (any dashboard
tab) see the merged view across every active publisher.

The `version` field is currently overloaded as the operator's
display-name carrier — `freenet-core` doesn't expose its build
version through `NodeDiagnostics` yet, so there's no real version to
ship. The dashboard renders this string as the gateway's friendly
label.

## Contract entry encoding

`EntryPayload.contracts` is a `Vec<String>`. Each string is a
base58 contract instance id followed by zero or more `|`-delimited
metadata segments, in any order:

| Segment | Meaning |
| --- | --- |
| (none) | bare key — legacy / skeleton publisher / probe failed |
| `\|w` | daemon-confirmed webapp |
| `\|w\|t=<pct>` | webapp with `<title>` (percent-encoded UTF-8) |
| `\|d` | confirmed *not* a webapp (probe returned non-200) |
| `\|c=<base58_hash>` | WASM code hash — proves "same app" across instance ids |

Examples:

```
BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE
BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE|w
BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE|w|t=Net-Graph
BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE|c=7ebvjngtate…|w|t=Net-Graph
```

Base58 keys never contain `|`, so the delimiter is unambiguous. Old
subscribers that don't know a particular suffix silently ignore it —
the wire format is forward-compatible. Encoder/decoder live in
[`shared::contract::encode_contract_entry`][1] /
[`decode_contract_entry`][1]; the round-trip test asserts every
combination.

[1]: ../shared/src/contract.rs

The dashboard groups contracts in this priority:
**title (lowercased) → code_hash → key**. Different code-hash
versions of the same-named app fold into one row with a
`⬢ N versions` badge; the tooltip lists the distinct hashes.

## Probe cache

Probes are HTTP GETs to `127.0.0.1:<ws-port>/v1/contract/web/<key>/?__sandbox=1`
on the local node. The `?__sandbox=1` route serves the contract's
*own* HTML — without it, freenet wraps every webapp in a generic outer
shell whose `<title>` is always literally `Freenet`, which is useless
for distinguishing contracts.

Two-pass per cycle:

1. **New keys** (not in cache) — always probed; the daemon sees a
   contract for the first time.
2. **Stale keys** (probed > 30 min ago) — re-probed up to 64 oldest
   entries per cycle. Lets a webapp redeploy / title change reflect
   in the dashboard without a daemon restart.

Probing is parallel-bounded at 16 with a 3-second per-request timeout
so first-cycle latency on a node hosting hundreds of contracts is a
few seconds, not minutes. Transport errors don't poison the cache —
a transient blip retries next cycle, a stale entry whose re-probe
failed retains its previous result.

## Health endpoints

When `--metrics-port <N>` is non-zero, the daemon listens on
`127.0.0.1:<N>` (loopback only — no LAN exposure):

| Path | Format | Purpose |
| --- | --- | --- |
| `/healthz` | JSON | Last-publish timestamp, session liveness, peer/contract/webapp/probed counts. Drop-in for a JSON-aware monitor. |
| `/metrics` | Prometheus text | Same data as gauges (`topology_publisher_*`) plus a `build_info{version="…"}`. Drop-in for any Prometheus scraper. |
| `/` | text | One-screen index of the above. |

Used together with `WatchdogSec=180` in the systemd unit: each
successful publish pings systemd's `WATCHDOG=1`, so a hung cycle
triggers a restart within ~3 minutes; meanwhile the metrics endpoint
gives a scraper an out-of-band liveness signal.

## Reconnect behaviour

The outer connection loop wraps each `run_session(...)` with
exponential backoff (1 s → 60 s, capped). State that survives reconnects:

- `probe_cache` — rebuilding it would re-probe every contract; keeping
  it across reconnects avoids a thundering herd on the local HTTP
  server every WS blip.
- `diagnostics_supported` — a node that rejected NodeQueries last
  session still rejects them now.

WS-fatal errors during a publish (`send Update` / `send NodeDiagnostics`
/ `recv:` failures) bubble up to the outer loop, which sleeps for the
backoff interval before reconnecting. The health endpoint flips
`session_alive: false` immediately on disconnect so a scraper notices
without waiting for the next successful publish.
