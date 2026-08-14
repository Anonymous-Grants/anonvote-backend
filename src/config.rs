use std::path::PathBuf;

/// All runtime configuration, loaded once from the environment (and `.env`
/// if present) at startup. See `.env.example` for the full list with
/// descriptions.
#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,

    /// The AnonVote contract's Soroban address (C...).
    pub contract_id: String,
    /// `stellar keys` identity alias (or raw secret key / G... address the
    /// CLI can resolve) used as the transaction source for every
    /// operator-signed call: `create_round`, `set_eligible`, `open_voting`,
    /// `cast_vote` (relaying a voter's proof — cast_vote itself needs no
    /// voter signature), and `finalize_round`. This address becomes
    /// `round.admin` for every round this backend creates.
    pub operator_identity: String,
    /// Named network the `stellar` CLI already knows about (`testnet`,
    /// `futurenet`, `mainnet`, or a custom name configured via
    /// `stellar network add`). Mutually exclusive with
    /// `stellar_rpc_url`/`stellar_network_passphrase`.
    pub stellar_network: Option<String>,
    pub stellar_rpc_url: Option<String>,
    pub stellar_network_passphrase: Option<String>,
    /// Path to the `stellar` CLI binary. Defaults to `stellar` (resolved via
    /// `PATH`).
    pub stellar_cli_path: String,

    /// Path to the Noir circuit package directory (containing `Nargo.toml`
    /// and `src/main.nr`) used for server-side proof generation. Defaults to
    /// `../anonvote-contracts/circuits/anonvote` relative to the working
    /// directory, matching this repo's sibling layout.
    pub circuit_dir: PathBuf,
    pub nargo_path: String,
    /// Path to a Barretenberg-compatible prover CLI. If unset or the binary
    /// can't be found, server-side proof generation (`POST /votes` with a
    /// `witness` body instead of a pre-built `proof`) is disabled and
    /// returns 503 — submitting an already-built proof still works.
    pub bb_path: Option<String>,

    /// Treasury identity used as the payment source in `POST
    /// /payouts/execute`. Defaults to `operator_identity`.
    pub treasury_identity: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();

        let database_url = require("DATABASE_URL")?;
        let bind_addr = env_or("BIND_ADDR", "0.0.0.0:8080");

        let contract_id = require("ANONVOTE_CONTRACT_ID")?;
        let operator_identity = require("STELLAR_OPERATOR_IDENTITY")?;
        let stellar_network = std::env::var("STELLAR_NETWORK").ok();
        let stellar_rpc_url = std::env::var("STELLAR_RPC_URL").ok();
        let stellar_network_passphrase = std::env::var("STELLAR_NETWORK_PASSPHRASE").ok();
        if stellar_network.is_none() && (stellar_rpc_url.is_none() || stellar_network_passphrase.is_none()) {
            anyhow::bail!(
                "set STELLAR_NETWORK (e.g. \"testnet\") or both STELLAR_RPC_URL and \
                 STELLAR_NETWORK_PASSPHRASE"
            );
        }
        let stellar_cli_path = env_or("STELLAR_CLI_PATH", "stellar");

        let circuit_dir = PathBuf::from(env_or(
            "ANONVOTE_CIRCUIT_DIR",
            "../anonvote-contracts/circuits/anonvote",
        ));
        let nargo_path = env_or("NARGO_PATH", "nargo");
        let bb_path = std::env::var("BB_PATH").ok().filter(|p| !p.is_empty());

        let treasury_identity = std::env::var("STELLAR_TREASURY_IDENTITY")
            .unwrap_or_else(|_| operator_identity.clone());

        Ok(Self {
            database_url,
            bind_addr,
            contract_id,
            operator_identity,
            stellar_network,
            stellar_rpc_url,
            stellar_network_passphrase,
            stellar_cli_path,
            circuit_dir,
            nargo_path,
            bb_path,
            treasury_identity,
        })
    }
}

fn require(key: &str) -> anyhow::Result<String> {
    std::env::var(key).map_err(|_| anyhow::anyhow!("missing required env var {key}"))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
