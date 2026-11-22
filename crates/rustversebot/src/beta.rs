use crate::{
    BotState,
    agent::{self, AgentRun, ChatAgent, MessageAddress, Outcome, Status},
    bot_templates::{BotTemplateSender, MESSAGE_LIMIT},
    scheduler::SchedulerSettings,
    telegram_html,
};
use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use rustverse_ai::PromptProgress;
use rustverse_ai::beta::{AgentChange, Branch, Revision, branch_revisions, revisions};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use teloxide::prelude::*;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, oneshot, watch};

const PARALLEL_REPORTS: usize = 3;

pub(crate) struct BetaPages {
    pub(crate) client: reqwest::Client,
    slots: Semaphore,
    agents: Mutex<HashMap<u64, Weak<AsyncMutex<()>>>>,
    manual_limit: usize,
}

impl BetaPages {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(180))
                .build()?,
            slots: Semaphore::new(PARALLEL_REPORTS),
            agents: Default::default(),
            manual_limit: match std::env::var("BETA_MAX_NEW_REPORTS") {
                Ok(value) => value.parse().context("invalid BETA_MAX_NEW_REPORTS")?,
                Err(std::env::VarError::NotPresent) => 10,
                Err(error) => return Err(error.into()),
            },
        })
    }

    // Jobs from commands and the scheduler share the same report cache.
    // Wait for a matching character before reserving a generation slot.
    async fn lock_agent(&self, id: u64) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut agents = self.agents.lock().unwrap();
            agents.retain(|_, lock| lock.strong_count() > 0);
            match agents.get(&id).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(AsyncMutex::new(()));
                    agents.insert(id, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }
}

async fn cached_report(
    state: &BotState,
    rev: &Revision,
    agent: &AgentChange,
    model: &str,
) -> Result<Option<String>> {
    state
        .db
        .beta_report(
            &rev.previous,
            &rev.current,
            agent.id,
            model,
            &report_version(agent),
        )
        .await
}

fn report_version(agent: &AgentChange) -> String {
    let mut version = rustverse_ai::prompt_version().to_owned();
    if !agent.forms.is_empty() {
        // A cached single-avatar report cannot stand in for a combined card.
        version.push_str(&format!("/forms-v1/{}", agent.status));
        for form in &agent.forms {
            version.push_str(&format!("/{}-{}", form.id, form.status));
        }
    }
    version
}

async fn report(
    state: &BotState,
    rev: &Revision,
    agent: &AgentChange,
    model: &str,
    progress: Option<watch::Sender<PromptProgress>>,
) -> Result<String> {
    if let Some(text) = cached_report(state, rev, agent, model).await? {
        return Ok(text);
    }
    let source = if progress.is_some() {
        "telegram"
    } else {
        "scheduler"
    };
    let _guard = state.beta.lock_agent(agent.id).await;
    if let Some(text) = cached_report(state, rev, agent, model).await? {
        return Ok(text);
    }
    let _slot = state.beta.slots.acquire().await?;
    let key = rustverse_ai::api_key()?;
    let span =
        rustverse_ai::telemetry::diff_span(source, model, &rev.previous, &rev.current, agent.id);
    let text = rustverse_ai::telemetry::observe(
        span,
        rustverse_ai::beta::report_with_progress(
            &state.beta.client,
            rev,
            agent,
            &key,
            model,
            progress,
        ),
    )
    .await?;
    state
        .db
        .save_beta_report(
            &rev.previous,
            &rev.current,
            agent.id,
            model,
            &report_version(agent),
            &text,
        )
        .await?;
    Ok(text)
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum DiffStage {
    Prepare,
    Report,
    Send,
}

fn diff_status(
    previous: Option<&str>,
    current: Option<&str>,
    stage: DiffStage,
    character: Option<&str>,
) -> Status<'static> {
    Status::Activity {
        template: "diff_status",
        context: json!({ "previous":previous, "current":current, "stage":stage, "character":character }),
    }
}

