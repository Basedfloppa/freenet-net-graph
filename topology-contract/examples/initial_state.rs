//! One-shot helper that writes a bincode-serialised empty `ContractState`
//! to `initial-state.bin` next to the working directory. Used as the
//! `--state` argument when publishing the topology contract via fdev:
//!
//! ```bash
//! cargo run --example initial_state
//! cargo run -p fdev -- publish \
//!     --code <packaged.wasm> \
//!     contract --state initial-state.bin
//! ```
//!
//! Run from `topology-contract/` so the output ends up in the same dir
//! as `freenet.toml`.

use shared::contract::ContractState;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = ContractState::default();
    let bytes = bincode::serialize(&state)?;
    let out = std::env::current_dir()?.join("initial-state.bin");
    std::fs::write(&out, &bytes)?;
    println!(
        "wrote {} bytes to {}",
        bytes.len(),
        PathBuf::from(out).display()
    );
    Ok(())
}
