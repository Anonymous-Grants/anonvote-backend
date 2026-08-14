mod payouts;
mod rounds;
mod voters;
mod votes;

use axum::{
    routing::{get, post},
    Router,
};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(health))
        .route("/rounds", post(rounds::create_round))
        .route("/rounds/{id}", get(rounds::get_round))
        .route("/rounds/{id}/eligibility", post(rounds::set_eligibility))
        .route("/rounds/{id}/open", post(rounds::open_voting))
        .route("/rounds/{id}/finalize", post(rounds::finalize_round))
        .route("/rounds/{id}/tally", get(rounds::get_tally))
        .route("/rounds/{id}/payouts", get(payouts::list_payouts))
        .route("/voters/register", post(voters::register))
        .route("/votes", post(votes::cast_vote))
        .route("/payouts/execute", post(payouts::execute_payouts))
}

async fn health() -> &'static str {
    "ok"
}
