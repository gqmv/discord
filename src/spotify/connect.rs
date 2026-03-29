use anyhow::Result;
use crossbeam_channel::{bounded, unbounded};
use futures_util::StreamExt;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{
    authentication::Credentials,
    config::SessionConfig,
    dealer::protocol::PayloadValue,
    Session,
};
use librespot_playback::{
    config::{Bitrate, PlayerConfig},
    mixer::{self, MixerConfig, NoOpVolume},
    player::{Player, PlayerEventChannel},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::sink::{DiscordSink, PcmReader};

/// Registers a new Spotify Connect device using the user's OAuth access token.
///
/// Returns:
/// - `Spirc`                          — handle for pause/resume/disconnect control
/// - `PcmReader`                      — live f32 PCM byte stream at 44 100 Hz stereo
/// - `PlayerEventChannel`             — stream of playback events (track changes, …)
/// - `mpsc::UnboundedReceiver<String>` — Jam join URLs as they arrive via the Spirc dealer
pub async fn create_connect_device(
    device_name: &str,
    access_token: &str,
) -> Result<(Spirc, PcmReader, PlayerEventChannel, mpsc::UnboundedReceiver<String>)> {
    let credentials = Credentials::with_access_token(access_token);
    let session = Session::new(SessionConfig::default(), None);

    // Bounded channel: each slot is ~20 ms of stereo f32 PCM at 44100 Hz (~7 KB).
    // 64 slots ≈ 1.3 s of audio. This throttles librespot's decode thread to
    // real-time speed — without this, librespot would decode an entire track in
    // ~2 seconds, signal "done" to Spotify, and the app would jump to the next track.
    let (pcm_tx, pcm_rx) = bounded::<Vec<u8>>(64);
    let (flush_tx, flush_rx) = unbounded::<()>();

    let player_config = PlayerConfig {
        bitrate: Bitrate::Bitrate320,
        ..Default::default()
    };

    let player = Player::new(
        player_config,
        session.clone(),
        Box::new(NoOpVolume),
        move || Box::new(DiscordSink::new(pcm_tx.clone(), flush_tx.clone())),
    );

    // Grab the event channel before the player is consumed by Spirc.
    let event_channel = player.get_player_event_channel();

    let soft_mixer = mixer::find(None).expect("SoftMixer must always be available");

    let connect_config = ConnectConfig {
        name: device_name.to_string(),
        initial_volume: 0x8000, // ~50 %
        ..Default::default()
    };

    // ── Jam session detection ────────────────────────────────────────────
    // Register the subscription NOW, in the dealer builder phase — i.e. before
    // Spirc::new() calls session.connect() which launches the WebSocket.  This
    // mirrors exactly how Spirc itself registers its own subscriptions and
    // guarantees we never miss an early delivery.
    let (jam_tx, jam_rx) = mpsc::unbounded_channel::<String>();
    let jam_stream = session
        .dealer()
        .listen_for("social-connect/v2/session_update", |msg| Ok(msg));

    let (spirc, spirc_task) = Spirc::new(
        connect_config,
        session,
        credentials,
        player,
        soft_mixer(MixerConfig::default())?,
    )
    .await?;

    // Drive the Spirc event loop in the background
    tokio::spawn(spirc_task);

    // Spawn the task that forwards Jam URLs once we know the dealer is up.
    match jam_stream {
        Err(e) => {
            error!("Failed to subscribe to social-connect dealer topic: {e}");
        }
        Ok(mut stream) => {
            tokio::spawn(async move {
                info!("Jam dealer subscription active — waiting for social-connect messages");
                while let Some(result) = stream.next().await {
                    match result {
                        Err(e) => {
                            warn!("Jam dealer message error: {e}");
                        }
                        Ok(msg) => {
                            debug!("social-connect message — uri: {}", msg.uri);
                            let url = extract_jam_url(&msg.payload);
                            if let Some(url) = url {
                                info!("Jam session URL received: {url}");
                                let _ = jam_tx.send(url);
                            }
                        }
                    }
                }
                info!("Jam dealer subscription stream ended");
            });
        }
    }

    let reader = PcmReader::new(pcm_rx, flush_rx);
    Ok((spirc, reader, event_channel, jam_rx))
}

/// Extracts a Spotify Jam join URL from a dealer message payload.
///
/// Spotify delivers `social-connect/v2/session_update` as a JSON-valued payload
/// whose structure matches the `SessionUpdate` protobuf (field names in camelCase).
/// We also handle the raw-binary (protobuf) case defensively by attempting a
/// UTF-8 decode and JSON parse with both camelCase and snake_case key variants.
pub fn extract_jam_url(payload: &PayloadValue) -> Option<String> {
    match payload {
        PayloadValue::Json(json) => {
            debug!("social-connect payload (JSON): {json}");
            parse_jam_url_from_json(json)
        }
        PayloadValue::Raw(bytes) => {
            if let Ok(text) = std::str::from_utf8(bytes) {
                debug!("social-connect payload (raw/UTF-8): {text}");
                parse_jam_url_from_json(text)
            } else {
                debug!(
                    "social-connect payload (raw/binary, {} bytes) — cannot parse",
                    bytes.len()
                );
                None
            }
        }
        PayloadValue::Empty => {
            debug!("social-connect payload is empty");
            None
        }
    }
}

/// Extract a `spotify://socialsession/TOKEN` deep-link from a JSON payload.
///
/// Priority:
/// 1. `session.joinSessionToken` / `session.join_session_token`  →  build the
///    deep-link from the token directly (most reliable).
/// 2. Any `joinSessionUrl` / `join_session_url` field  →  normalise whatever
///    URL Spotify provided (often `hm://...`) into a `spotify://` deep-link.
fn parse_jam_url_from_json(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;

    // The interesting object is either the root value or its "session" child.
    let session = value
        .get("session")
        .unwrap_or(&value);

    // 1. Prefer the session token — gives us the canonical deep-link directly.
    let token_keys = ["joinSessionToken", "join_session_token"];
    for key in token_keys {
        if let Some(token) = session.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            debug!("Jam token ({key}): {token}");
            return Some(format!("spotify://socialsession/{token}"));
        }
    }

    // 2. Fall back to any URL/URI field and normalise it.
    let url_keys = [
        "joinSessionUrl", "join_session_url",
        "joinSessionUri", "join_session_uri",
    ];
    for key in url_keys {
        if let Some(raw) = session.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            debug!("Jam raw url ({key}): {raw}");
            if let Some(normalised) = normalise_jam_url(raw) {
                return Some(normalised);
            }
        }
    }

    None
}

/// Convert any Spotify-internal URL/URI into a `spotify://socialsession/TOKEN`
/// deep-link that opens the Spotify app directly.
///
/// Returns `None` for completely unrecognised schemes so we never forward junk.
fn normalise_jam_url(raw: &str) -> Option<String> {
    // hm://social-connect/v2/sessions/join/TOKEN
    if let Some(token) = raw
        .strip_prefix("hm://social-connect/v2/sessions/join/")
        .filter(|t| !t.is_empty())
    {
        return Some(format!("spotify://socialsession/{token}"));
    }

    // https://open.spotify.com/socialsession/TOKEN?si=...
    if let Some(rest) = raw.strip_prefix("https://open.spotify.com/socialsession/") {
        let token = rest.split('?').next().unwrap_or(rest);
        if !token.is_empty() {
            return Some(format!("spotify://socialsession/{token}"));
        }
    }

    // Already a deep-link
    if raw.starts_with("spotify://socialsession/") {
        return Some(raw.to_owned());
    }

    warn!("Unrecognised Jam URL scheme, discarding: {raw}");
    None
}
