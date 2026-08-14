//! Orchestrates zero-knowledge proof generation for the Noir circuit in
//! `anonvote-contracts/circuits/anonvote` ("I'm a registered voter and this
//! is my first vote in this round" — see that repo's README for the
//! statement being proved).
//!
//! Two independently useful pieces live here:
//!
//! - **Poseidon2 hashing** (`compute_leaf`, `compute_nullifier`), via
//!   `soroban-sdk` + `soroban-poseidon` run natively (no WASM/network
//!   involved — `Env::default()` runs the real host implementation outside a
//!   contract too). This is the *exact* same hash construction the deployed
//!   contract and the circuit both use, reused here rather than
//!   reimplemented, so a nullifier/commitment this service computes is
//!   guaranteed bit-identical to what the contract will check on-chain.
//! - **Witness generation + proving** (`prove`), which shells out to
//!   `nargo execute` (verified against this exact circuit while building
//!   this module — see the backend README) to solve the circuit and produce
//!   a witness, then to a configurable Barretenberg-compatible `bb` binary
//!   to turn that witness into a Groth16 proof. The `bb` step is disabled
//!   (returns `ProofServiceError::Unavailable`) unless `BB_PATH` is
//!   configured and resolves to a real binary — most deployments of this
//!   backend will have callers submit an already-built `proof` to `POST
//!   /votes` instead (see the README's "casting a vote" section) and never
//!   need this path.
use std::path::{Path, PathBuf};

use serde::Serialize;
use soroban_sdk::{crypto::bn254::Bn254Fr, vec as sdk_vec, BytesN, Env, U256};
use soroban_poseidon::Poseidon2Sponge;
use tokio::process::Command;

use crate::models::{ProofDto, ProveWitnessRequest};

const POSEIDON2_T: u32 = 4;
const MERKLE_DEPTH: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum ProofServiceError {
    #[error("{0}")]
    InvalidInput(String),

    #[error(
        "server-side proof generation is not configured on this backend (no BB_PATH); \
         submit an already-built `proof` instead"
    )]
    Unavailable(String),

    #[error("failed to run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("witness generation failed:\n{stderr}")]
    WitnessGeneration { stderr: String },

    #[error("proving failed:\n{stderr}")]
    Proving { stderr: String },

    #[error("filesystem error while preparing the circuit workspace: {0}")]
    Io(#[from] std::io::Error),
}

pub struct ProofService {
    circuit_dir: PathBuf,
    nargo_path: String,
    bb_path: Option<String>,
}

impl ProofService {
    pub fn new(circuit_dir: PathBuf, nargo_path: String, bb_path: Option<String>) -> Self {
        Self {
            circuit_dir,
            nargo_path,
            bb_path,
        }
    }

    pub fn is_proving_available(&self) -> bool {
        self.bb_path.is_some()
    }

    /// `Poseidon2::hash([secret], 1)` — the leaf a `register` commitment
    /// must equal, matching `contracts/anonvote/src/merkle.rs` and
    /// `circuits/anonvote/src/main.nr`'s `leaf` computation exactly. Exposed
    /// for callers building tooling around this backend (e.g. to sanity
    /// check a commitment before registering); not used by any route here.
    #[allow(dead_code)]
    pub fn compute_leaf(&self, secret_hex: &str) -> Result<String, ProofServiceError> {
        let env = Env::default();
        let secret = hex_to_u256(&env, secret_hex)?;
        Ok(u256_to_hex(&Self::poseidon2(&env, &[secret])))
    }

    /// `Poseidon2::hash([secret, round_id], 2)` — what `cast_vote`'s
    /// `nullifier` argument must equal for a given `secret` and round.
    pub fn compute_nullifier(&self, secret_hex: &str, round_id: u64) -> Result<String, ProofServiceError> {
        let env = Env::default();
        let secret = hex_to_u256(&env, secret_hex)?;
        let round_id = U256::from_u128(&env, round_id as u128);
        Ok(u256_to_hex(&Self::poseidon2(&env, &[secret, round_id])))
    }

