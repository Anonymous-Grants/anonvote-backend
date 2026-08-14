mod config;
mod error;
mod models;
mod proof_service;
mod routes;
mod soroban_client;
mod state;

use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
    config::Config, proof_service::ProofService, soroban_client::SorobanClient,
    state::AppStateInner,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anonvote_backend=info,tower_http=info".into()),
        )
        .init();

    let config = Config::from_env()?;

    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let soroban = SorobanClient::new(&config);
    let operator_address =
        SorobanClient::resolve_address(&config.stellar_cli_path, &config.operator_identity)
            .await
            .map_err(|e| anyhow::anyhow!("could not resolve STELLAR_OPERATOR_IDENTITY: {e}"))?;
    tracing::info!(%operator_address, "resolved operator identity");

    if !config.circuit_dir.exists() {
        tracing::warn!(
            circuit_dir = %config.circuit_dir.display(),
            "circuit directory does not exist; server-side proof generation will fail if requested"
        );
    }
    let proof_service = ProofService::new(
        config.circuit_dir.clone(),
        config.nargo_path.clone(),
        config.bb_path.clone(),
    );
    tracing::info!(
        available = proof_service.is_proving_available(),
        "server-side proof generation"
    );

    let bind_addr = config.bind_addr.clone();
    let state: state::AppState = Arc::new(AppStateInner {
        db,
        soroban,
        proof_service,
        config,
        operator_address,
    });

    let app = routes::router()
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    tracing::info!(%bind_addr, "starting anonvote-backend");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
