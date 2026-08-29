mod config;

use std::{env, io::Write as _, net::SocketAddr, sync::Arc};

use open_search_providers::grok::{
    GrokCredentialSession, GrokCredentialStore, GrokOAuthClient, GrokSearchProvider,
};
use open_search_runtime::SearchEngine;
use open_search_server::app;
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:8080";
const DEFAULT_LOG_FILTER: &str = "info,rmcp=warn";
const GROK_AGENT_ID_METADATA_KEY: &str = "grok.agent_id";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .init();

    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => serve().await,
        [command] if command == "serve" => serve().await,
        [provider, command] if provider == "grok" && command == "oauth" => grok_oauth().await,
        [argument] if matches!(argument.as_str(), "-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unknown command; run open-search --help",
        )
        .into()),
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let listen_address =
        env::var("LISTEN_ADDRESS").unwrap_or_else(|_| DEFAULT_LISTEN_ADDRESS.to_owned());
    let listen_address: SocketAddr = listen_address.parse()?;
    let store = GrokCredentialStore::connect(config::database_path()).await?;
    let grok_agent_id = store
        .load_or_insert_metadata(GROK_AGENT_ID_METADATA_KEY, &Uuid::new_v4().to_string())
        .await?;
    let session = GrokCredentialSession::load(store).await?;
    let grok_credential_configured = session.has_credentials().await;
    let bearer_token = config::mcp_bearer_token();
    let bearer_auth = bearer_token.is_some();
    let engine = SearchEngine::new(Arc::new(GrokSearchProvider::new(session, grok_agent_id)));
    let listener = tokio::net::TcpListener::bind(listen_address).await?;

    info!(
        address = %listen_address,
        bearer_auth,
        grok_credential_configured,
        "open-search listening"
    );
    axum::serve(listener, app(engine, bearer_token))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn grok_oauth() -> Result<(), Box<dyn std::error::Error>> {
    let started = GrokOAuthClient::new().start().await?;
    println!("Open this URL to authorize Grok:");
    println!(
        "{}",
        started
            .challenge
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&started.challenge.verification_uri)
    );
    println!("User code: {}", started.challenge.user_code);
    println!("Waiting for authorization...");
    std::io::stdout().flush()?;

    let credentials = started.pending.complete().await?;
    let store = GrokCredentialStore::connect(config::database_path()).await?;
    store.save_authorization(&credentials).await?;
    println!("Grok authorization completed and saved.");
    Ok(())
}

fn print_help() {
    println!("open-search");
    println!();
    println!("Usage:");
    println!("  open-search serve       Start the service (default)");
    println!("  open-search grok oauth  Authorize the Grok account");
    println!();
    println!(
        "Credential data is stored in {} by default.",
        config::DEFAULT_DATABASE_PATH
    );
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal handler");
    }
}
