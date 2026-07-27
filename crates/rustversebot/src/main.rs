mod bot_templates;
mod db;
mod handlers;
mod scheduler;
mod templates;
mod web;

use anyhow::Context;
use std::{net::SocketAddr, sync::Arc};
use teloxide::{prelude::*, utils::command::BotCommands};

/// All bot commands, matched case-insensitively.
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    /// Show the welcome message and usage.
    Start,
    /// Register a ZZZ UID for tracking: `/register <uid>`.
    Register(String),
    /// Remove a ZZZ UID from tracking: `/unregister <uid>`.
    Unregister(String),
    /// Show the current endgame season status.
    Status,
    /// Show the next pair of mode seasons.
    Next,
    /// Show the current pair of mode seasons.
    Current,
    /// Show the previous pair of mode seasons.
    Previous,
    /// Show the Deadly Assault leaderboard.
    #[command(rename = "top_da")]
    TopDeadly,
    /// Show the Shiyu Defense leaderboard.
    #[command(rename = "top_sd")]
    TopShiyu,
    /// Update the HoYoLAB cookie: `/cookie <cookie_string>`.
    Cookie(String),
    /// Fetch all tracked users again. This command requires administrator access.
    #[command(rename = "refetch_all")]
    RefetchAll,
    /// Fetch one UID again: `/refetch <uid>`.
    #[command(rename = "refetch")]
    RefetchUid(String),
    /// Show Deadly Assault details for a UID: `/da <uid>`.
    #[command(rename = "da")]
    Da(String),
    /// Show Shiyu Defense details for a UID: `/shiyu <uid>`.
    #[command(rename = "shiyu")]
    Shiyu(String),
    /// List all registered UIDs in this chat.
    #[command(rename = "uids")]
    Uids,
}

/// Shared bot state accessible from all handlers and the scheduler.
pub struct BotState {
    pub db: db::Db,
    pub templates: templates::TemplateEngine,
    /// Telegram user ID of the bot administrator.
    pub admin_id: i64,
    pub public_web_url: Option<String>,
    pub nanoka: nanoka::NanokaClient,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    log::info!("Starting ZZZ Endgame Tracker Bot");

    let admin_id = std::env::var("BOT_ADMIN_ID")
        .context("BOT_ADMIN_ID is required")?
        .parse::<i64>()
        .context("BOT_ADMIN_ID must be a valid Telegram user ID")?;
    let database_url =
        std::env::var("TURSO_DATABASE_URL").context("TURSO_DATABASE_URL is required")?;
    if database_url.trim().is_empty() {
        anyhow::bail!("TURSO_DATABASE_URL must not be empty");
    }

    let db = db::Db::connect(&database_url).await?;
    log::info!("Local libSQL database ready");

    // Set up template engine
    let templates = templates::TemplateEngine::new()?;
    log::info!("Templates loaded");

    let public_web_url = std::env::var("BOT_PUBLIC_WEB_URL")
        .ok()
        .map(|value| value.trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty());
    let state = Arc::new(BotState {
        db,
        templates,
        admin_id,
        public_web_url,
        nanoka: nanoka::NanokaClient::new().with_lang("en"),
    });

    let bot = Bot::from_env();
    log::info!("Bot connected");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn the background scheduler
    let sched_bot = bot.clone();
    let sched_state = state.clone();
    let scheduler_task = tokio::spawn(async move {
        if let Err(e) = scheduler::run(sched_bot, sched_state, shutdown_rx).await {
            log::error!("Scheduler crashed: {e}");
        }
    });

    let web_task = match std::env::var("BOT_WEB_BIND") {
        Ok(value) => {
            let bind = value
                .parse::<SocketAddr>()
                .with_context(|| format!("BOT_WEB_BIND must be a socket address, got {value:?}"))?;
            let web_state = state.clone();
            let web_shutdown = shutdown_tx.subscribe();
            Some(tokio::spawn(async move {
                if let Err(error) = web::serve(web_state, bind, web_shutdown).await {
                    log::error!("Web dashboard crashed: {error:#}");
                }
            }))
        }
        Err(std::env::VarError::NotPresent) => {
            log::info!("Web dashboard disabled (BOT_WEB_BIND is not set)");
            None
        }
        Err(error) => return Err(error).context("could not read BOT_WEB_BIND"),
    };

    // Set up the command handler dispatch tree
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter(handlers::is_missing_detail_command)
                .endpoint(handlers::missing_detail_command_handler),
        )
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handlers::command_handler),
        )
        .branch(Update::filter_callback_query().endpoint(handlers::callback_handler));

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .build();
    let dispatcher = dispatcher.dispatch();
    tokio::pin!(dispatcher);

    tokio::select! {
        _ = &mut dispatcher => log::warn!("Telegram dispatcher stopped"),
        result = shutdown_signal() => {
            if let Err(error) = result {
                log::error!("Shutdown signal listener failed: {error}");
            }
        }
    }

    let _ = shutdown_tx.send(true);
    if let Err(error) = scheduler_task.await {
        log::error!("Scheduler task could not be joined: {error}");
    }
    if let Some(task) = web_task
        && let Err(error) = task.await
    {
        log::error!("Web dashboard task could not be joined: {error}");
    }

    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
