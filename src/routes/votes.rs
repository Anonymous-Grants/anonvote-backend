use axum::{extract::State, Json};

use crate::{
    error::AppError,
    models::{BallotRow, CastVoteRequest, CastVoteResponse, OnChainRound},
    routes::rounds::fetch_round,
    soroban_client::{hex_bytes, json, u32_, u64_, Invoked},
    state::AppState,
};

/// `POST /votes` — casts an anonymous ballot.
///
/// `cast_vote` needs no signature at all (that's the point — see the
/// anonvote-contracts README), so this backend submits it as its own
/// operator identity purely as a fee-paying relayer; nothing about the
/// caller's HTTP connection or Stellar identity (if any) is recorded
/// anywhere, and the only two things persisted are the nullifier and the
/// choice — exactly what's already public on-chain.
///
/// The request must contain exactly one of:
/// - `proof`: an already-built Groth16 proof (the common case — generated
///   client-side, or by a separate proving service), or
/// - `witness`: the private circuit witness, for this backend to prove
///   server-side via `proof_service` (requires `BB_PATH` to be configured;
///   see the README).
pub async fn cast_vote(
    State(state): State<AppState>,
    Json(req): Json<CastVoteRequest>,
) -> Result<Json<CastVoteResponse>, AppError> {
    let round = fetch_round(&state, req.round_id).await?;
    if round.phase != "voting" {
        return Err(AppError::Conflict(format!(
            "round {} is not open for voting (phase = {})",
            req.round_id, round.phase
        )));
    }
    if req.choice < 0 || req.choice >= round.num_choices {
        return Err(AppError::BadRequest(format!(
            "choice must be in 0..{}",
            round.num_choices
        )));
    }

    let proof = match (&req.proof, &req.witness) {
        (Some(p), None) => p.clone(),
        (None, Some(witness)) => {
            let expected_nullifier = state
                .proof_service
                .compute_nullifier(&witness.secret, req.round_id as u64)?;
            if !hex_eq(&expected_nullifier, &req.nullifier_hex) {
                return Err(AppError::BadRequest(
                    "nullifier_hex does not match Poseidon2(secret, round_id) for the given witness"
                        .to_string(),
                ));
            }

            let onchain: OnChainRound = state
                .soroban
                .read("get_round", &[u64_("round_id", req.round_id as u64)])
                .await?;
            state
                .proof_service
                .prove(
                    witness,
                    &onchain.merkle_root,
                    &req.nullifier_hex,
                    req.round_id as u64,
                    req.choice as u32,
                )
                .await?
        }
        (Some(_), Some(_)) => {
            return Err(AppError::BadRequest(
                "provide only one of `proof` or `witness`, not both".to_string(),
            ))
        }
        (None, None) => {
            return Err(AppError::BadRequest(
                "one of `proof` or `witness` is required".to_string(),
            ))
        }
    };

    let Invoked { tx_hash, .. } = state
        .soroban
        .invoke_as_operator::<()>(
            "cast_vote",
            &[
                u64_("round_id", req.round_id as u64),
                u32_("choice", req.choice as u32),
                hex_bytes("nullifier", &req.nullifier_hex),
                json("proof", &proof),
            ],
        )
        .await?;

    // The chain is the actual source of truth for double-vote rejection
    // (cast_vote itself just failed above with NullifierAlreadyUsed if this
    // were a replay); this insert is a public mirror of the ballot log for
    // fast reads, not a rejection mechanism.
    let row: BallotRow = sqlx::query_as(
        r#"INSERT INTO ballots (round_id, nullifier_hex, choice_index, cast_vote_tx_hash)
           VALUES ($1, $2, $3, $4)
           RETURNING *"#,
    )
    .bind(req.round_id)
    .bind(&req.nullifier_hex)
    .bind(req.choice)
    .bind(tx_hash.clone().unwrap_or_default())
    .fetch_one(&state.db)
    .await?;

    Ok(Json(CastVoteResponse {
        round_id: req.round_id,
        choice: row.choice_index,
        nullifier_hex: row.nullifier_hex,
        tx_hash,
    }))
}

fn hex_eq(a: &str, b: &str) -> bool {
    a.trim_start_matches("0x").eq_ignore_ascii_case(b.trim_start_matches("0x"))
}
