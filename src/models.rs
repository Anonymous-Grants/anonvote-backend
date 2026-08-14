//! DB row types and HTTP request/response DTOs.
//!
//! `voter_registrations` and `ballots` (and everything derived from them,
//! like `TallyResponse`) never appear in the same struct or query together
//! with a shared key beyond `round_id` — see `migrations/0001_init.sql`.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------
// DB rows
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RoundRow {
    pub id: i64,
    pub contract_id: String,
    pub admin: String,
    pub title: String,
    pub num_choices: i32,
    pub phase: String,
    pub payout_pool_stroops: i64,
    pub create_round_tx_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub voting_opened_at: Option<DateTime<Utc>>,
    pub finalized_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ProposalRow {
    pub id: Uuid,
    pub round_id: i64,
    pub choice_index: i32,
    pub title: String,
    pub description: Option<String>,
    pub payout_address: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct VoterRegistrationRow {
    pub id: Uuid,
    pub round_id: i64,
    pub voter_address: String,
    pub leaf_index: i32,
    pub commitment_hex: String,
    pub register_tx_hash: String,
    pub registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BallotRow {
    pub id: Uuid,
    pub round_id: i64,
    pub nullifier_hex: String,
    pub choice_index: i32,
    pub cast_vote_tx_hash: String,
    pub cast_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PayoutRow {
    pub id: Uuid,
    pub round_id: i64,
    pub proposal_id: Uuid,
    pub amount_stroops: i64,
    pub vote_share_bps: i32,
    pub status: String,
    pub payout_tx_hash: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub executed_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------
// On-chain response shapes (decoded from `stellar contract invoke` JSON)
// ---------------------------------------------------------------------

/// Mirrors `anonvote::Round` exactly as the contract's spec renders it
/// (confirmed against a live deployment while building this module). Kept
/// complete even though only `phase`/`registered_count` are read today, so
/// deserialization stays correct if a handler needs more of it later.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OnChainRound {
    pub admin: String,
    pub ballot_count: u32,
    pub id: u64,
    pub merkle_root: String,
    pub num_choices: u32,
    pub phase: String,
    pub registered_count: u32,
    pub title: String,
}

// ---------------------------------------------------------------------
// POST /rounds
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateRoundRequest {
    pub title: String,
    #[serde(default)]
    pub payout_pool_stroops: i64,
    pub proposals: Vec<CreateProposalRequest>,
}

#[derive(Debug, Deserialize)]
pub struct CreateProposalRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub payout_address: String,
}

#[derive(Debug, Serialize)]
pub struct CreateRoundResponse {
    #[serde(flatten)]
    pub round: RoundRow,
    pub proposals: Vec<ProposalRow>,
}

#[derive(Debug, Deserialize)]
pub struct SetEligibilityRequest {
    pub voters: Vec<String>,
    #[serde(default = "default_true")]
    pub eligible: bool,
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct SetEligibilityResponse {
    pub round_id: i64,
    pub updated: Vec<String>,
    pub failed: Vec<EligibilityFailure>,
}

#[derive(Debug, Serialize)]
pub struct EligibilityFailure {
    pub voter: String,
    pub error: String,
}

// ---------------------------------------------------------------------
// POST /voters/register
// ---------------------------------------------------------------------

/// The voter's wallet builds and signs the `register` invocation itself
/// (this backend never holds a voter's key — `register` needs
/// `voter.require_auth()`) and submits the resulting envelope here. This
/// backend only relays it to the network and records the outcome.
#[derive(Debug, Deserialize)]
pub struct RegisterVoterRequest {
    pub round_id: i64,
    pub voter: String,
    pub commitment_hex: String,
    pub signed_xdr: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterVoterResponse {
    pub round_id: i64,
    pub voter: String,
    pub leaf_index: i32,
    pub commitment_hex: String,
    pub tx_hash: Option<String>,
}

// ---------------------------------------------------------------------
// POST /votes
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofDto {
    pub a: String,
    pub b: String,
    pub c: String,
}

/// The private witness for "I'm a registered voter and this is my first
/// vote in this round" (see `circuits/anonvote/src/main.nr` in
/// anonvote-contracts) — only used when asking this backend to prove
/// server-side instead of submitting an already-built proof. Field elements
/// are decimal or `0x`-prefixed hex strings, matching Noir's `Prover.toml`
/// convention.
#[derive(Debug, Clone, Deserialize)]
pub struct ProveWitnessRequest {
    pub secret: String,
    pub merkle_path: Vec<String>,
    pub path_indices: Vec<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CastVoteRequest {
    pub round_id: i64,
    pub choice: i32,
    pub nullifier_hex: String,
    /// Exactly one of `proof` or `witness` must be set. See the README's
    /// "casting a vote" section.
    #[serde(default)]
    pub proof: Option<ProofDto>,
    #[serde(default)]
    pub witness: Option<ProveWitnessRequest>,
}

#[derive(Debug, Serialize)]
pub struct CastVoteResponse {
    pub round_id: i64,
    pub choice: i32,
    pub nullifier_hex: String,
    pub tx_hash: Option<String>,
}

// ---------------------------------------------------------------------
// GET /rounds/{id}/tally
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct TallyResponse {
    pub round_id: i64,
    pub phase: String,
    pub registered_count: u32,
    pub choices: Vec<ChoiceTally>,
    pub total_votes: u64,
}

#[derive(Debug, Serialize)]
pub struct ChoiceTally {
    pub choice_index: i32,
    pub proposal_title: String,
    pub payout_address: String,
    pub votes: u64,
}

// ---------------------------------------------------------------------
// POST /payouts/execute
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ExecutePayoutsRequest {
    pub round_id: i64,
}

#[derive(Debug, Serialize)]
pub struct ExecutePayoutsResponse {
    pub round_id: i64,
    pub payout_pool_stroops: i64,
    pub payouts: Vec<PayoutResult>,
}

#[derive(Debug, Serialize)]
pub struct PayoutResult {
    pub proposal_id: Uuid,
    pub choice_index: i32,
    pub payout_address: String,
    pub votes: u64,
    pub vote_share_bps: i32,
    pub amount_stroops: i64,
    pub status: String,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
}
