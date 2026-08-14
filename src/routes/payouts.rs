use axum::{
    extract::{Path, State},
    Json,
};

use crate::{
    error::AppError,
    models::{ExecutePayoutsRequest, ExecutePayoutsResponse, PayoutResult, PayoutRow},
    routes::rounds::{fetch_proposals, fetch_round},
    soroban_client::u64_,
    state::AppState,
};

/// `GET /rounds/{id}/payouts` — payout history for a round (empty until
/// `POST /payouts/execute` has run).
pub async fn list_payouts(
    State(state): State<AppState>,
    Path(round_id): Path<i64>,
) -> Result<Json<Vec<PayoutRow>>, AppError> {
    fetch_round(&state, round_id).await?;
    let rows: Vec<PayoutRow> = sqlx::query_as("SELECT * FROM payouts WHERE round_id = $1 ORDER BY created_at")
        .bind(round_id)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(rows))
}

/// `POST /payouts/execute` — once a round is finalized, splits its
/// `payout_pool_stroops` across proposals in proportion to their share of
/// the final tally (a one-shot, Drips-style [1] proportional split — see
/// the README's "payouts" section for why "one-shot" rather than a
/// continuous stream) and submits one classic Stellar payment per proposal
/// with a nonzero share.
///
/// [1]: https://drips.network — continuous proportional funding splits;
/// this endpoint borrows the "split by share" idea for a single settlement
/// rather than implementing an on-chain streaming split itself.
///
/// Idempotent per round: if payouts were already recorded for `round_id`,
/// this returns 409 rather than paying out twice.
pub async fn execute_payouts(
    State(state): State<AppState>,
    Json(req): Json<ExecutePayoutsRequest>,
) -> Result<Json<ExecutePayoutsResponse>, AppError> {
    let round = fetch_round(&state, req.round_id).await?;
    if round.phase != "finalized" {
        return Err(AppError::Conflict(format!(
            "round {} must be finalized before payouts can execute (phase = {})",
            req.round_id, round.phase
        )));
    }
    if round.payout_pool_stroops <= 0 {
        return Err(AppError::BadRequest(
            "round has no payout_pool_stroops configured".to_string(),
        ));
    }

    let already: i64 = sqlx::query_scalar("SELECT count(*) FROM payouts WHERE round_id = $1")
        .bind(req.round_id)
        .fetch_one(&state.db)
        .await?;
    if already > 0 {
        return Err(AppError::Conflict(format!(
            "payouts have already been executed for round {}",
            req.round_id
        )));
    }

    let proposals = fetch_proposals(&state, req.round_id).await?;
    let votes: Vec<u64> = state
        .soroban
        .read("tally", &[u64_("round_id", req.round_id as u64)])
        .await?;
    let proposal_votes: Vec<u64> = proposals
        .iter()
        .map(|p| votes.get(p.choice_index as usize).copied().unwrap_or(0))
        .collect();
    let total_votes: u64 = proposal_votes.iter().sum();

    let shares = allocate_stroops(round.payout_pool_stroops as u64, &proposal_votes, total_votes);

    let mut results = Vec::with_capacity(proposals.len());
    for (proposal, (amount_stroops, vote_share_bps)) in proposals.iter().zip(shares) {
        let votes_for = votes.get(proposal.choice_index as usize).copied().unwrap_or(0);

        let (status, tx_hash, error) = if amount_stroops == 0 {
            (PayoutStatus::Confirmed, None, None)
        } else {
            match state
                .soroban
                .send_payment(
                    &state.config.treasury_identity,
                    &proposal.payout_address,
                    amount_stroops as i64,
                )
                .await
            {
                Ok(tx_hash) => (PayoutStatus::Submitted, tx_hash, None),
                Err(e) => (PayoutStatus::Failed, None, Some(e.to_string())),
            }
        };

        sqlx::query(
            r#"INSERT INTO payouts (round_id, proposal_id, amount_stroops, vote_share_bps, status, payout_tx_hash, error, executed_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now())"#,
        )
        .bind(req.round_id)
        .bind(proposal.id)
        .bind(amount_stroops as i64)
        .bind(vote_share_bps)
        .bind(status.as_str())
        .bind(&tx_hash)
        .bind(&error)
        .execute(&state.db)
        .await?;

        results.push(PayoutResult {
            proposal_id: proposal.id,
            choice_index: proposal.choice_index,
            payout_address: proposal.payout_address.clone(),
            votes: votes_for,
            vote_share_bps,
            amount_stroops: amount_stroops as i64,
            status: status.as_str().to_string(),
            tx_hash,
            error,
        });
    }

    Ok(Json(ExecutePayoutsResponse {
        round_id: req.round_id,
        payout_pool_stroops: round.payout_pool_stroops,
        payouts: results,
    }))
}

