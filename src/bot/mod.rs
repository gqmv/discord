pub mod commands;

use std::sync::Arc;

use serenity::{
    all::{
        Client, CreateCommand, GatewayIntents, Interaction, Ready,
    },
    async_trait,
    prelude::{Context, EventHandler},
};
use songbird::SerenityInit;
use tracing::{error, info};

use crate::{config::Config, store::Store};

struct Handler {
    cfg: Arc<Config>,
    store: Arc<Store>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected and ready", ready.user.name);

        // Store the bot's username so commands can use it as the Spotify Connect device name.
        *self.store.bot_name.write().await = ready.user.name.clone();

        // Register the slash command on the configured guild (instant, no propagation delay)
        let guild_id = serenity::model::id::GuildId::new(self.cfg.discord_guild_id);
        let cmd = CreateCommand::new("start-spotify")
            .description("Connect your Spotify and start playing in your voice channel");

        match guild_id.create_command(&ctx.http, cmd).await {
            Ok(c) => info!("Registered slash command: /{}", c.name),
            Err(e) => error!("Failed to register command: {e}"),
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            if command.data.name == "start-spotify" {
                commands::start_spotify::run(
                    &ctx,
                    &command,
                    self.cfg.clone(),
                    self.store.clone(),
                )
                .await;
            }
        }
    }
}

pub async fn build_client(cfg: Arc<Config>, store: Arc<Store>) -> anyhow::Result<Client> {
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MESSAGES;

    let client = Client::builder(&cfg.discord_token, intents)
        .event_handler(Handler {
            cfg,
            store,
        })
        .register_songbird()
        .await?;

    Ok(client)
}
