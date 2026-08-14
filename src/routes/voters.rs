use axum::{extract::State, Json};

use crate::{
    error::AppError,
    models::{OnChainRound, RegisterVoterRequest, RegisterVoterResponse, VoterRegistrationRow},
    routes::rounds::fetch_round,
    soroban_client::u64_,
    state::AppState,
};

/// `POST /voters/register` — relays a voter's already-signed `register`
/// transaction to the network and records the registration.
///
/// This backend never builds or signs this transaction itself: `register`
/// requires `voter.require_auth()`, a signature only the voter's own wallet
/// can produce, and this backend must never be in a position to produce it
/// on their behalf. The voter's wallet is expected to have built and
/// simulated the invocation (so the required authorization entry is
/// recorded) before signing — a standard Stellar SDK's
/// "prepare-then-sign-then-submit" flow does this automatically; skipping
/// straight from an unsimulated transaction to signing will fail
/// `require_auth` at submission time. This step *does* reveal that
/// `voter` registered (it's a real signed transaction from their address),
/// but not which ballot they later cast — see the anonvote-contracts README.
pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterVoterRequest>,
) -> Result<Json<RegisterVoterResponse>, AppError> {
    let round = fetch_round(&state, req.round_id).await?;
    if round.phase != "registration" {
        return Err(AppError::Conflict(format!(
            "round {} is not accepting registrations (phase = {})",
            req.round_id, round.phase
        )));
    }

    let relayed = state.soroban.relay_signed_xdr(&req.signed_xdr).await?;

    // `register`'s return value is `()`, so the leaf index isn't in the
    // relay's own output; read it back from the round's registered_count,
    // which is safe here because Soroban orders transactions and this read
    // happens immediately after our own submission was confirmed. Under
    // concurrent registrations from *other* callers landing in the same
    // window this is best-effort — the on-chain `Registered` event (topic
    // `("anonvote", "register")`) is the authoritative source if you need a
    // guaranteed-correct leaf index.
    let onchain: OnChainRound = state
        .soroban
        .read("get_round", &[u64_("round_id", req.round_id as u64)])
        .await?;
    let leaf_index = onchain.registered_count.saturating_sub(1) as i32;

    let row: VoterRegistrationRow = sqlx::query_as(
        r#"INSERT INTO voter_registrations (round_id, voter_address, leaf_index, commitment_hex, register_tx_hash)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (round_id, voter_address) DO UPDATE
             SET leaf_index = EXCLUDED.leaf_index,
                 commitment_hex = EXCLUDED.commitment_hex,
                 register_tx_hash = EXCLUDED.register_tx_hash
           RETURNING *"#,
    )
    .bind(req.round_id)
    .bind(&req.voter)
    .bind(leaf_index)
    .bind(&req.commitment_hex)
    .bind(relayed.tx_hash.clone().unwrap_or_default())
    .fetch_one(&state.db)
    .await?;

    Ok(Json(RegisterVoterResponse {
        round_id: req.round_id,
        voter: req.voter,
        leaf_index: row.leaf_index,
        commitment_hex: row.commitment_hex,
        tx_hash: relayed.tx_hash,
    }))
}
