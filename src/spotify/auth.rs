use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;

use crate::config::Config;

const SCOPES: &str = "streaming user-read-email user-read-private \
    user-read-playback-state user-modify-playback-state user-read-currently-playing";

/// Build the Spotify OAuth authorization URL for this user.
/// The `state` parameter is the Discord user ID so the callback can route the tokens back.
pub fn build_auth_url(cfg: &Config, state: &str) -> String {
    let scope = urlencoding::encode(SCOPES);
    let redirect = urlencoding::encode(&cfg.spotify_redirect_uri);
    format!(
        "https://accounts.spotify.com/authorize\
        ?response_type=code\
        &client_id={client_id}\
        &scope={scope}\
        &redirect_uri={redirect}\
        &state={state}",
        client_id = cfg.spotify_client_id,
    )
}

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[allow(dead_code)]
    pub token_type: String,
}

/// Exchange an authorization code for access + refresh tokens.
pub async fn exchange_code(cfg: &Config, code: &str) -> Result<TokenResponse> {
    let credentials = STANDARD.encode(format!(
        "{}:{}",
        cfg.spotify_client_id, cfg.spotify_client_secret
    ));

    let client = reqwest::Client::new();
    let resp = client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", format!("Basic {credentials}"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &cfg.spotify_redirect_uri),
        ])
        .send()
        .await
        .context("Token exchange request failed")?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Spotify token exchange error: {body}");
    }

    resp.json::<TokenResponse>()
        .await
        .context("Failed to parse token response")
}

/// Fetch the current user's Spotify profile (for display / logging).
pub async fn get_spotify_user(access_token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Profile {
        display_name: Option<String>,
        id: String,
    }

    let client = reqwest::Client::new();
    let profile: Profile = client
        .get("https://api.spotify.com/v1/me")
        .bearer_auth(access_token)
        .send()
        .await
        .context("Profile request failed")?
        .json()
        .await
        .context("Failed to parse profile")?;

    Ok(profile.display_name.unwrap_or(profile.id))
}

// urlencoding is a tiny helper — implement inline to avoid adding a crate
mod urlencoding {
    pub fn encode(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }
}
