use dashmap::DashMap;
use tokio::sync::{oneshot, RwLock};

/// Tokens received after OAuth is complete.
#[derive(Debug, Clone)]
pub struct SpotifyTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Shared application state threaded through the axum server and Discord bot.
pub struct Store {
    /// Pending OAuth flows: discord_user_id → oneshot sender waiting for tokens.
    pub pending: DashMap<String, oneshot::Sender<SpotifyTokens>>,

    /// The bot's Discord username, set once the gateway Ready event fires.
    /// Used as the Spotify Connect device name so it appears correctly in the app.
    pub bot_name: RwLock<String>,
}

impl Store {
    pub fn new() -> Self {
        Store {
            pending: DashMap::new(),
            bot_name: RwLock::new("Discord Bot".to_string()),
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}
