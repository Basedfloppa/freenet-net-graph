//! Topology-contract wire types.
//!
//! The on-chain (well, on-contract) state is a map keyed by node identity
//! (Ed25519 public key) of the most-recent signed snapshot that node has
//! published about itself: its ring location, external address, and direct
//! neighbours. A node's entry can only be replaced by an entry signed with
//! the matching key, and only by one with a strictly newer timestamp
//! (LWW per key). The contract's `update_state` enforces both checks.
//!
//! ```text
//! Publisher                        Contract                    Subscriber (dashboard)
//!    |                                |                             |
//!    |-- sign(payload, sk) --------->|                              |
//!    |   PUT/UPDATE delta            |                              |
//!    |                               |-- verify sig                 |
//!    |                               |-- compare ts to existing     |
//!    |                               |-- replace if newer ----------|---> notify
//!    |                                                              |
//!    |                                                              v
//!    |                                                          merged graph
//! ```
//!
//! Wire format is bincode (1.x) over the contract's `State` / `StateDelta`
//! byte channels. Every field is `Vec<u8>` / fixed-size arrays so the encoding
//! is byte-stable across platforms.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Length of an Ed25519 public key in bytes.
pub const PUBKEY_LEN: usize = 32;
/// Length of an Ed25519 signature in bytes.
pub const SIG_LEN: usize = 64;

/// A neighbour of the publishing node, as the dashboard reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NeighborInfo {
    /// `host:port` (UDP socket address from the dashboard).
    pub address: String,
    pub location: Option<f64>,
    pub is_gateway: bool,
}

/// What the publisher signs and what subscribers display.
///
/// The `public_key` field is embedded so the contract can verify without
/// depending on out-of-band PKI.
///
/// Wire format note: bincode is positional, so adding fields here is a
/// breaking change for any deployed contract state. The `contracts`
/// field was added together with the dashboard's switch to a
/// subscription-only data source — older publishers must be upgraded
/// in lockstep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntryPayload {
    /// Ed25519 public key of the publisher; identifies this entry slot.
    #[serde(with = "byte_array_32")]
    pub public_key: [u8; PUBKEY_LEN],
    /// Externally-visible UDP address (`host:port`) — same string the dashboard
    /// shows under "External address". Empty if not yet known.
    pub external_address: String,
    /// Ring location reported by the publishing node, if any.
    pub own_location: Option<f64>,
    /// freenet-core build version, e.g. "0.1.148".
    pub version: Option<String>,
    /// Directly-connected peers as the publishing node sees them.
    pub neighbors: Vec<NeighborInfo>,
    /// Contracts this node is currently subscribed to.
    ///
    /// Each entry is either a bare base58 contract key (legacy / skeleton
    /// publisher with no probing capability) **or** an enriched string
    /// `<base58>|w` / `<base58>|w|t=<percent-encoded-title>` / `<base58>|d`
    /// produced by [`encode_contract_entry`]. Use [`decode_contract_entry`]
    /// on the subscriber side to extract `(key, is_webapp, title)`.
    ///
    /// The encoding is backward-compatible: bare base58 keys never contain
    /// `|`, so a parser that splits on `|` cleanly recovers both old and
    /// new entries. This keeps the `bincode`-positional wire format stable
    /// — adding richer per-contract metadata does not require a coordinated
    /// publisher/subscriber upgrade.
    pub contracts: Vec<String>,
    /// Wall-clock time at the publisher (milliseconds since UNIX epoch). Used
    /// to break ties under last-writer-wins. Monotonicity is the publisher's
    /// responsibility — the contract treats it as opaque.
    pub timestamp_ms: u64,
}

/// A signed entry: payload bytes + Ed25519 signature over those bytes.
///
/// The verifier recovers `public_key` from `bincode::deserialize::<EntryPayload>(&payload)`.
/// `payload` is *the* canonical bytestring — both publisher and contract must
/// agree byte-for-byte.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignedEntry {
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    #[serde(with = "byte_array_64")]
    pub signature: [u8; SIG_LEN],
}

/// On-contract state: map from public-key to its most-recent signed entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContractState {
    pub entries: BTreeMap<[u8; PUBKEY_LEN], SignedEntry>,
}

/// Delta sent over the wire: a list of signed entries to merge.
///
/// In a one-shot publish the list has length 1 (the publisher's own entry).
/// `get_state_delta` may return many entries when sync'ing a stale subscriber.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContractDelta {
    pub entries: Vec<SignedEntry>,
}

/// Summary used to compute a delta for a partially-stale peer: the highest
/// timestamp this peer has seen for each key. The `get_state_delta` answer
/// includes only entries strictly newer than what the summary lists.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContractSummary {
    pub known_timestamps: BTreeMap<[u8; PUBKEY_LEN], u64>,
}

