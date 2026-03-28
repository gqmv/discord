use anyhow::Result;
use crossbeam_channel::{bounded, unbounded};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{authentication::Credentials, config::SessionConfig, Session};
use librespot_playback::{
    config::{Bitrate, PlayerConfig},
    mixer::{self, MixerConfig, NoOpVolume},
    player::{Player, PlayerEventChannel},
};

use super::sink::{DiscordSink, PcmReader};

/// Registers a new Spotify Connect device using the user's OAuth access token.
///
/// Returns:
/// - `Spirc`              — handle for pause/resume/disconnect control
/// - `PcmReader`          — live f32 PCM byte stream at 44 100 Hz stereo, ready for songbird
/// - `PlayerEventChannel` — stream of playback events (track changes, pause, stop, …)
pub async fn create_connect_device(
    device_name: &str,
    access_token: &str,
) -> Result<(Spirc, PcmReader, PlayerEventChannel)> {
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

    let reader = PcmReader::new(pcm_rx, flush_rx);
    Ok((spirc, reader, event_channel))
}
