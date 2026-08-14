use axum::{http::StatusCode, response::IntoResponse, response::Response, Json};
use serde_json::json;

use crate::{proof_service::ProofServiceError, soroban_client::SorobanError};

/// The one error type every route handler returns. Maps internal failures
/// (DB, Soroban RPC/CLI, proof generation) to an HTTP status and a small
/// JSON body, and logs the underlying cause on the way out.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error(transparent)]
    Soroban(#[from] SorobanError),

    #[error(transparent)]
    Proof(#[from] ProofServiceError),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::Soroban(SorobanError::Contract { code, name }) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("on-chain call rejected: {name} (contract error #{code})"),
            ),
            AppError::Soroban(err) => {
                tracing::error!(error = ?err, "soroban client error");
                (
                    StatusCode::BAD_GATEWAY,
                    "failed to reach the Soroban network".to_string(),
                )
            }
            AppError::Proof(ProofServiceError::Unavailable(msg)) => {
                (StatusCode::SERVICE_UNAVAILABLE, msg.clone())
            }
            AppError::Proof(err) => {
                tracing::error!(error = ?err, "proof service error");
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("proof generation failed: {err}"),
                )
            }
            AppError::Db(err) => {
                tracing::error!(error = ?err, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal database error".to_string(),
                )
            }
            AppError::Other(err) => {
                tracing::error!(error = ?err, "unhandled error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}
