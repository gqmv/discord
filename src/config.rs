use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_client_id: u64,
    pub discord_guild_id: u64,
    pub spotify_client_id: String,
    pub spotify_client_secret: String,
    pub spotify_redirect_uri: String,
    pub auth_server_port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Config {
            discord_token: required("DISCORD_TOKEN")?,
            discord_client_id: required("DISCORD_CLIENT_ID")?
                .parse()
                .context("DISCORD_CLIENT_ID must be a number")?,
            discord_guild_id: required("DISCORD_GUILD_ID")?
                .parse()
                .context("DISCORD_GUILD_ID must be a number")?,
            spotify_client_id: required("SPOTIFY_CLIENT_ID")?,
            spotify_client_secret: required("SPOTIFY_CLIENT_SECRET")?,
            spotify_redirect_uri: required("SPOTIFY_REDIRECT_URI")?,
            auth_server_port: std::env::var("AUTH_SERVER_PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .context("AUTH_SERVER_PORT must be a number")?,
        })
    }
}

fn required(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("Missing required env var: {key}"))
}
