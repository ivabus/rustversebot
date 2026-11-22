#![recursion_limit = "256"]

mod agent;
mod agent_access;
mod ai_jobs;
mod beta;
mod bot_templates;
mod db;
mod handlers;
mod promocodes;
mod prompt;
mod scheduler;
mod season_time;
mod telegram_html;
mod templates;
mod web;

use anyhow::Context;
use std::{net::SocketAddr, sync::Arc};
use teloxide::{prelude::*, utils::command::BotCommands};

/// All bot commands, matched case-insensitively.
#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", parse_with = command_text)]
enum Command {
    /// Show the welcome message and usage.
    Start,
    /// Register a ZZZ UID for tracking: `/register <uid>`.
    Register(String),
    /// Remove a ZZZ UID from tracking: `/unregister <uid>`.
    Unregister(String),
    /// Show the current endgame season status.
    Status,
    /// Show ZZZ promo codes with redemption links.
    #[command(aliases = ["codes", "promo"], hide_aliases)]
    Promocodes,
    /// Show the next pair of mode seasons.
    Next,
    /// Show the current pair of mode seasons.
    Current,
    /// Show the previous pair of mode seasons.
    Previous,
    /// Show an infographic for a current or next endgame season: `/endgame [id]`.
    Endgame(String),
    /// Изменения агентов за последнюю бету: /betadiff [prev cur].
    BetaDiff(String),
    /// Задать вопрос о ZZZ текстом или в подписи к скриншотам.
    Prompt(String),
    /// Выбрать модель ИИ или включить /model auto (для администратора).
    Model(String),
    /// Сравнить версии беты или релиза по данным GachaBase.
    Diff(String),
    /// Сбросить минутный лимит пользователя (для администратора).
    ResetLimit(String),
    /// Журнал пользователей ИИ (для администратора).
    AiUsers(String),
    /// Заблокировать доступ пользователя к ИИ (для администратора).
    AiBan(String),
    /// Восстановить доступ пользователя к ИИ (для администратора).
    AiUnban(String),
    /// Show Deadly Assault details: /da [uid].
    #[command(rename = "da")]
    Da(String),
    /// Show Shiyu Defense details: /shiyu [uid].
    #[command(rename = "shiyu")]
    Shiyu(String),
    /// Show the Deadly Assault leaderboard.
    #[command(rename = "top_da")]
    TopDeadly,
    /// Show the Shiyu Defense leaderboard.
    #[command(rename = "top_shiyu", alias = "top_sd", hide_aliases)]
    TopShiyu,
    /// Update the HoYoLAB cookie: `/cookie <cookie_string>`.
    Cookie(String),
    /// Fetch all tracked users again. This command requires administrator access.
    #[command(rename = "refetch_all")]
    RefetchAll,
    /// Fetch one UID again: `/refetch <uid>`.
    #[command(rename = "refetch")]
    RefetchUid(String),
    /// List all registered UIDs in this chat.
    #[command(rename = "uids")]
    Uids,
}

// Every command with an argument takes the complete text after the command.
fn command_text(text: String) -> Result<(String,), teloxide::utils::command::ParseError> {
    Ok((text,))
}

fn telegram_menu() -> Vec<teloxide::types::BotCommand> {
    let mut commands = Command::bot_commands();
    for command in &mut commands {
        command.command = command.command.trim_start_matches('/').to_owned();
    }
    commands
}

/// Shared bot state accessible from all handlers and the scheduler.
pub struct BotState {
    pub(crate) ai_jobs: tokio::sync::mpsc::Sender<ai_jobs::Job>,
    pub(crate) beta: beta::BetaPages,
    pub db: db::Db,
    pub templates: templates::TemplateEngine,
    /// Telegram user ID of the bot administrator.
    pub admin_id: i64,
    pub public_web_url: Option<String>,
    pub nanoka: nanoka::NanokaClient,
    pub http: reqwest::Client,
}

