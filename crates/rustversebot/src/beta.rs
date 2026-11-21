use crate::scheduler::SchedulerSettings;
use crate::{BotState, bot_templates::BotTemplateSender};
use anyhow::{Context, Result};
use rustverse_ai::beta::{AgentChange, Revision, revisions};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use teloxide::{prelude::*, types::InputFile};

pub(crate) struct BetaPages {
    client: reqwest::Client,
    generation: tokio::sync::Mutex<()>,
}

impl BetaPages {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(180))
                .build()?,
            generation: Default::default(),
        })
    }
}

async fn report(state: &BotState, rev: &Revision, agent: &AgentChange) -> Result<String> {
    let _guard = state.beta.generation.lock().await;
    if let Some(text) = state
        .db
        .beta_report(&rev.previous, &rev.current, agent.id)
        .await?
    {
        return Ok(text);
    }
    let key = rustverse_ai::api_key()?;
    let model =
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| rustverse_ai::DEFAULT_MODEL.to_owned());
    let text =
        rustverse_ai::beta::agent_report(&state.beta.client, rev, agent, &key, &model).await?;
    state
        .db
        .save_beta_report(&rev.previous, &rev.current, agent.id, &text)
        .await?;
    Ok(text)
}

fn by_agent(rows: &[Revision]) -> BTreeMap<u64, Vec<(&Revision, &AgentChange)>> {
    let mut out = BTreeMap::<_, Vec<_>>::new();
    for row in rows {
        for agent in &row.agents {
            out.entry(agent.id).or_default().push((row, agent));
        }
    }
    out
}

async fn render_page(
    state: &BotState,
    changes: &[(&Revision, &AgentChange)],
    prev: &str,
    cur: &str,
) -> Result<Vec<u8>> {
    let (_, agent) = changes.last().context("empty agent history")?;
    let mut page = format!("# {}\n\n## `{prev}` → `{cur}`\n", agent.name);
    for &(row, agent) in changes {
        let text = report(state, row, agent).await?;
        let (_, body) = text.split_once("\n\n## ").context("missing report title")?;
        let (_, body) = body.split_once("\n\n").context("missing report body")?;
        if row.previous != prev || row.current != cur {
            page.push_str(&format!("\n## {} → {}\n", row.previous, row.current));
        }
        page.push('\n');
        page.push_str(body);
        page.push('\n');
    }
    rustverse_images::beta_page(&page).await
}