enum PayoutStatus {
    Confirmed,
    Submitted,
    Failed,
}
impl PayoutStatus {
    fn as_str(&self) -> &'static str {
        match self {
            PayoutStatus::Confirmed => "confirmed",
            PayoutStatus::Submitted => "submitted",
            PayoutStatus::Failed => "failed",
        }
    }
}

/// Splits `pool` stroops across `votes` in exact proportion to each entry's
/// share of `total_votes`, using the largest-remainder method so the
/// amounts sum to exactly `pool` (plain `pool * v / total` per entry would
/// under-allocate by a few stroops to rounding). Returns `(amount_stroops,
/// vote_share_bps)` per entry, in the same order as `votes`.
fn allocate_stroops(pool: u64, votes: &[u64], total_votes: u64) -> Vec<(u64, i32)> {
    if total_votes == 0 {
        return votes.iter().map(|_| (0, 0)).collect();
    }

    let mut amounts = Vec::with_capacity(votes.len());
    let mut remainders = Vec::with_capacity(votes.len());
    let mut allocated: u64 = 0;
    for &v in votes {
        let numerator = pool as u128 * v as u128;
        let amount = (numerator / total_votes as u128) as u64;
        remainders.push(numerator % total_votes as u128);
        amounts.push(amount);
        allocated += amount;
    }

    let mut leftover = pool - allocated;
    let mut order: Vec<usize> = (0..votes.len()).collect();
    order.sort_by(|&a, &b| remainders[b].cmp(&remainders[a]));
    for i in order {
        if leftover == 0 {
            break;
        }
        amounts[i] += 1;
        leftover -= 1;
    }

    amounts
        .into_iter()
        .zip(votes)
        .map(|(amount, &v)| {
            let bps = ((v as u128 * 10_000) / total_votes as u128) as i32;
            (amount, bps)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::allocate_stroops;

    #[test]
    fn splits_sum_exactly_to_the_pool_even_with_rounding() {
        let shares = allocate_stroops(1_000, &[1, 1, 1], 3);
        let total: u64 = shares.iter().map(|(a, _)| a).sum();
        assert_eq!(total, 1_000);
        // largest-remainder distributes the 1-stroop rounding leftovers,
        // so amounts differ by at most 1 stroop from each other.
        let min = shares.iter().map(|(a, _)| *a).min().unwrap();
        let max = shares.iter().map(|(a, _)| *a).max().unwrap();
        assert!(max - min <= 1);
    }

    #[test]
    fn zero_votes_gets_zero_payout() {
        let shares = allocate_stroops(1_000, &[5, 0, 5], 10);
        assert_eq!(shares[1].0, 0);
        assert_eq!(shares[1].1, 0);
        assert_eq!(shares[0].0 + shares[2].0, 1_000);
    }

    #[test]
    fn no_votes_at_all_pays_out_nothing() {
        let shares = allocate_stroops(1_000, &[0, 0], 0);
        assert_eq!(shares, vec![(0, 0), (0, 0)]);
    }

    #[test]
    fn proportional_to_vote_share() {
        let shares = allocate_stroops(10_000, &[75, 25], 100);
        assert_eq!(shares[0], (7_500, 7_500));
        assert_eq!(shares[1], (2_500, 2_500));
    }
}