impl BotState {
    pub(crate) async fn research_setting(&self) -> anyhow::Result<String> {
        Ok(self
            .db
            .model_override()
            .await?
            .or_else(|| std::env::var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| "auto".into()))
    }

    pub(crate) async fn ai_model(&self) -> anyhow::Result<rustverse_ai::ModelSelection> {
        let setting = self.research_setting().await?;
        if setting == "auto" {
            return std::env::var("OPENAI_MODEL")
                .ok()
                .filter(|s| s != "auto")
                .unwrap_or_else(|| rustverse_ai::DEFAULT_MODEL.into())
                .parse();
        }
        setting.parse()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let _langfuse = rustverse_ai::telemetry::init("rustversebot")?;
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
    let (ai_jobs, ai_queue) = ai_jobs::channel();
    let state = Arc::new(BotState {
        ai_jobs,
        beta: beta::BetaPages::new()?,
        db,
        templates,
        admin_id,
        public_web_url,
        nanoka: nanoka::NanokaClient::new().with_lang("en"),
        http: reqwest::Client::builder()
            .user_agent(concat!("rustversebot/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
    });

    let bot = Bot::from_env();
    if let Err(error) = bot.set_my_commands(telegram_menu()).await {
        log::warn!("Could not update Telegram command menu: {error}");
    }
    log::info!("Bot connected");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let ai_task = tokio::spawn(ai_jobs::run(ai_queue, shutdown_rx.clone()));

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
                .filter(prompt::media::is_media)
                .endpoint(prompt::media::receive),
        )
        .branch(
            Update::filter_message()
                .filter(handlers::is_missing_detail_command)
                .endpoint(handlers::missing_detail_command_handler),
        )
        .branch(
            Update::filter_message()
                .filter(handlers::is_missing_endgame_command)
                .endpoint(handlers::missing_endgame_command_handler),
        )
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handlers::command_handler),
        )
        .branch(
            Update::filter_message()
                .filter(prompt::is_follow_up)
                .endpoint(prompt::follow_up),
        )
        .branch(Update::filter_callback_query().endpoint(handlers::callback_handler));

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            state,
            prompt::media::Albums::default(),
            shutdown_tx.subscribe()
        ])
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
    ai_task.await?;
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

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn model_command_keeps_the_reasoning_suffix_in_group_chats() {
        assert!(
            matches!(Command::parse("/model", "bot"), Ok(Command::Model(args)) if args.is_empty())
        );
        assert!(
            matches!(Command::parse("/model@bot gpt-6.2-sol@medium", "bot"), Ok(Command::Model(args)) if args=="gpt-6.2-sol@medium")
        );
    }

    #[test]
    fn promocode_aliases_resolve_in_private_and_group_chats() {
        for command in ["/promocodes", "/codes", "/promo"] {
            for text in [command.to_owned(), format!("{command}@bot")] {
                assert!(matches!(
                    Command::parse(&text, "bot"),
                    Ok(Command::Promocodes)
                ));
            }
        }
    }

    #[test]
    fn detail_and_top_commands_match_the_public_menu() {
        let menu = telegram_menu();
        for name in ["da", "shiyu", "top_da", "top_shiyu"] {
            assert!(menu.iter().any(|command| command.command == name));
        }
        assert!(matches!(
            Command::parse("/da 123", "bot"),
            Ok(Command::Da(_))
        ));
        assert!(matches!(
            Command::parse("/shiyu 123", "bot"),
            Ok(Command::Shiyu(_))
        ));
        assert!(matches!(
            Command::parse("/top_da", "bot"),
            Ok(Command::TopDeadly)
        ));
        for command in ["/top_shiyu@bot", "/top_sd"] {
            assert!(matches!(
                Command::parse(command, "bot"),
                Ok(Command::TopShiyu)
            ));
        }
        assert!(
            menu.iter()
                .all(|c| !c.command.starts_with('/') && c.command != "top_sd")
        );
    }
}