async fn send_page(
    bot: &Bot,
    chat: ChatId,
    id: u64,
    png: Vec<u8>,
    settings: &SchedulerSettings,
) -> Result<()> {
    // The renderer returns a PNG; width and height are in the fixed IHDR fields.
    anyhow::ensure!(png.len() >= 24, "incomplete PNG");
    let width = u32::from_be_bytes(png[16..20].try_into()?);
    let height = u32::from_be_bytes(png[20..24].try_into()?);
    let photo = width.saturating_add(height) <= 10000
        && width.max(height) <= width.min(height).saturating_mul(20)
        && png.len() <= 10_000_000;
    let file = InputFile::memory(png).file_name(format!("beta-{id}.png"));
    let mut attempt = 1;
    loop {
        tokio::time::sleep(settings.request_spacing).await;
        let result = if photo {
            bot.send_photo(chat, file.clone()).await
        } else {
            bot.send_document(chat, file.clone()).await
        };
        match result {
            Ok(_) => return Ok(()),
            Err(error) if attempt < settings.retry_attempts => {
                let wait = match error {
                    teloxide::RequestError::RetryAfter(seconds) => seconds.duration(),
                    teloxide::RequestError::Network(_) => {
                        settings.retry_base_delay * (1u32 << (attempt - 1).min(8))
                    }
                    other => return Err(other.into()),
                };
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub async fn command(bot: &Bot, chat: ChatId, args: &str, state: &BotState) -> Result<()> {
    let sender = BotTemplateSender::new(bot, &state.templates);
    let args: Vec<_> = args.split_whitespace().collect();
    let (prev, cur) = match args.as_slice() {
        [] => (None, None),
        [p, c] => (Some(*p), Some(*c)),
        _ => {
            sender.send_message(chat, "beta_usage", &()).await?;
            return Ok(());
        }
    };
    let run = async {
        let rows = revisions(&state.beta.client, prev, cur).await?;
        let prev = &rows.first().context("empty changelog")?.previous;
        let cur = &rows.last().unwrap().current;
        let agents = by_agent(&rows);
        let context = json!({"previous":prev,"current":cur,"count":agents.len()});
        if agents.is_empty() {
            sender.send_message(chat, "beta_empty", &context).await?;
            return Ok(());
        }
        sender.send_message(chat, "beta_progress", &context).await?;
        let mut failed = Vec::new();
        let settings = SchedulerSettings::from_env();
        for (id, changes) in agents {
            let result = async {
                let png = render_page(state, &changes, prev, cur).await?;
                send_page(bot, chat, id, png, &settings).await
            }
            .await;
            if let Err(error) = result {
                log::warn!("Beta page {id} failed: {error:#}");
                failed.push(id.to_string());
            }
        }
        anyhow::ensure!(
            failed.is_empty(),
            "Не удалось отправить страницы: {}",
            failed.join(", ")
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = run {
        sender
            .send_message(chat, "error_generic", &json!({"error":error.to_string()}))
            .await?;
    }
    Ok(())
}

pub async fn poll(bot: &Bot, state: &BotState, settings: &SchedulerSettings) -> Result<()> {
    if state.db.get_distinct_chats().await?.is_empty() || rustverse_ai::api_key().is_err() {
        return Ok(());
    }
    let latest = revisions(&state.beta.client, None, None)
        .await?
        .pop()
        .context("empty changelog")?;
    let mut groups = BTreeMap::<String, Vec<i64>>::new();
    for (chat, cursor) in state.db.beta_cursors(&latest.current).await? {
        groups.entry(cursor).or_default().push(chat);
    }
    for (cursor, chats) in groups {
        let rows = if cursor == latest.previous {
            vec![latest.clone()]
        } else {
            match revisions(&state.beta.client, Some(&cursor), Some(&latest.current)).await {
                Ok(rows) => rows,
                Err(e) => {
                    log::warn!("Cannot load beta revisions after {cursor}: {e:#}");
                    continue;
                }
            }
        };
        let mut active = chats;
        for row in rows {
            if active.is_empty() {
                break;
            }
            deliver_revision(bot, state, &row, &mut active, settings).await;
        }
    }
    Ok(())
}

async fn deliver_revision(
    bot: &Bot,
    state: &BotState,
    row: &Revision,
    active: &mut Vec<i64>,
    settings: &SchedulerSettings,
) {
    let mut failed = HashSet::new();
    for agent in &row.agents {
        let mut pending = Vec::new();
        for &chat in active.iter() {
            match state.db.beta_delivered(chat, &row.current, agent.id).await {
                Ok(false) => pending.push(chat),
                Ok(true) => (),
                Err(error) => {
                    log::warn!("Beta delivery lookup for chat {chat}: {error:#}");
                    failed.insert(chat);
                }
            }
        }
        if pending.is_empty() {
            continue;
        }
        let png = match render_page(state, &[(row, agent)], &row.previous, &row.current).await {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("Beta page {} {}: {e:#}", row.current, agent.id);
                failed.extend(pending);
                continue;
            }
        };
        for chat in pending {
            let delivery = async {
                send_page(bot, ChatId(chat), agent.id, png.clone(), settings).await?;
                state
                    .db
                    .mark_beta_delivered(chat, &row.current, agent.id)
                    .await
            }
            .await;
            if let Err(error) = delivery {
                log::warn!("Beta page {} for chat {chat}: {error:#}", agent.id);
                failed.insert(chat);
            }
        }
    }
    active.retain(|chat| !failed.contains(chat));
    for &chat in active.iter() {
        if let Err(error) = state
            .db
            .advance_beta(chat, &row.previous, &row.current)
            .await
        {
            log::warn!("Updating beta cursor for chat {chat}: {error:#}");
        }
    }
}
