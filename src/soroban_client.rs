//! Soroban integration: everything that talks to the AnonVote contract on a
//! live network.
//!
//! Rather than hand-rolling Soroban transaction XDR (account sequence
//! numbers, fee/resource-footprint bookkeeping, `SorobanAuthorizationEntry`
//! signing) in this process, this module drives the official `stellar` CLI
//! as a subprocess. The CLI already does all of that correctly, and its
//! per-function "implicit CLI" (generated from the contract's embedded spec)
//! gives a stable, scriptable contract: JSON on stdout for the decoded
//! return value, human-readable progress/event logs on stderr, and a
//! non-zero exit code plus an `Error(Contract, #N)` line on a rejected call.
//! That contract was confirmed directly against a real testnet deployment of
//! this contract while building this module (see the backend README).
//!
//! Two call shapes are used:
//! - **operator-signed** (`invoke_as_operator`): this backend's own identity
//!   is the transaction source and signer. Used for `create_round`,
//!   `set_eligible`, `open_voting`, `finalize_round` (all of which need
//!   `round.admin.require_auth()` — satisfied because this backend's
//!   operator identity *is* `round.admin` for every round it creates), and
//!   `cast_vote` (which needs no auth at all — the whole point of the ZK
//!   design — so the operator is just the fee-paying relayer).
//! - **relay** (`relay_signed_xdr`): a transaction someone else already
//!   built and signed (a voter's wallet, for `register`, which needs
//!   `voter.require_auth()` — a signature this backend must never be able to
//!   produce on a voter's behalf). This backend only forwards it to the
//!   network.
use std::process::Stdio;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::de::DeserializeOwned;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum SorobanError {
    #[error("failed to launch `{cli}`: {source}")]
    Spawn {
        cli: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the AnonVote contract rejected this call: {name} (#{code})")]
    Contract { code: u32, name: &'static str },
    #[error("stellar CLI exited with status {status}: {stderr}")]
    Cli { status: i32, stderr: String },
    #[error("could not parse CLI output as {expected}: {source}\noutput was: {output}")]
    Decode {
        expected: &'static str,
        output: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("submitted transaction {tx_hash} but it did not succeed: {detail}")]
    TransactionFailed { tx_hash: String, detail: String },
}

/// Mirrors `anonvote::Error` in `anonvote-contracts/contracts/anonvote/src/errors.rs`.
/// Kept as a plain duplicate (rather than a path dependency on that crate) so
/// this backend stays independently buildable; update both if the contract's
/// error codes ever change.
fn contract_error_name(code: u32) -> &'static str {
    match code {
        1 => "AlreadyInitialized",
        2 => "NotInitialized",
        3 => "NotAdmin",
        4 => "RoundNotFound",
        5 => "RegistrationSetFull",
        6 => "AlreadyRegistered",
        7 => "RegistrationClosed",
        8 => "VotingNotOpen",
        9 => "VotingClosed",
        10 => "InvalidChoice",
        11 => "InvalidProof",
        12 => "NullifierAlreadyUsed",
        13 => "InvalidPhaseTransition",
        14 => "RoundAlreadyFinalized",
        15 => "InvalidVerifyingKey",
        _ => "Unknown",
    }
}

static CONTRACT_ERROR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"Error\(Contract,\s*#(\d+)\)").unwrap());
static TX_HASH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:Signing transaction:|explorer/\w+/tx/)\s*([0-9a-fA-F]{64})").unwrap());

fn find_contract_error(stderr: &str) -> Option<SorobanError> {
    CONTRACT_ERROR_RE.captures(stderr).map(|c| {
        let code: u32 = c[1].parse().unwrap_or(0);
        SorobanError::Contract {
            code,
            name: contract_error_name(code),
        }
    })
}

fn find_tx_hash(stderr: &str) -> Option<String> {
    TX_HASH_RE.captures(stderr).map(|c| c[1].to_string())
}