/// Reasons a `SignedEntry` may be rejected by the contract.
#[derive(Debug, PartialEq)]
pub enum VerifyError {
    BadPayload,
    BadSignature,
    KeyMismatch,
}

impl SignedEntry {
    /// Decode the inner payload and verify the signature against its embedded
    /// public key. Returns the decoded payload on success.
    pub fn verify(&self) -> Result<EntryPayload, VerifyError> {
        let payload: EntryPayload =
            bincode::deserialize(&self.payload).map_err(|_| VerifyError::BadPayload)?;
        let vk = VerifyingKey::from_bytes(&payload.public_key)
            .map_err(|_| VerifyError::KeyMismatch)?;
        let sig = Signature::from_bytes(&self.signature);
        vk.verify(&self.payload, &sig)
            .map_err(|_| VerifyError::BadSignature)?;
        Ok(payload)
    }
}

impl ContractState {
    /// Apply a single signed entry under last-writer-wins semantics.
    ///
    /// Returns `Ok(true)` if the state was modified, `Ok(false)` if the
    /// incoming entry was older or equal (no-op). Returns `Err` if the entry
    /// fails verification — the caller decides whether to drop or surface.
    pub fn apply(&mut self, entry: SignedEntry) -> Result<bool, VerifyError> {
        let payload = entry.verify()?;
        let key = payload.public_key;
        if let Some(existing) = self.entries.get(&key) {
            // Cheap fast-path: existing entry's timestamp dominates new one.
            if let Ok(existing_payload) = bincode::deserialize::<EntryPayload>(&existing.payload) {
                if existing_payload.timestamp_ms >= payload.timestamp_ms {
                    return Ok(false);
                }
            }
        }
        self.entries.insert(key, entry);
        Ok(true)
    }

    /// Compute a summary suitable for handing to a peer that wants to sync.
    pub fn summarize(&self) -> ContractSummary {
        let mut s = ContractSummary::default();
        for (k, v) in &self.entries {
            if let Ok(p) = bincode::deserialize::<EntryPayload>(&v.payload) {
                s.known_timestamps.insert(*k, p.timestamp_ms);
            }
        }
        s
    }

    /// Return entries that are strictly newer than what `summary` lists.
    pub fn delta_against(&self, summary: &ContractSummary) -> ContractDelta {
        let mut entries = Vec::new();
        for (k, v) in &self.entries {
            let payload = match bincode::deserialize::<EntryPayload>(&v.payload) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let known = summary.known_timestamps.get(k).copied().unwrap_or(0);
            if payload.timestamp_ms > known {
                entries.push(v.clone());
            }
        }
        ContractDelta { entries }
    }
}

/// Encode a contract entry for [`EntryPayload::contracts`].
///
/// Wire format (suffix-based, `|`-delimited; segments may appear in any
/// order, unknown segments are ignored — keeps decoder forward-compatible):
/// - `<base58>` — bare key, no metadata (skeleton publisher / probe failed).
/// - `<base58>|w` — daemon-confirmed webapp, no title.
/// - `<base58>|w|t=<pct>` — webapp with title; `<pct>` is percent-encoded.
/// - `<base58>|d` — daemon-confirmed *not* a webapp (data-only contract).
/// - `<base58>|c=<base58_hash>` — code hash (the WASM contract's content
///   hash). Two contract entries sharing this value are *guaranteed* to
///   run the same code — they're the same app. Subscribers use it as the
///   primary grouping key so different titles for one app, or one title
///   coincidentally shared by different apps, both resolve correctly.
/// - Combinable: `<base58>|c=<hash>|w|t=<title>` is a webapp entry with
///   both code hash and friendly title attached.
///
/// Base58 keys never contain `|`, so the delimiter is unambiguous.
pub fn encode_contract_entry(
    key: &str,
    is_webapp: Option<bool>,
    title: Option<&str>,
    code_hash: Option<&str>,
) -> String {
    let mut out = String::with_capacity(key.len() + 64);
    out.push_str(key);
    if let Some(c) = code_hash.map(str::trim).filter(|c| !c.is_empty()) {
        out.push_str("|c=");
        out.push_str(c);
    }
    match is_webapp {
        Some(true) => {
            out.push_str("|w");
            if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
                out.push_str("|t=");
                out.push_str(&pct_encode(t));
            }
        }
        Some(false) => out.push_str("|d"),
        None => {}
    }
    out
}

