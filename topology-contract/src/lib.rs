//! Topology contract — accumulates signed neighbour-list snapshots from
//! Freenet nodes so dashboards can render a stitched view of the network.
//!
//! Wire format:
//! - `State`        : bincode of [`shared::contract::ContractState`]
//! - `StateDelta`   : bincode of [`shared::contract::ContractDelta`]
//! - `StateSummary` : bincode of [`shared::contract::ContractSummary`]
//!
//! Merge semantics: per-key Last-Writer-Wins keyed on the publisher's
//! Ed25519 public key. An entry from peer P can only replace P's previous
//! entry, and only if the timestamp is strictly newer. Cross-key
//! interference is impossible because the public key is embedded inside
//! the signed payload and the contract verifies the signature.
//!
//! Order of `update_state` deltas does not affect the final state — the
//! merge is commutative because per-key LWW is associative on the timestamp
//! ordering. This satisfies the freenet contract commutativity requirement.

use freenet_stdlib::prelude::*;
use shared::contract::{ContractDelta, ContractState, ContractSummary};

#[allow(dead_code)]
struct Topology;

#[contract]
impl ContractInterface for Topology {
    fn validate_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let parsed: ContractState = match bincode::deserialize(state.as_ref()) {
            Ok(s) => s,
            Err(_) => return Ok(ValidateResult::Invalid),
        };
        for entry in parsed.entries.values() {
            if entry.verify().is_err() {
                return Ok(ValidateResult::Invalid);
            }
        }
        Ok(ValidateResult::Valid)
    }

    fn update_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let mut current: ContractState = bincode::deserialize(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        for update in data {
            match update {
                UpdateData::Delta(d) => apply_delta_bytes(&mut current, d.as_ref())?,
                UpdateData::State(s) => {
                    let incoming: ContractState = bincode::deserialize(s.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    for (_, entry) in incoming.entries {
                        // Drop bad signatures silently; do not poison the merge.
                        let _ = current.apply(entry);
                    }
                }
                UpdateData::StateAndDelta { state: s, delta: d } => {
                    let incoming: ContractState = bincode::deserialize(s.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    for (_, entry) in incoming.entries {
                        let _ = current.apply(entry);
                    }
                    apply_delta_bytes(&mut current, d.as_ref())?;
                }
                // The `Related*` variants are not used by this contract; ignore
                // them rather than fail, since `UpdateData` is `#[non_exhaustive]`
                // and may grow new variants in the future.
                _ => {}
            }
        }

        let bytes =
            bincode::serialize(&current).map_err(|e| ContractError::Other(e.to_string()))?;
        Ok(UpdateModification::valid(State::from(bytes)))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let current: ContractState = bincode::deserialize(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary = current.summarize();
        let bytes =
            bincode::serialize(&summary).map_err(|e| ContractError::Other(e.to_string()))?;
        Ok(StateSummary::from(bytes))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let current: ContractState = bincode::deserialize(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary: ContractSummary = bincode::deserialize(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let delta = current.delta_against(&summary);
        let bytes = bincode::serialize(&delta).map_err(|e| ContractError::Other(e.to_string()))?;
        Ok(StateDelta::from(bytes))
    }
}

#[allow(dead_code)]
fn apply_delta_bytes(state: &mut ContractState, bytes: &[u8]) -> Result<(), ContractError> {
    let delta: ContractDelta =
        bincode::deserialize(bytes).map_err(|e| ContractError::Deser(e.to_string()))?;
    for entry in delta.entries {
        // Reject of a bad signature means "this peer can't speak for that
        // public key" — we silently drop rather than fail the whole update,
        // because the rest of the deltas in a multi-update batch may still
        // be valid and dropping one bad apple is the safer behaviour.
        let _ = state.apply(entry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use shared::contract::{EntryPayload, NeighborInfo, SignedEntry};

    fn sign_entry(sk: &SigningKey, ts: u64) -> SignedEntry {
        let payload = EntryPayload {
            public_key: sk.verifying_key().to_bytes(),
            external_address: "10.0.0.1:31337".into(),
            own_location: Some(0.5),
            version: Some("0.1.148".into()),
            neighbors: vec![NeighborInfo {
                address: "10.0.0.2:31337".into(),
                location: Some(0.7),
                is_gateway: true,
            }],
            contracts: vec![],
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
    fn empty_state_validates() {
        let empty = bincode::serialize(&ContractState::default()).unwrap();
        let res = Topology::validate_state(
            Parameters::from(vec![]),
            State::from(empty),
            RelatedContracts::default(),
        )
        .unwrap();
        assert_eq!(res, ValidateResult::Valid);
    }

    #[test]
    fn update_with_delta_round_trip() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let entry = sign_entry(&sk, 100);
        let delta = ContractDelta {
            entries: vec![entry],
        };
        let delta_bytes = bincode::serialize(&delta).unwrap();

        let initial = bincode::serialize(&ContractState::default()).unwrap();

        let modification = Topology::update_state(
            Parameters::from(vec![]),
            State::from(initial),
            vec![UpdateData::Delta(StateDelta::from(delta_bytes))],
        )
        .unwrap();

        let new_state_bytes = modification.new_state.unwrap();
        let parsed: ContractState = bincode::deserialize(new_state_bytes.as_ref()).unwrap();
        assert_eq!(parsed.entries.len(), 1);
    }

    #[test]
    fn order_of_deltas_does_not_matter() {
        // Commutativity check: applying deltas in any order yields the same
        // final state. This is the load-bearing property the contract trait
        // demands; without it the contract may be deprioritized.
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let entries: Vec<SignedEntry> = [50u64, 200, 100, 300, 250]
            .iter()
            .map(|ts| sign_entry(&sk, *ts))
            .collect();

        let mut a = ContractState::default();
        let mut b = ContractState::default();

        for e in &entries {
            a.apply(e.clone()).unwrap();
        }
        let mut reversed = entries.clone();
        reversed.reverse();
        for e in &reversed {
            b.apply(e.clone()).unwrap();
        }

        assert_eq!(a, b);
        let stored = a.entries.get(&sk.verifying_key().to_bytes()).unwrap();
        let p: EntryPayload = bincode::deserialize(&stored.payload).unwrap();
        assert_eq!(p.timestamp_ms, 300);
    }

    #[test]
    fn unsigned_entry_in_state_invalidates() {
        // Build a state where the signature is wrong on purpose.
        let sk = SigningKey::from_bytes(&[5u8; 32]);
        let mut entry = sign_entry(&sk, 1);
        entry.signature[0] ^= 0xff;
        let mut bad = ContractState::default();
        bad.entries.insert(sk.verifying_key().to_bytes(), entry);
        let bytes = bincode::serialize(&bad).unwrap();

        let res = Topology::validate_state(
            Parameters::from(vec![]),
            State::from(bytes),
            RelatedContracts::default(),
        )
        .unwrap();
        assert_eq!(res, ValidateResult::Invalid);
    }
}