    /// `Env` (via `soroban-env-host`) is not `Send`/`Sync`, so it's created
    /// fresh per call rather than stored on `ProofService` — this type is
    /// held in shared `axum::State`, which must be. Each call is
    /// independent; nothing needs to persist across `Env` instances.
    fn poseidon2(env: &Env, inputs: &[U256]) -> U256 {
        let mut sponge = Poseidon2Sponge::<POSEIDON2_T, Bn254Fr>::new(env);
        let mut v = sdk_vec![env];
        for i in inputs {
            v.push_back(i.clone());
        }
        sponge.compute_hash(&v)
    }

    /// Generates the circuit witness for `witness`/`public`, then (if
    /// `BB_PATH` is configured) proves it, returning a `ProofDto` ready to
    /// hand to `SorobanClient` as `cast_vote`'s `proof` argument.
    pub async fn prove(
        &self,
        witness: &ProveWitnessRequest,
        merkle_root_hex: &str,
        nullifier_hex: &str,
        round_id: u64,
        choice: u32,
    ) -> Result<ProofDto, ProofServiceError> {
        let Some(bb_path) = &self.bb_path else {
            return Err(ProofServiceError::Unavailable(
                "BB_PATH is not configured on this backend".to_string(),
            ));
        };

        if witness.merkle_path.len() != MERKLE_DEPTH || witness.path_indices.len() != MERKLE_DEPTH {
            return Err(ProofServiceError::InvalidInput(format!(
                "merkle_path and path_indices must both have exactly {MERKLE_DEPTH} entries"
            )));
        }

        let workspace = tempfile::tempdir()?;
        copy_circuit_package(&self.circuit_dir, workspace.path())?;
        write_prover_toml(
            workspace.path(),
            witness,
            merkle_root_hex,
            nullifier_hex,
            round_id,
            choice,
        )?;

        let witness_name = "witness";
        let output = Command::new(&self.nargo_path)
            .arg("execute")
            .arg(witness_name)
            .arg("--prover-name")
            .arg("Prover")
            .current_dir(workspace.path())
            .output()
            .await
            .map_err(|source| ProofServiceError::Spawn {
                program: self.nargo_path.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(ProofServiceError::WitnessGeneration {
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let witness_path = workspace.path().join("target").join(format!("{witness_name}.gz"));
        let circuit_path = workspace.path().join("target").join("anonvote.json");
        let proof_dir = workspace.path().join("proof");
        std::fs::create_dir_all(&proof_dir)?;

        // Barretenberg CLI syntax varies by `bb` version; this backend
        // treats the invocation as configuration rather than hard-coding
        // one version's flags. See BB_PROVE_ARGS in `.env.example`.
        let bb_args = bb_prove_args(&circuit_path, &witness_path, &proof_dir);
        let output = Command::new(bb_path)
            .args(&bb_args)
            .output()
            .await
            .map_err(|source| ProofServiceError::Spawn {
                program: bb_path.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(ProofServiceError::Proving {
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        read_groth16_proof(&proof_dir)
    }
}

fn bb_prove_args(circuit_path: &Path, witness_path: &Path, out_dir: &Path) -> Vec<String> {
    std::env::var("BB_PROVE_ARGS")
        .ok()
        .map(|template| {
            template
                .split_whitespace()
                .map(|s| {
                    s.replace("{circuit}", &circuit_path.to_string_lossy())
                        .replace("{witness}", &witness_path.to_string_lossy())
                        .replace("{out}", &out_dir.to_string_lossy())
                })
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "prove".to_string(),
                "--scheme".to_string(),
                "groth16".to_string(),
                "-b".to_string(),
                circuit_path.to_string_lossy().into_owned(),
                "-w".to_string(),
                witness_path.to_string_lossy().into_owned(),
                "-o".to_string(),
                out_dir.to_string_lossy().into_owned(),
            ]
        })
}

/// Reads `bb`'s proof output directory. Expects `proof_fields.json` (an
/// array of hex field elements: `[a.x, a.y, b.x.c0, b.x.c1, b.y.c0, b.y.c1,
/// c.x, c.y]`, the layout `bb`'s Groth16 output uses) and packs it into the
/// `BytesN<64>`/`BytesN<128>`/`BytesN<64>` hex layout `verify_groth16`
/// expects. Adjust here if your `bb` version's output layout differs.
fn read_groth16_proof(proof_dir: &Path) -> Result<ProofDto, ProofServiceError> {
    let fields_path = proof_dir.join("proof_fields.json");
    let raw = std::fs::read_to_string(&fields_path).map_err(|source| ProofServiceError::Spawn {
        program: format!("read {}", fields_path.display()),
        source,
    })?;
    let fields: Vec<String> = serde_json::from_str(&raw).map_err(|e| {
        ProofServiceError::InvalidInput(format!("could not parse {}: {e}", fields_path.display()))
    })?;
    if fields.len() != 8 {
        return Err(ProofServiceError::InvalidInput(format!(
            "expected 8 field elements in {}, found {}",
            fields_path.display(),
            fields.len()
        )));
    }
    let f = |i: usize| fields[i].trim_start_matches("0x").to_string();
    let pad64 = |hex: String| format!("{hex:0>64}");

    Ok(ProofDto {
        a: format!("{}{}", pad64(f(0)), pad64(f(1))),
        b: format!("{}{}{}{}", pad64(f(3)), pad64(f(2)), pad64(f(5)), pad64(f(4))),
        c: format!("{}{}", pad64(f(6)), pad64(f(7))),
    })
}

fn copy_circuit_package(src: &Path, dst: &Path) -> Result<(), ProofServiceError> {
    std::fs::copy(src.join("Nargo.toml"), dst.join("Nargo.toml"))?;
    copy_dir_recursive(&src.join("src"), &dst.join("src"))?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ProverToml<'a> {
    secret: &'a str,
    merkle_path: &'a [String],
    path_indices: &'a [bool],
    merkle_root: &'a str,
    nullifier: &'a str,
    round_id: String,
    choice: String,
}

fn write_prover_toml(
    dir: &Path,
    witness: &ProveWitnessRequest,
    merkle_root_hex: &str,
    nullifier_hex: &str,
    round_id: u64,
    choice: u32,
) -> Result<(), ProofServiceError> {
    let doc = ProverToml {
        secret: &witness.secret,
        merkle_path: &witness.merkle_path,
        path_indices: &witness.path_indices,
        merkle_root: merkle_root_hex,
        nullifier: nullifier_hex,
        round_id: round_id.to_string(),
        choice: choice.to_string(),
    };
    let toml = toml_string(&doc);
    std::fs::write(dir.join("Prover.toml"), toml)?;
    Ok(())
}

/// Hand-rolled instead of pulling in the `toml` crate: Noir's `Prover.toml`
/// only ever needs flat scalars, bools, and arrays of those, which is
/// simple enough to emit directly.
fn toml_string(doc: &ProverToml) -> String {
    let path = doc
        .merkle_path
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let indices = doc
        .path_indices
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "secret = \"{secret}\"\nmerkle_path = [{path}]\npath_indices = [{indices}]\nmerkle_root = \"{root}\"\nnullifier = \"{nullifier}\"\nround_id = \"{round_id}\"\nchoice = \"{choice}\"\n",
        secret = doc.secret,
        root = doc.merkle_root,
        nullifier = doc.nullifier,
        round_id = doc.round_id,
        choice = doc.choice,
    )
}

fn hex_to_u256(env: &Env, hex: &str) -> Result<U256, ProofServiceError> {
    let clean = hex.trim_start_matches("0x");
    let mut bytes = hex::decode(clean)
        .map_err(|e| ProofServiceError::InvalidInput(format!("invalid hex field element: {e}")))?;
    if bytes.len() > 32 {
        return Err(ProofServiceError::InvalidInput(
            "field element must be at most 32 bytes".to_string(),
        ));
    }
    let mut padded = vec![0u8; 32 - bytes.len()];
    padded.append(&mut bytes);
    let arr: [u8; 32] = padded.try_into().expect("padded to 32 bytes");
    Ok(U256::from_be_bytes(env, &BytesN::from_array(env, &arr).into()))
}

fn u256_to_hex(v: &U256) -> String {
    let bytes: soroban_sdk::Bytes = v.to_be_bytes();
    let mut out = [0u8; 32];
    for (i, b) in bytes.into_iter().enumerate() {
        out[i] = b;
    }
    format!("0x{}", hex::encode(out))
}