/// A `--name value` pair for the contract's implicit CLI. Use the
/// constructors below rather than building these by hand so encoding stays
/// consistent (hex for `Bytes`/`BytesN`, raw strkey for `Address`, JSON for
/// structs).
pub struct Arg(pub String, pub String);

pub fn addr(name: &str, value: &str) -> Arg {
    Arg(name.to_string(), value.to_string())
}
pub fn u64_(name: &str, value: u64) -> Arg {
    Arg(name.to_string(), value.to_string())
}
pub fn u32_(name: &str, value: u32) -> Arg {
    Arg(name.to_string(), value.to_string())
}
pub fn bool_(name: &str, value: bool) -> Arg {
    Arg(name.to_string(), value.to_string())
}
pub fn text(name: &str, value: &str) -> Arg {
    Arg(name.to_string(), value.to_string())
}
/// `Bytes`/`BytesN` fields take plain lowercase hex, no `0x` prefix.
pub fn hex_bytes(name: &str, value: &str) -> Arg {
    Arg(name.to_string(), value.trim_start_matches("0x").to_lowercase())
}
/// Struct/tuple-typed fields take a JSON object whose string fields are
/// themselves hex per `hex_bytes` — build it with `serde_json::json!` /
/// `serde_json::to_string` and pass the result here.
pub fn json(name: &str, value: impl serde::Serialize) -> Arg {
    Arg(name.to_string(), serde_json::to_string(&value).expect("serializable arg"))
}

/// Result of an operator-signed call that changed contract state.
pub struct Invoked<T> {
    pub value: T,
    pub tx_hash: Option<String>,
}

/// Result of relaying an already-signed transaction.
pub struct Relayed {
    pub tx_hash: Option<String>,
}

#[derive(Clone)]
pub struct SorobanClient {
    cli_path: String,
    contract_id: String,
    operator_identity: String,
    network_args: Vec<String>,
}

impl SorobanClient {
    pub fn new(cfg: &Config) -> Self {
        let mut network_args = Vec::new();
        if let Some(network) = &cfg.stellar_network {
            network_args.push("--network".to_string());
            network_args.push(network.clone());
        } else {
            network_args.push("--rpc-url".to_string());
            network_args.push(cfg.stellar_rpc_url.clone().unwrap());
            network_args.push("--network-passphrase".to_string());
            network_args.push(cfg.stellar_network_passphrase.clone().unwrap());
        }
        Self {
            cli_path: cfg.stellar_cli_path.clone(),
            contract_id: cfg.contract_id.clone(),
            operator_identity: cfg.operator_identity.clone(),
            network_args,
        }
    }

    /// A read-only contract call (`tally`, `get_round`, `ballots`, ...).
    /// Simulated only (`--send=no`): no fee, no ledger write, no
    /// transaction ever hits the network.
    pub async fn read<T: DeserializeOwned>(
        &self,
        function: &str,
        args: &[Arg],
    ) -> Result<T, SorobanError> {
        let mut cli_args = self.invoke_prelude(&self.operator_identity);
        cli_args.push("--send".to_string());
        cli_args.push("no".to_string());
        cli_args.push("--".to_string());
        cli_args.push(function.to_string());
        push_args(&mut cli_args, args);

        let output = self.run(&cli_args).await?;
        decode(&output.stdout, "read result")
    }

    /// An operator-signed state-changing call. See the module doc for which
    /// contract functions this is valid for.
    pub async fn invoke_as_operator<T: DeserializeOwned>(
        &self,
        function: &str,
        args: &[Arg],
    ) -> Result<Invoked<T>, SorobanError> {
        let mut cli_args = self.invoke_prelude(&self.operator_identity);
        cli_args.push("--".to_string());
        cli_args.push(function.to_string());
        push_args(&mut cli_args, args);

        let output = self.run(&cli_args).await?;
        let value = decode(&output.stdout, "invoke result")?;
        Ok(Invoked {
            value,
            tx_hash: find_tx_hash(&output.stderr),
        })
    }