/// Decoded contract entry. `key` is the base58 contract instance id;
/// `code_hash` is the WASM content hash when the publisher shipped one
/// (newer daemons). Unknown segments are dropped, so older publishers
/// stay forward-compatible.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedContractEntry {
    pub key: String,
    pub is_webapp: Option<bool>,
    pub title: Option<String>,
    pub code_hash: Option<String>,
}

/// Inverse of [`encode_contract_entry`]. Bare base58 key decodes to
/// `{ key, None, None, None }`. Unknown segments are ignored — that's
/// what lets us add suffixes without coordinated subscriber upgrades.
pub fn decode_contract_entry(s: &str) -> DecodedContractEntry {
    let mut parts = s.split('|');
    let key = parts.next().unwrap_or("").to_string();
    let mut is_webapp: Option<bool> = None;
    let mut title: Option<String> = None;
    let mut code_hash: Option<String> = None;
    for part in parts {
        if part == "w" {
            is_webapp = Some(true);
        } else if part == "d" {
            is_webapp = Some(false);
        } else if let Some(rest) = part.strip_prefix("t=") {
            let decoded = pct_decode(rest);
            if !decoded.is_empty() {
                title = Some(decoded);
            }
        } else if let Some(rest) = part.strip_prefix("c=") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                code_hash = Some(trimmed.to_string());
            }
        }
    }
    DecodedContractEntry { key, is_webapp, title, code_hash }
}

/// Minimal percent-encoder: alphanumerics and `- _ . ~` pass through; ` ` →
/// `+`; everything else → `%XX` (uppercase hex). Matches `application/x-www-
/// form-urlencoded` for ASCII; non-ASCII bytes are encoded as their UTF-8.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push(hex_nibble(b >> 4));
            out.push(hex_nibble(b & 0x0f));
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = hex_val(bytes[i + 1]);
                let l = hex_val(bytes[i + 2]);
                match (h, l) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + n - 10) as char,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

mod byte_array_32 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

