use std::{sync::Arc, time::Duration};

use librespot_playback::player::PlayerEvent;
use serenity::all::{
    ActivityData, Color, CommandInteraction, Context, CreateEmbed, CreateEmbedAuthor,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    OnlineStatus,
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

    let voice_ch_id = ctx
        .cache
        .guild(guild_id)
        .and_then(|g| {
            g.voice_states
                .get(&command.user.id)
                .and_then(|vs| vs.channel_id)
        });

    let voice_ch_id = match voice_ch_id {
        Some(id) => id,
        None => {
            reply_ephemeral(ctx, command, "You need to be in a voice channel first.").await;
            return;
        }
    };

    // Text channel where the command was run — used for public announcements.
    let text_ch_id = command.channel_id;

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
    info!("Spotify user: {spotify_name} — joining voice channel {voice_ch_id}");

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

    let handler_lock = match manager.join(guild_id, voice_ch_id).await {
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

    // Public channel announcement
    let _ = text_ch_id
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
    let bot_name_jam = device_name.clone();
    tokio::spawn(async move {
        let mut rx = jam_url_rx;
        while let Some(url) = rx.recv().await {
            info!("Jam session started: {url}");
            let _ = text_ch_id
                .send_message(
                    &ctx_jam.http,
                    CreateMessage::new().content(format!(
                        "🎵 **{bot_name_jam}** — [Join the Jam]({url})"
                    )),
                )
                .await;
        }
    });

    // ── 9. Now-playing embed + presence update on every track change ───────
    let ctx_np = ctx.clone();
    let device_name_np = device_name.clone();
    tokio::spawn(async move {
        let mut events = event_channel;
        while let Some(event) = events.recv().await {
            match event {
                PlayerEvent::TrackChanged { audio_item } => {
                    // ── Extract metadata ──────────────────────────────────
                    let track = audio_item.name.clone();

                    let (artist, album) = match &audio_item.unique_fields {
                        librespot_metadata::audio::UniqueFields::Track { artists, album, .. } => (
                            artists.0.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),
                            album.clone(),
                        ),
                        librespot_metadata::audio::UniqueFields::Local { artists, album, .. } => (
                            artists.as_deref().unwrap_or("Unknown artist").to_string(),
                            album.as_deref().unwrap_or("").to_string(),
                        ),
                        librespot_metadata::audio::UniqueFields::Episode { show_name, .. } => {
                            (show_name.clone(), String::new())
                        }
                    };

                    // Largest cover image available
                    let cover_url = audio_item
                        .covers
                        .iter()
                        .max_by_key(|c| c.width)
                        .map(|c| c.url.clone())
                        .unwrap_or_default();

                    // spotify:track:ID → https://open.spotify.com/track/ID
                    let spotify_url = audio_item
                        .uri
                        .strip_prefix("spotify:")
                        .map(|rest| {
                            format!("https://open.spotify.com/{}", rest.replacen(':', "/", 1))
                        })
                        .unwrap_or_default();

                    // mm:ss duration
                    let duration = {
                        let total = audio_item.duration_ms / 1000;
                        format!("{}:{:02}", total / 60, total % 60)
                    };

                    let explicit_tag = if audio_item.is_explicit { " 🅴" } else { "" };

                    let status = format!("{track} · {artist}");
                    info!("Now playing: {status}");

                    // ── Discord presence ──────────────────────────────────
                    ctx_np.set_presence(
                        Some(ActivityData::listening(&status)),
                        OnlineStatus::Online,
                    );

                    // ── Now-playing embed ─────────────────────────────────
                    let mut embed = CreateEmbed::new()
                        .author(CreateEmbedAuthor::new(&artist))
                        .title(format!("{track}{explicit_tag}"))
                        .color(Color::from_rgb(30, 215, 96)) // Spotify green
                        .footer(CreateEmbedFooter::new(format!(
                            "🎧 {device_name_np}  ·  {duration}"
                        )));

                    if !spotify_url.is_empty() {
                        embed = embed.url(&spotify_url);
                    }
                    if !album.is_empty() {
                        embed = embed.description(format!("*{album}*"));
                    }
                    if !cover_url.is_empty() {
                        embed = embed.thumbnail(&cover_url);
                    }

                    let _ = text_ch_id
                        .send_message(&ctx_np.http, CreateMessage::new().embed(embed))
                        .await;
                }
                PlayerEvent::Stopped { .. } => {
                    ctx_np.set_presence(None, OnlineStatus::Online);
                }
                _ => {}
            }
        }
        ctx_np.set_presence(None, OnlineStatus::Online);
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
                    g.voice_states.values().any(|vs| {
                        vs.channel_id == Some(voice_ch_id)
                            && vs.user_id != ctx2.cache.current_user().id
                    })
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