async fn send_report_part(
    sender: &BotTemplateSender<'_>,
    chat: ChatId,
    thread: Option<teloxide::types::ThreadId>,
    part: &str,
    settings: &SchedulerSettings,
) -> Result<()> {
    let mut attempt = 1;
    loop {
        tokio::time::sleep(settings.request_spacing).await;
        let result = sender
            .send_to_thread(
                chat,
                thread,
                "prompt_answer",
                &json!({"text":part, "footer":null}),
            )
            .await;
        let error = match result {
            Ok(_) => return Ok(()),
            Err(error) if attempt >= settings.retry_attempts => return Err(error),
            Err(error) => error,
        };
        let wait = match error.downcast_ref::<teloxide::RequestError>() {
            Some(teloxide::RequestError::RetryAfter(wait)) => wait.duration(),
            Some(teloxide::RequestError::Network(_)) => {
                settings.retry_base_delay * (1u32 << (attempt - 1).min(8))
            }
            _ => return Err(error),
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

// Run preparation alongside Telegram delivery. Each receiver owns its position
// in the revision, so a faster character cannot overtake an earlier one.
fn prepare_reports<T>(
    jobs: Vec<impl Future<Output = T>>,
) -> (impl Future<Output = Result<()>>, Vec<oneshot::Receiver<T>>) {
    let (jobs, reports): (Vec<_>, Vec<_>) = jobs
        .into_iter()
        .map(|job| {
            let (ready, report) = oneshot::channel();
            (
                async move {
                    let _ = ready.send(job.await);
                },
                report,
            )
        })
        .unzip();
    let prepare = async move {
        stream::iter(jobs)
            .for_each_concurrent(PARALLEL_REPORTS, |job| job)
            .await;
        Ok(())
    };
    (prepare, reports)
}

async fn send_reports(
    bot: &Bot,
    state: &BotState,
    row: &Revision,
    model: &str,
    chat: ChatId,
    settings: &SchedulerSettings,
    mut status: Option<&mut AgentRun<'_>>,
) -> Result<()> {
    let automatic = status.is_none();
    let thread = status.as_ref().and_then(|s| s.address().thread);
    let confirmed = if automatic {
        state.db.delivered_beta_agents(chat.0, &row.current).await?
    } else {
        HashSet::new()
    };
    let sender = BotTemplateSender::new(bot, &state.templates);
    let mut jobs = Vec::new();
    let mut deliveries = Vec::new();
    for agent in &row.agents {
        let mut outstanding = agent
            .changed_members()
            .filter(|m| !confirmed.contains(&m.id));
        let Some(first) = outstanding.next() else {
            continue;
        };
        let ids: Vec<_> = std::iter::once(first.id)
            .chain(outstanding.map(|m| m.id))
            .collect();
        let (tx, progress) = watch::channel(PromptProgress::default());
        deliveries.push((agent, ids, progress));
        jobs.push(prepare_report_parts(
            state,
            row,
            agent,
            first,
            model,
            chat,
            (!automatic).then_some(tx),
        ));
    }
    let (prepare, reports) = prepare_reports(jobs);
    let deliver = async {
        for ((agent, ids, progress), result) in deliveries.into_iter().zip(reports) {
            let next = async { result.await.context("Report generation stopped")? };
            let parts = match status.as_deref_mut() {
                Some(run) => {
                    run.track_progress(
                        diff_status(
                            Some(&row.previous),
                            Some(&row.current),
                            DiffStage::Report,
                            Some(&agent.name),
                        ),
                        progress,
                        next,
                    )
                    .await?
                }
                None => next.await?,
            };
            if let Some(status) = status.as_deref_mut() {
                status
                    .show(&diff_status(
                        Some(&row.previous),
                        Some(&row.current),
                        DiffStage::Send,
                        Some(&agent.name),
                    ))
                    .await?;
            }
            for (i, part) in parts.iter().enumerate() {
                send_report_part(&sender, chat, thread, part, settings).await?;
                if automatic {
                    let remaining = &parts[i + 1..];
                    if remaining.is_empty() {
                        state
                            .db
                            .mark_beta_delivered(chat.0, &row.current, &ids)
                            .await?;
                    } else {
                        state
                            .db
                            .save_pending_beta_text(chat.0, &row.current, ids[0], remaining)
                            .await?;
                    }
                }
            }
        }
        anyhow::Ok(())
    };
    tokio::try_join!(prepare, deliver)?;
    Ok(())
}

async fn prepare_report_parts(
    state: &BotState,
    row: &Revision,
    agent: &AgentChange,
    first: &AgentChange,
    model: &str,
    chat: ChatId,
    progress: Option<watch::Sender<PromptProgress>>,
) -> Result<Vec<String>> {
    let automatic = progress.is_none();
    if automatic
        && let Some(remaining) = state
            .db
            .pending_beta_text(chat.0, &row.current, first.id)
            .await?
    {
        return Ok(remaining);
    }
    let text = report(state, row, agent, model, progress).await?;
    let source = state.templates.render("beta_source", &json!({
        "comparison": !automatic,
        "hotfix": row.previous.rsplit_once('_').map(|p| p.0) == row.current.rsplit_once('_').map(|p| p.0),
        "release": row.current.starts_with("release_"),
        "url": rustverse_ai::beta::diff_url(row, first)?.as_str(),
    }))?;
    Ok(telegram_html::chunks(
        "",
        &format!("{text}\n\n{}", source.text),
        MESSAGE_LIMIT,
    ))
}

pub async fn diff_command(
    bot: &Bot,
    address: MessageAddress,
    user: Option<&teloxide::types::User>,
    args: &str,
    state: &std::sync::Arc<BotState>,
) -> Result<()> {
    if args.trim().is_empty() {
        return picker(bot, address, user, "menu", state).await;
    }
    command(bot, address, user, args, state, true).await
}

pub async fn picker(
    bot: &Bot,
    address: MessageAddress,
    user: Option<&teloxide::types::User>,
    action: &str,
    state: &std::sync::Arc<BotState>,
) -> Result<()> {
    use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
    if !crate::agent_access::check_chat(bot, address, crate::db::AiFeature::Diff, state).await? {
        return Ok(());
    }
    let sender = BotTemplateSender::new(bot, &state.templates);
    let run = async {
        let fields: Vec<_> = action.split(':').collect();
        let (step, buttons, previous, release) = match fields.as_slice() {
            ["menu"] => {
                let mut buttons = Vec::new();
                for branch in [Branch::Beta, Branch::Release] {
                    let label = state
                        .templates
                        .render("diff_button", &json!({"kind":branch.name()}))?
                        .text;
                    buttons.push(InlineKeyboardButton::callback(
                        label,
                        format!("diff:branch:{}", branch.name()),
                    ));
                }
                ("branch", buttons, "", false)
            }
            ["pair", before, after] => {
                return command(
                    bot,
                    address,
                    user,
                    &format!("{before} {after}"),
                    state,
                    true,
                )
                .await;
            }
            ["branch", _] | ["from", _] => {
                let token = fields[1];
                let branch = Branch::parse(token.split('_').next().unwrap_or_default())?;
                let recent: Vec<_> = rustverse_ai::beta::recent_revisions(&state.beta.client)
                    .await?
                    .into_iter()
                    .filter(|v| v.split('_').next() == Some(branch.name()))
                    .collect();
                let selecting_end = fields[0] == "from";
                let choices = if selecting_end {
                    let i = recent
                        .iter()
                        .position(|s| s == token)
                        .context("Выбранная версия больше не в списке /diff")?;
                    &recent[..i]
                } else {
                    recent.get(1..).unwrap_or_default()
                };
                let mut buttons = Vec::new();
                for version in choices {
                    let (_, number, design) =
                        revision_parts(version).context("invalid revision")?;
                    let label = state
                        .templates
                        .render("diff_button", &json!({"version":number,"design":design}))?
                        .text;
                    let data = if selecting_end {
                        format!("diff:pair:{token}:{version}")
                    } else {
                        format!("diff:from:{version}")
                    };
                    if data.len() <= 64 {
                        buttons.push(InlineKeyboardButton::callback(label, data));
                    }
                }
                (
                    if selecting_end { "end" } else { "start" },
                    buttons,
                    token,
                    branch == Branch::Release,
                )
            }
            _ => ("invalid", vec![], "", false),
        };
        if buttons.is_empty() {
            sender
                .send_to_thread(
                    address.chat,
                    address.thread,
                    "diff_picker",
                    &json!({"step":"invalid"}),
                )
                .await?;
        } else {
            let keyboard = InlineKeyboardMarkup::new(buttons.chunks(2).map(|pair| pair.to_vec()));
            sender
                .send_message_with_keyboard(
                    address.chat,
                    address.thread,
                    "diff_picker",
                    &json!({"step":step,"release":release,"previous":previous}),
                    keyboard,
                )
                .await?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = run {
        sender
            .send_to_thread(
                address.chat,
                address.thread,
                "error_generic",
                &json!({"error":error.to_string()}),
            )
            .await?;
    }
    Ok(())
}

fn revision_parts(revision: &str) -> Option<(&str, &str, &str)> {
    let (branch, tail) = revision.split_once('_')?;
    let (version, design) = tail.split_once('_')?;
    Some((branch, version, design))
}

pub async fn command(
    bot: &Bot,
    address: MessageAddress,
    user: Option<&teloxide::types::User>,
    args: &str,
    state: &std::sync::Arc<BotState>,
    compare: bool,
) -> Result<()> {
    let chat = address.chat;
    if !crate::agent_access::check_chat(bot, address, crate::db::AiFeature::Diff, state).await? {
        return Ok(());
    }
    let sender = BotTemplateSender::new(bot, &state.templates);
    let args: Vec<_> = args.split_whitespace().collect();
    let (prev, cur) = match args.as_slice() {
        [] => (None, None),
        [p, c] => (Some(*p), Some(*c)),
        _ => {
            sender
                .send_to_thread(chat, address.thread, "beta_usage", &())
                .await?;
            return Ok(());
        }
    };
    agent::launch(
        bot,
        address,
        user,
        if compare { "diff" } else { "betadiff" },
        state,
        Diff {
            previous: prev.map(str::to_owned),
            current: cur.map(str::to_owned),
            compare,
            admin: crate::agent_access::is_admin(user, state),
        },
    )
    .await
}

struct Diff {
    previous: Option<String>,
    current: Option<String>,
    compare: bool,
    admin: bool,
}

impl ChatAgent for Diff {
    fn initial_status(&self) -> Status<'_> {
        diff_status(
            self.previous.as_deref(),
            self.current.as_deref(),
            DiffStage::Prepare,
            None,
        )
    }

    async fn execute(&self, status: &mut AgentRun<'_>, state: &BotState) -> Result<Outcome> {
        let model = status
            .model()
            .parse::<rustverse_ai::ModelSelection>()?
            .cache_key();
        let chat = status.address().chat;
        let bot = status.bot().clone();
        let mut rows = revisions(
            &state.beta.client,
            self.previous.as_deref(),
            self.current.as_deref(),
        )
        .await?;
        if self.compare && rows.len() > 1 {
            rows = vec![rustverse_ai::beta::comparison_range(&rows)?];
        }
        let prev = rows.first().context("empty changelog")?.previous.clone();
        let cur = rows.last().unwrap().current.clone();
        status
            .show(&diff_status(
                Some(&prev),
                Some(&cur),
                DiffStage::Prepare,
                None,
            ))
            .await?;
        for row in &mut rows {
            row.group_forms(&state.beta.client).await?;
        }
        let settings = SchedulerSettings::from_env();
        if rows.iter().all(|row| row.agents.is_empty()) {
            return state
                .templates
                .render("beta_empty", &json!({"previous":prev,"current":cur}))
                .map(Outcome::Notice);
        }
        // Check the whole range before sending the first report.
        if !self.admin
            && rows.iter().map(|r| r.agents.len()).sum::<usize>() > state.beta.manual_limit
        {
            let mut missing = 0;
            for row in &rows {
                for agent in &row.agents {
                    if cached_report(state, row, agent, &model).await?.is_none() {
                        missing += 1;
                        if missing > state.beta.manual_limit {
                            return state
                                .templates
                                .render("beta_limit", &json!({"limit":state.beta.manual_limit}))
                                .map(Outcome::Notice);
                        }
                    }
                }
            }
        }
        for row in &rows {
            send_reports(&bot, state, row, &model, chat, &settings, Some(status)).await?;
        }
        Ok(Outcome::Delivered)
    }
}

pub async fn poll(bot: &Bot, state: &BotState, settings: &SchedulerSettings) -> Result<()> {
    if !state.db.has_diff_subscribers().await? || rustverse_ai::api_key().is_err() {
        return Ok(());
    }
    let model = state.ai_model().await?.cache_key();
    for branch in [Branch::Beta, Branch::Release] {
        if let Err(err) = poll_branch(bot, state, settings, branch, &model).await {
            log::warn!("Cannot poll {} changelog: {err:#}", branch.name());
        }
    }
    Ok(())
}

async fn poll_branch(
    bot: &Bot,
    state: &BotState,
    settings: &SchedulerSettings,
    branch: Branch,
    model: &str,
) -> Result<()> {
    let latest = branch_revisions(&state.beta.client, branch, None, None)
        .await?
        .pop()
        .context("empty changelog")?;
    let mut groups = BTreeMap::<String, Vec<i64>>::new();
    for (chat, cursor) in state
        .db
        .revision_cursors(branch.name(), &latest.current)
        .await?
    {
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
        for mut row in rows {
            if active.is_empty() {
                break;
            }
            if let Err(error) = row.group_forms(&state.beta.client).await {
                log::warn!("Cannot resolve agent forms for {}: {error:#}", row.current);
                break;
            }
            deliver_revision(bot, state, branch, &row, &mut active, settings, model).await;
        }
    }
    Ok(())
}

async fn deliver_revision(
    bot: &Bot,
    state: &BotState,
    branch: Branch,
    row: &Revision,
    active: &mut Vec<i64>,
    settings: &SchedulerSettings,
    model: &str,
) {
    let mut successful = Vec::new();
    for &chat in active.iter() {
        let delivery = async {
            if !state
                .db
                .revision_pending(chat, branch.name(), &row.previous)
                .await?
            {
                return Ok(false);
            }
            send_reports(bot, state, row, model, ChatId(chat), settings, None).await?;
            state
                .db
                .advance_revision(chat, branch.name(), &row.previous, &row.current)
                .await?;
            anyhow::Ok(true)
        }
        .await;
        match delivery {
            Ok(true) => successful.push(chat),
            Ok(false) => {}
            Err(error) => log::warn!("Beta revision {} for chat {chat}: {error:#}", row.current),
        }
    }
    *active = successful;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn independent_reports_overlap_and_results_keep_revision_order() {
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let mut releases = Vec::new();
        let mut jobs = Vec::new();
        for id in 0..=PARALLEL_REPORTS {
            let (release, gate) = oneshot::channel();
            releases.push(Some(release));
            let started = started.clone();
            jobs.push(async move {
                started.send(id).unwrap();
                gate.await.unwrap();
                id
            });
        }
        let (prepare, mut reports) = prepare_reports(jobs);
        let mut prepare = std::pin::pin!(prepare);
        assert!(futures_util::poll!(&mut prepare).is_pending());
        for id in 0..PARALLEL_REPORTS {
            assert_eq!(starts.try_recv().unwrap(), id);
        }
        assert!(starts.try_recv().is_err());
        // Finish the second character while the first is still researching.
        releases[1].take().unwrap().send(()).unwrap();
        assert!(futures_util::poll!(&mut prepare).is_pending());
        assert_eq!(starts.try_recv().unwrap(), PARALLEL_REPORTS);
        assert!(matches!(
            reports[0].try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        for release in releases.into_iter().flatten() {
            release.send(()).unwrap();
        }
        prepare.await.unwrap();
        for (id, report) in reports.into_iter().enumerate() {
            assert_eq!(report.await.unwrap(), id);
        }
    }

    #[tokio::test]
    async fn only_reports_for_the_same_character_wait_for_each_other() {
        let pages = BetaPages::new().unwrap();
        let first = pages.lock_agent(1).await;
        let mut duplicate = std::pin::pin!(pages.lock_agent(1));
        assert!(futures_util::poll!(&mut duplicate).is_pending());
        assert!(futures_util::poll!(std::pin::pin!(pages.lock_agent(2))).is_ready());
        drop(first);
        assert!(futures_util::poll!(&mut duplicate).is_ready());
    }

    #[test]
    fn text_report_keeps_the_release_source_link() {
        let template = crate::templates::TemplateEngine::new().unwrap();
        let url = "https://zzz.gachabase.net/diff/agents/1/a?prev=x&cur=y";
        let source = template
            .render("beta_source", &json!({"release":true, "url":url}))
            .unwrap();
        let parts =
            telegram_html::chunks("", &format!("# A & B\n\n{}", source.text), MESSAGE_LIMIT);
        let html = scraper::Html::parse_fragment(&parts[0]);
        let link = html
            .select(&scraper::Selector::parse("a").unwrap())
            .next()
            .unwrap();
        assert_eq!(link.value().attr("href"), Some(url));
        assert_eq!(link.text().collect::<String>(), "Релиз патча · GachaBase");
    }
}