mod byte_array_64 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_slice(), s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        v.as_slice()
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 64 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn make_entry(sk: &SigningKey, ts: u64, neighbors: Vec<NeighborInfo>) -> SignedEntry {
        let payload = EntryPayload {
            public_key: sk.verifying_key().to_bytes(),
            external_address: "10.0.0.1:31337".into(),
            own_location: Some(0.42),
            version: Some("0.1.148".into()),
            neighbors,
            contracts: Vec::new(),
            timestamp_ms: ts,
        };
        let bytes = bincode::serialize(&payload).unwrap();
        let sig: ed25519_dalek::Signature = sk.sign(&bytes);
        SignedEntry {
            payload: bytes,
            signature: sig.to_bytes(),
        }
    }

    #[test]
    fn signed_entry_verifies() {
        let sk = SigningKey::generate(&mut OsRng);
        let entry = make_entry(&sk, 1, vec![]);
        let payload = entry.verify().unwrap();
        assert_eq!(payload.public_key, sk.verifying_key().to_bytes());
        assert_eq!(payload.timestamp_ms, 1);
    }

    #[test]
    fn tampered_payload_fails() {
        let sk = SigningKey::generate(&mut OsRng);
        let mut entry = make_entry(&sk, 1, vec![]);
        // Flip a byte deep inside the bincoded payload.
        let last = entry.payload.len() - 1;
        entry.payload[last] ^= 0xff;
        assert_eq!(entry.verify(), Err(VerifyError::BadSignature));
    }

    #[test]
    fn lww_merge_keeps_newer() {
        let sk = SigningKey::generate(&mut OsRng);
        let mut state = ContractState::default();

        let old = make_entry(&sk, 100, vec![]);
        let mid = make_entry(&sk, 200, vec![]);
        let same = make_entry(&sk, 200, vec![]); // same ts → no replace
        let new = make_entry(
            &sk,
            300,
            vec![NeighborInfo {
                address: "1.2.3.4:1".into(),
                location: Some(0.5),
                is_gateway: true,
            }],
        );

        assert!(state.apply(old.clone()).unwrap()); // empty → take it
        assert!(state.apply(mid.clone()).unwrap()); // newer → take it
        assert!(!state.apply(old.clone()).unwrap()); // older → no-op
        assert!(!state.apply(same.clone()).unwrap()); // equal ts → no-op (LWW favours incumbent)
        assert!(state.apply(new.clone()).unwrap()); // newer → take it

        let stored = state.entries.get(&sk.verifying_key().to_bytes()).unwrap();
        let p: EntryPayload = bincode::deserialize(&stored.payload).unwrap();
        assert_eq!(p.timestamp_ms, 300);
        assert_eq!(p.neighbors.len(), 1);
    }

    #[test]
    fn cross_key_isolation() {
        let sk_a = SigningKey::generate(&mut OsRng);
        let sk_b = SigningKey::generate(&mut OsRng);
        let mut state = ContractState::default();
        state.apply(make_entry(&sk_a, 100, vec![])).unwrap();
        state.apply(make_entry(&sk_b, 50, vec![])).unwrap();
        assert_eq!(state.entries.len(), 2);
    }

    #[test]
    fn summary_and_delta_round_trip() {
        let sk_a = SigningKey::generate(&mut OsRng);
        let sk_b = SigningKey::generate(&mut OsRng);

        let mut server = ContractState::default();
        server.apply(make_entry(&sk_a, 100, vec![])).unwrap();
        server.apply(make_entry(&sk_b, 200, vec![])).unwrap();

        // Client knows only an old entry for A and nothing for B.
        let mut client = ContractState::default();
        client.apply(make_entry(&sk_a, 50, vec![])).unwrap();

        let summary = client.summarize();
        let delta = server.delta_against(&summary);

        // Should yield a fresh A and the entire B.
        assert_eq!(delta.entries.len(), 2);

        for entry in delta.entries {
            client.apply(entry).unwrap();
        }
        assert_eq!(client.summarize(), server.summarize());
    }

    #[test]
    fn contract_entry_roundtrip() {
        // bare key, untouched by encoder when no probe data
        let bare = "BRQiAyN4VSWRp6sW6Xvt2B6RmHyp6dQFFZhStvpnLUkE";
        assert_eq!(encode_contract_entry(bare, None, None, None), bare);
        let d = decode_contract_entry(bare);
        assert_eq!(d.key, bare);
        assert_eq!(d.is_webapp, None);
        assert_eq!(d.title, None);
        assert_eq!(d.code_hash, None);

        // webapp without title
        let enc = encode_contract_entry(bare, Some(true), None, None);
        assert_eq!(enc, format!("{bare}|w"));
        let d = decode_contract_entry(&enc);
        assert_eq!(d.is_webapp, Some(true));
        assert_eq!(d.title, None);

        // webapp with title — spaces, unicode, special chars
        let title = "Net-Graph Dashboard / 网络图 v1.0";
        let enc = encode_contract_entry(bare, Some(true), Some(title), None);
        let d = decode_contract_entry(&enc);
        assert_eq!(d.key, bare);
        assert_eq!(d.is_webapp, Some(true));
        assert_eq!(d.title.as_deref(), Some(title));

        // data-only contract
        let enc = encode_contract_entry(bare, Some(false), None, None);
        assert_eq!(enc, format!("{bare}|d"));
        let d = decode_contract_entry(&enc);
        assert_eq!(d.is_webapp, Some(false));

        // empty title falls back to no-title encoding
        let enc = encode_contract_entry(bare, Some(true), Some("   "), None);
        assert_eq!(enc, format!("{bare}|w"));

        // code hash round-trip (the new field)
        let code = "7ebvjngtateejbke3trhq9vsidpi8ex7akwzfhwtf13r";
        let enc = encode_contract_entry(bare, Some(true), Some("App"), Some(code));
        let d = decode_contract_entry(&enc);
        assert_eq!(d.key, bare);
        assert_eq!(d.code_hash.as_deref(), Some(code));
        assert_eq!(d.is_webapp, Some(true));
        assert_eq!(d.title.as_deref(), Some("App"));

        // forward-compatibility: a future suffix is silently ignored.
        let future = format!("{bare}|c={code}|w|t={}|future=blah", pct_encode("App"));
        let d = decode_contract_entry(&future);
        assert_eq!(d.key, bare);
        assert_eq!(d.code_hash.as_deref(), Some(code));
        assert_eq!(d.is_webapp, Some(true));
        assert_eq!(d.title.as_deref(), Some("App"));
    }

    #[test]
    fn cross_signing_blocked() {
        // An entry signed by A but claiming to be B must fail verification.
        let sk_a = SigningKey::generate(&mut OsRng);
        let sk_b = SigningKey::generate(&mut OsRng);

        let mut payload = EntryPayload {
            public_key: sk_b.verifying_key().to_bytes(), // claim to be B
            external_address: "1.2.3.4:1".into(),
            own_location: None,
            version: None,
            neighbors: vec![],
            contracts: vec![],
            timestamp_ms: 1,
        };
        // ensure compiler retains the field
        payload.timestamp_ms = 1;
        let bytes = bincode::serialize(&payload).unwrap();
        let sig: ed25519_dalek::Signature = sk_a.sign(&bytes); // but signed by A
        let entry = SignedEntry {
            payload: bytes,
            signature: sig.to_bytes(),
        };
        assert_eq!(entry.verify(), Err(VerifyError::BadSignature));
    }
}
