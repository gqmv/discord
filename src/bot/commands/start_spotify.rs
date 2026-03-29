use std::{sync::Arc, time::Duration};

use librespot_playback::player::PlayerEvent;
use serenity::all::{
    ActivityData, CommandInteraction, Context, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, OnlineStatus,
};
use songbird::input::{Input, RawAdapter};
use tokio::sync::oneshot;
use tracing::{error, info};

use crate::{
    config::Config,
    spotify::{
        auth::{build_auth_url, get_spotify_user},
        connect::create_connect_device,
    },
    store::Store,
};

pub async fn run(ctx: &Context, command: &CommandInteraction, cfg: Arc<Config>, store: Arc<Store>) {
    // ── 1. Require the user to be in a voice channel ──────────────────────
    let guild_id = match command.guild_id {
        Some(id) => id,
        None => {
            reply_ephemeral(ctx, command, "This command must be used inside a server.").await;
            return;
        }
    };

    let voice_channel_id = {
        ctx.cache
            .guild(guild_id)
            .and_then(|g| {
                g.voice_states
                    .get(&command.user.id)
                    .and_then(|vs| vs.channel_id)
            })
    };

    let channel_id = match voice_channel_id {
        Some(id) => id,
        None => {
            reply_ephemeral(ctx, command, "You need to be in a voice channel first.").await;
            return;
        }
    };

    // ── 2. Send ephemeral auth link ───────────────────────────────────────
    let user_id = command.user.id.to_string();
    let auth_url = build_auth_url(&cfg, &user_id);

    let msg = format!(
        "**Connect your Spotify account:**\n[Click here to authorize]({auth_url})\n\n\
         _This link expires in 5 minutes._"
    );
    reply_ephemeral(ctx, command, &msg).await;

    // ── 3. Wait for OAuth callback (5-minute timeout) ─────────────────────
    let (tx, rx) = oneshot::channel();
    store.pending.insert(user_id.clone(), tx);

    let tokens = match tokio::time::timeout(Duration::from_secs(300), rx).await {
        Ok(Ok(t)) => t,
        Ok(Err(_)) => {
            store.pending.remove(&user_id);
            follow_up(ctx, command, "❌ Authorization was cancelled.").await;
            return;
        }
        Err(_) => {
            store.pending.remove(&user_id);
            follow_up(ctx, command, "❌ Authorization timed out. Please run the command again.").await;
            return;
        }
    };

    // ── 4. Show who we're connecting as ──────────────────────────────────
    let spotify_name = get_spotify_user(&tokens.access_token)
        .await
        .unwrap_or_else(|_| "Unknown".to_string());
    info!("Spotify user: {spotify_name} — joining voice channel {channel_id}");

    follow_up(
        ctx,
        command,
        &format!(
            "✅ Connected as **{spotify_name}**. Joining your voice channel and registering as a Spotify Connect device…"
        ),
    )
    .await;

    // ── 5. Create librespot Spirc device + PcmReader ──────────────────────
    let device_name = store.bot_name.read().await.clone();
    let (spirc, reader, event_channel, jam_url_rx) = match create_connect_device(&device_name, &tokens.access_token).await {
        Ok(pair) => pair,
        Err(e) => {
            error!("Failed to create Spotify Connect device: {e}");
            follow_up(ctx, command, &format!("❌ Spotify Connect error: {e}")).await;
            return;
        }
    };

    // ── 6. Join voice channel and hand audio to songbird ─────────────────
    let manager = match songbird::get(ctx).await {
        Some(m) => m,
        None => {
            error!("Songbird not initialised");
            follow_up(ctx, command, "❌ Internal error: voice system not ready.").await;
            return;
        }
    };

    let handler_lock = match manager.join(guild_id, channel_id).await {
        Ok(h) => h,
        Err(e) => {
            error!("Failed to join voice channel: {e}");
            follow_up(ctx, command, &format!("❌ Could not join voice channel: {e}")).await;
            let _ = spirc.shutdown();
            return;
        }
    };

    let input: Input = RawAdapter::new(reader, 44_100, 2).into();

    {
        let mut handler = handler_lock.lock().await;
        handler.play_input(input);
    }

    // Ephemeral confirmation to the user who ran the command
    follow_up(
        ctx,
        command,
        &format!("✅ **{spotify_name}** — open Spotify, pick **{device_name}** as your device, then hit **Start a Jam** to share the link here!"),
    )
    .await;

    // Public channel announcement so everyone sees when a Jam starts
    let _ = command
        .channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new().content(format!(
                "🎵 **{spotify_name}** started a listening session on **{device_name}**! \
                Open Spotify → **Start a Jam** → I'll post the link here automatically."
            )),
        )
        .await;

    // ── 8. Jam URL: post the join link publicly when a Jam is started ─────
    let ctx_jam = ctx.clone();
    let channel_id = command.channel_id;
    let bot_name_jam = device_name.clone();
    tokio::spawn(async move {
        let mut rx = jam_url_rx;
        while let Some(url) = rx.recv().await {
            info!("Jam session started: {url}");
            let _ = channel_id
                .send_message(
                    &ctx_jam.http,
                    CreateMessage::new().content(format!(
                        "🎵 **{bot_name_jam}** — [Join the Jam]({url})"
                    )),
                )
                .await;
        }
    });

    // ── 9. Presence: update "Listening to" status as tracks change ────────
    let ctx_presence = ctx.clone();
    tokio::spawn(async move {
        let mut events = event_channel;
        while let Some(event) = events.recv().await {
            match event {
                PlayerEvent::TrackChanged { audio_item } => {
                    let track = &audio_item.name;
                    let artist = match &audio_item.unique_fields {
                        librespot_metadata::audio::UniqueFields::Track { artists, .. } => {
                            artists
                                .0
                                .iter()
                                .map(|a| a.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                        librespot_metadata::audio::UniqueFields::Local { artists, .. } => {
                            artists.as_deref().unwrap_or("").to_string()
                        }
                        librespot_metadata::audio::UniqueFields::Episode {
                            show_name, ..
                        } => show_name.clone(),
                    };
                    let status = format!("{track} · {artist}");
                    info!("Now playing: {status}");
                    ctx_presence.set_presence(
                        Some(ActivityData::listening(&status)),
                        OnlineStatus::Online,
                    );
                }
                PlayerEvent::Stopped { .. } => {
                    ctx_presence.set_presence(None, OnlineStatus::Online);
                }
                _ => {}
            }
        }
        // Channel closed (librespot shut down) — clear presence
        ctx_presence.set_presence(None, OnlineStatus::Online);
    });

    // ── 10. Watcher: clean up when the voice channel empties ──────────────
    let ctx2 = ctx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let still_connected = ctx2
                .cache
                .guild(guild_id)
                .map(|g| {
                    g.voice_states
                        .values()
                        .any(|vs| vs.channel_id == Some(channel_id) && vs.user_id != ctx2.cache.current_user().id)
                })
                .unwrap_or(false);

            if !still_connected {
                info!("Voice channel empty — shutting down Spotify Connect device");
                let _ = spirc.shutdown();
                let _ = manager.leave(guild_id).await;
                break;
            }
        }
    });
}

async fn reply_ephemeral(ctx: &Context, command: &CommandInteraction, content: &str) {
    let resp = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(content)
            .ephemeral(true),
    );
    if let Err(e) = command.create_response(&ctx.http, resp).await {
        error!("Failed to send interaction response: {e}");
    }
}

async fn follow_up(ctx: &Context, command: &CommandInteraction, content: &str) {
    use serenity::all::CreateInteractionResponseFollowup;
    let resp = CreateInteractionResponseFollowup::new()
        .content(content)
        .ephemeral(true);
    if let Err(e) = command.create_followup(&ctx.http, resp).await {
        error!("Failed to send follow-up: {e}");
    }
}
