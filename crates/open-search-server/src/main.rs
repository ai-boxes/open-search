use std::{env, net::SocketAddr, sync::Arc};

use open_search_providers::{UnconfiguredAnswerProvider, UnconfiguredSearchProvider};
use open_search_runtime::SearchEngine;
use open_search_server::app;
use tracing::info;
use tracing_subscriber::EnvFilter;

const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let listen_address = env::var("OPEN_SEARCH_LISTEN_ADDRESS")
        .unwrap_or_else(|_| DEFAULT_LISTEN_ADDRESS.to_owned());
    let listen_address: SocketAddr = listen_address.parse()?;
    let listener = tokio::net::TcpListener::bind(listen_address).await?;

    let engine = SearchEngine::new(
        Arc::new(UnconfiguredSearchProvider),
        Arc::new(UnconfiguredAnswerProvider),
    );

    info!(address = %listen_address, "open-search listening");
    axum::serve(listener, app(engine))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal handler");
    }
}
