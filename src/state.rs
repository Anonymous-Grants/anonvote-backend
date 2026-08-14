use std::sync::Arc;

use sqlx::PgPool;

use crate::{config::Config, proof_service::ProofService, soroban_client::SorobanClient};

pub struct AppStateInner {
    pub db: PgPool,
    pub soroban: SorobanClient,
    pub proof_service: ProofService,
    pub config: Config,
    /// The `G...` address `config.operator_identity` resolves to, resolved
    /// once at startup. This is `round.admin` for every round this backend
    /// creates.
    pub operator_address: String,
}

pub type AppState = Arc<AppStateInner>;
