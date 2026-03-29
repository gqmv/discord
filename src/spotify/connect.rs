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
fn extract_jam_url(payload: &PayloadValue) -> Option<String> {
    match payload {
        PayloadValue::Json(json) => {
            debug!("social-connect payload (JSON): {json}");
            parse_jam_url_from_json(json)
        }
        PayloadValue::Raw(bytes) => {
            // The payload arrived as base64-decoded binary.  Spotify sometimes
            // sends the proto bytes directly; attempt a best-effort UTF-8 parse
            // in case it is actually JSON wrapped in a different envelope.
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

/// Try to pull `joinSessionUrl` (camelCase) or `join_session_url` (snake_case)
/// from the top-level object or from a nested `"session"` key.
fn parse_jam_url_from_json(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;

    // Candidates in priority order
    let candidates = [
        // Standard protobuf-JSON (camelCase) — nested under "session"
        value.pointer("/session/joinSessionUrl"),
        // snake_case variant
        value.pointer("/session/join_session_url"),
        // Sometimes delivered at the top level
        value.get("joinSessionUrl"),
        value.get("join_session_url"),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Some(url) = candidate.as_str() {
            if !url.is_empty() {
                return Some(url.to_owned());
            }
        }
    }

    None
}
