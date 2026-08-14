use axum::{extract::Path, extract::State, Json};

use crate::{
    error::AppError,
    models::{
        ChoiceTally, CreateRoundRequest, CreateRoundResponse, EligibilityFailure, OnChainRound,
        ProposalRow, RoundRow, SetEligibilityRequest, SetEligibilityResponse, TallyResponse,
    },
    soroban_client::{addr, bool_, text, u32_, u64_, Invoked},
    state::AppState,
};

/// `POST /rounds` — creates a round on-chain (`create_round`, admin =
/// this backend's operator identity) and records the round plus its
/// proposals in Postgres in one transaction. `proposals[i].payout_address`
/// is where that proposal's share of `payout_pool_stroops` goes once
/// `POST /payouts/execute` runs after the round is finalized.
pub async fn create_round(
    State(state): State<AppState>,
    Json(req): Json<CreateRoundRequest>,
) -> Result<Json<CreateRoundResponse>, AppError> {
    if req.proposals.is_empty() {
        return Err(AppError::BadRequest(
            "at least one proposal is required".to_string(),
        ));
    }
    if req.payout_pool_stroops < 0 {
        return Err(AppError::BadRequest(
            "payout_pool_stroops must not be negative".to_string(),
        ));
    }
    let num_choices = req.proposals.len() as u32;

    let Invoked { value: round_id, tx_hash } = state
        .soroban
        .invoke_as_operator::<u64>(
            "create_round",
            &[
                addr("admin", &state.operator_address),
                text("title", &req.title),
                u32_("num_choices", num_choices),
            ],
        )
        .await?;

    let mut tx = state.db.begin().await?;
    let round: RoundRow = sqlx::query_as(
        r#"INSERT INTO rounds (id, contract_id, admin, title, num_choices, payout_pool_stroops, create_round_tx_hash)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING *"#,
    )
    .bind(round_id as i64)
    .bind(&state.config.contract_id)
    .bind(&state.operator_address)
    .bind(&req.title)
    .bind(num_choices as i32)
    .bind(req.payout_pool_stroops)
    .bind(&tx_hash)
    .fetch_one(&mut *tx)
    .await?;

    let mut proposals = Vec::with_capacity(req.proposals.len());
    for (i, p) in req.proposals.iter().enumerate() {
        let row: ProposalRow = sqlx::query_as(
            r#"INSERT INTO proposals (round_id, choice_index, title, description, payout_address)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING *"#,
        )
        .bind(round_id as i64)
        .bind(i as i32)
        .bind(&p.title)
        .bind(&p.description)
        .bind(&p.payout_address)
        .fetch_one(&mut *tx)
        .await?;
        proposals.push(row);
    }
    tx.commit().await?;

    Ok(Json(CreateRoundResponse { round, proposals }))
}

/// `GET /rounds/{id}` — round + proposal metadata from this backend's own
/// bookkeeping (fast, no network round-trip). For live on-chain state use
/// `GET /rounds/{id}/tally`.
pub async fn get_round(
    State(state): State<AppState>,
    Path(round_id): Path<i64>,
) -> Result<Json<CreateRoundResponse>, AppError> {
    let round = fetch_round(&state, round_id).await?;
    let proposals = fetch_proposals(&state, round_id).await?;
    Ok(Json(CreateRoundResponse { round, proposals }))
}

/// `POST /rounds/{id}/eligibility` — marks each of `voters` eligible (or
/// ineligible) to register, via the contract's `set_eligible`. This is the
/// round's Sybil-resistance hook (see the anonvote-contracts README): plug
/// in whatever eligibility list the round already uses upstream of this
/// call (a badgeholder registry, a hackathon judge roster, ...).
pub async fn set_eligibility(
    State(state): State<AppState>,
    Path(round_id): Path<i64>,
    Json(req): Json<SetEligibilityRequest>,
) -> Result<Json<SetEligibilityResponse>, AppError> {
    fetch_round(&state, round_id).await?;

    let mut updated = Vec::new();
    let mut failed = Vec::new();
    for voter in &req.voters {
        let result = state
            .soroban
            .invoke_as_operator::<()>(
                "set_eligible",
                &[
                    u64_("round_id", round_id as u64),
                    addr("voter", voter),
                    bool_("eligible", req.eligible),
                ],
            )
            .await;
        match result {
            Ok(_) => updated.push(voter.clone()),
            Err(e) => failed.push(EligibilityFailure {
                voter: voter.clone(),
                error: e.to_string(),
            }),
        }
    }

    Ok(Json(SetEligibilityResponse {
        round_id,
        updated,
        failed,
    }))
}

