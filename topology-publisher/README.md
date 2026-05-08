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
