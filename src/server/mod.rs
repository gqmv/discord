use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    config::Config,
    spotify::auth::exchange_code,
    store::{Store, SpotifyTokens},
};

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<Config>,
    pub store: Arc<Store>,
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/auth", get(handle_auth_redirect))
        .route("/auth/callback", get(handle_auth_callback))
        .with_state(state)
}

/// GET /auth?state=<discord_user_id>
/// Redirects the user to Spotify's authorization page.
async fn handle_auth_redirect(
    State(s): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let state = params.get("state").cloned().unwrap_or_default();
    let url = crate::spotify::auth::build_auth_url(&s.config, &state);
    Redirect::temporary(&url)
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// GET /auth/callback?code=...&state=<discord_user_id>
async fn handle_auth_callback(
    State(s): State<ServerState>,
    Query(params): Query<CallbackParams>,
) -> impl IntoResponse {
    if let Some(err) = params.error {
        return Html(format!(
            "<h1>Authorization denied</h1><p>{err}</p>\
             <p>You can close this tab.</p>"
        ));
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            return Html(
                "<h1>Missing code</h1><p>Something went wrong. Please try again.</p>".to_string(),
            )
        }
    };

    let user_id = match params.state {
        Some(s) => s,
        None => {
            return Html(
                "<h1>Missing state</h1><p>Cannot identify the Discord user.</p>".to_string(),
            )
        }
    };

    match exchange_code(&s.config, &code).await {
        Ok(tokens) => {
            if let Some((_, tx)) = s.store.pending.remove(&user_id) {
                let spotify_tokens = SpotifyTokens {
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                };
                if tx.send(spotify_tokens).is_err() {
                    error!("Discord command timed out before tokens arrived for user {user_id}");
                } else {
                    info!("OAuth complete for Discord user {user_id}");
                }
            } else {
                error!("No pending session for user {user_id} — did the command expire?");
            }

            Html(
                "<h1>✅ Connected to Spotify!</h1>\
                 <p>You can close this tab and go back to Discord.</p>"
                    .to_string(),
            )
        }
        Err(e) => {
            error!("Token exchange failed: {e}");
            Html(format!("<h1>Error</h1><p>{e}</p>"))
        }
    }
}
