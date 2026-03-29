mod bot;
mod config;
mod server;
mod spotify;
mod store;

use std::sync::Arc;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "discord_spotify_bot=debug,songbird=warn,serenity=warn,info".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = Arc::new(config::Config::from_env()?);
    let store = Arc::new(store::Store::new());

    // ── HTTP auth-callback server ─────────────────────────────────────────
    let server_state = server::ServerState {
        config: cfg.clone(),
        store: store.clone(),
    };
    let app = server::router(server_state);
    let listen_addr = format!("0.0.0.0:{}", cfg.auth_server_port);
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    info!("Auth server listening on http://{listen_addr}");

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("Auth server crashed");
    });

    // ── Discord bot ───────────────────────────────────────────────────────
    let mut client = bot::build_client(cfg, store).await?;
    info!("Starting Discord bot…");
    client.start().await?;

    Ok(())
}