/// `POST /rounds/{id}/open` — closes registration and opens voting
/// (`open_voting`). From this point the round's Merkle root is frozen, so
/// every vote's proof is checked against the same anonymity set.
pub async fn open_voting(
    State(state): State<AppState>,
    Path(round_id): Path<i64>,
) -> Result<Json<RoundRow>, AppError> {
    state
        .soroban
        .invoke_as_operator::<()>("open_voting", &[u64_("round_id", round_id as u64)])
        .await?;

    let round: RoundRow = sqlx::query_as(
        "UPDATE rounds SET phase = 'voting', voting_opened_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(round_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("round {round_id}")))?;

    Ok(Json(round))
}

/// `POST /rounds/{id}/finalize` — closes voting (`finalize_round`) and
/// returns the final tally. Required before `POST /payouts/execute` will
/// run for this round.
pub async fn finalize_round(
    State(state): State<AppState>,
    Path(round_id): Path<i64>,
) -> Result<Json<TallyResponse>, AppError> {
    let Invoked { value: votes, .. } = state
        .soroban
        .invoke_as_operator::<Vec<u64>>("finalize_round", &[u64_("round_id", round_id as u64)])
        .await?;

    sqlx::query("UPDATE rounds SET phase = 'finalized', finalized_at = now() WHERE id = $1")
        .bind(round_id)
        .execute(&state.db)
        .await?;

    let onchain: OnChainRound = state
        .soroban
        .read("get_round", &[u64_("round_id", round_id as u64)])
        .await?;
    let proposals = fetch_proposals(&state, round_id).await?;
    let choices = zip_tally(&proposals, &votes);
    let total_votes = choices.iter().map(|c| c.votes).sum();

    Ok(Json(TallyResponse {
        round_id,
        phase: "Finalized".to_string(),
        registered_count: onchain.registered_count,
        choices,
        total_votes,
    }))
}

/// `GET /rounds/{id}/tally` — the live public tally, read directly from the
/// chain on every call (`get_round` + `tally`, both simulated-only, no fee)
/// rather than served from a cache, so it's never stale. Only vote *counts*
/// are public here — see the anonvote-contracts README for why that's safe:
/// nothing about which registrant cast which ballot is derivable from it.
pub async fn get_tally(
    State(state): State<AppState>,
    Path(round_id): Path<i64>,
) -> Result<Json<TallyResponse>, AppError> {
    let proposals = fetch_proposals(&state, round_id).await?;
    if proposals.is_empty() {
        return Err(AppError::NotFound(format!("round {round_id}")));
    }

    let onchain: OnChainRound = state
        .soroban
        .read("get_round", &[u64_("round_id", round_id as u64)])
        .await?;
    let votes: Vec<u64> = state
        .soroban
        .read("tally", &[u64_("round_id", round_id as u64)])
        .await?;

    let choices = zip_tally(&proposals, &votes);
    let total_votes = choices.iter().map(|c| c.votes).sum();

    Ok(Json(TallyResponse {
        round_id,
        phase: onchain.phase,
        registered_count: onchain.registered_count,
        choices,
        total_votes,
    }))
}

fn zip_tally(proposals: &[ProposalRow], votes: &[u64]) -> Vec<ChoiceTally> {
    proposals
        .iter()
        .map(|p| ChoiceTally {
            choice_index: p.choice_index,
            proposal_title: p.title.clone(),
            payout_address: p.payout_address.clone(),
            votes: votes.get(p.choice_index as usize).copied().unwrap_or(0),
        })
        .collect()
}

pub(crate) async fn fetch_round(state: &AppState, round_id: i64) -> Result<RoundRow, AppError> {
    sqlx::query_as("SELECT * FROM rounds WHERE id = $1")
        .bind(round_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("round {round_id}")))
}

pub(crate) async fn fetch_proposals(
    state: &AppState,
    round_id: i64,
) -> Result<Vec<ProposalRow>, AppError> {
    Ok(
        sqlx::query_as("SELECT * FROM proposals WHERE round_id = $1 ORDER BY choice_index")
            .bind(round_id)
            .fetch_all(&state.db)
            .await?,
    )
}