    /// Submits a transaction someone else has already built and signed
    /// (base64 XDR), for calls that need a signature this backend cannot
    /// produce (e.g. a voter's `register`). This backend never sees, and
    /// therefore never needs to hold, the signer's key.
    pub async fn relay_signed_xdr(&self, signed_xdr: &str) -> Result<Relayed, SorobanError> {
        let mut cli_args = vec!["tx".to_string(), "send".to_string()];
        cli_args.extend(self.network_args.clone());

        let mut child = Command::new(&self.cli_path)
            .args(&cli_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| SorobanError::Spawn {
                cli: self.cli_path.clone(),
                source,
            })?;

        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(signed_xdr.trim().as_bytes())
            .await
            .map_err(|source| SorobanError::Spawn {
                cli: self.cli_path.clone(),
                source,
            })?;

        let output = child.wait_with_output().await.map_err(|source| SorobanError::Spawn {
            cli: self.cli_path.clone(),
            source,
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            if let Some(err) = find_contract_error(&stderr) {
                return Err(err);
            }
            let tx_hash = find_tx_hash(&stderr).unwrap_or_default();
            return Err(SorobanError::TransactionFailed {
                tx_hash,
                detail: stderr,
            });
        }

        Ok(Relayed {
            tx_hash: find_tx_hash(&stderr).or_else(|| find_tx_hash(&stdout)),
        })
    }

    /// Sends a plain classic-Stellar payment (not a contract call) from
    /// `source_identity` to `destination`. Used by `POST /payouts/execute`
    /// to pay out each proposal's proportional share once a round is
    /// finalized.
    pub async fn send_payment(
        &self,
        source_identity: &str,
        destination: &str,
        amount_stroops: i64,
    ) -> Result<Option<String>, SorobanError> {
        let mut args = vec![
            "tx".to_string(),
            "new".to_string(),
            "payment".to_string(),
            "--source-account".to_string(),
            source_identity.to_string(),
            "--destination".to_string(),
            destination.to_string(),
            "--amount".to_string(),
            amount_stroops.to_string(),
        ];
        args.extend(self.network_args.clone());
        let output = self.run(&args).await?;
        Ok(find_tx_hash(&output.stderr))
    }

    /// Resolves a `stellar keys` identity alias (or already-a-strkey value)
    /// to its `G...` address. Used once at startup to learn the operator's
    /// real address for `round.admin`/DB bookkeeping.
    pub async fn resolve_address(cli_path: &str, identity: &str) -> Result<String, SorobanError> {
        let output = Command::new(cli_path)
            .args(["keys", "address", identity])
            .output()
            .await
            .map_err(|source| SorobanError::Spawn {
                cli: cli_path.to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(SorobanError::Cli {
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn invoke_prelude(&self, source_account: &str) -> Vec<String> {
        let mut args = vec![
            "contract".to_string(),
            "invoke".to_string(),
            "--id".to_string(),
            self.contract_id.clone(),
            "--source-account".to_string(),
            source_account.to_string(),
        ];
        args.extend(self.network_args.clone());
        args
    }

    async fn run(&self, args: &[String]) -> Result<RawOutput, SorobanError> {
        let output = Command::new(&self.cli_path)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| SorobanError::Spawn {
                cli: self.cli_path.clone(),
                source,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if !output.status.success() {
            if let Some(err) = find_contract_error(&stderr) {
                return Err(err);
            }
            return Err(SorobanError::Cli {
                status: output.status.code().unwrap_or(-1),
                stderr,
            });
        }

        Ok(RawOutput { stdout, stderr })
    }
}

struct RawOutput {
    stdout: String,
    stderr: String,
}

fn push_args(cli_args: &mut Vec<String>, args: &[Arg]) {
    for Arg(name, value) in args {
        cli_args.push(format!("--{name}"));
        cli_args.push(value.clone());
    }
}

fn decode<T: DeserializeOwned>(raw_stdout: &str, expected: &'static str) -> Result<T, SorobanError> {
    let trimmed = raw_stdout.trim();
    serde_json::from_str(trimmed).map_err(|source| SorobanError::Decode {
        expected,
        output: trimmed.to_string(),
        source,
    })
}
