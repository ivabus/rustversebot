use crate::{
    BotState,
    agent::{self, AgentRun, ChatAgent, MessageAddress, Outcome, Status},
    bot_templates::{BotTemplateSender, MESSAGE_LIMIT},
    scheduler::SchedulerSettings,
    telegram_html,
};
use anyhow::{Context, Result};
use rustverse_ai::PromptProgress;
use rustverse_ai::beta::{AgentChange, Branch, Revision, branch_revisions, revisions};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use teloxide::prelude::*;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, watch};

const PARALLEL_REPORTS: usize = 3;
// Real GachaBase agent IDs are positive. Zero identifies revision-wide cache and delivery rows.
const WHOLE_REVISION: u64 = 0;

type RevisionLocks = HashMap<(String, String), Weak<AsyncMutex<()>>>;

pub(crate) struct BetaPages {
    pub(crate) client: reqwest::Client,
    slots: Semaphore,
    revisions: Mutex<RevisionLocks>,
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
            revisions: Default::default(),
            manual_limit: match std::env::var("BETA_MAX_NEW_REPORTS") {
                Ok(value) => value.parse().context("invalid BETA_MAX_NEW_REPORTS")?,
                Err(std::env::VarError::NotPresent) => 10,
                Err(error) => return Err(error.into()),
            },
        })
    }

    async fn lock_revision(
        &self,
        previous: &str,
        current: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut revisions = self.revisions.lock().unwrap();
            revisions.retain(|_, lock| lock.strong_count() > 0);
            let entry = revisions
                .entry((previous.into(), current.into()))
                .or_default();
            entry.upgrade().unwrap_or_else(|| {
                let lock = Arc::new(AsyncMutex::new(()));
                *entry = Arc::downgrade(&lock);
                lock
            })
        };
        lock.lock_owned().await
    }
}

async fn cached_report(
    state: &BotState,
    rev: &Revision,
    model: &str,
    version: &str,
) -> Result<Option<String>> {
    state
        .db
        .beta_report(&rev.previous, &rev.current, WHOLE_REVISION, model, version)
        .await
}

fn report_version(row: &Revision) -> String {
    use std::fmt::Write;
    let mut version = rustverse_ai::prompt_version().to_owned();
    for agent in &row.agents {
        write!(version, "/{}", agent.id).unwrap();
        for member in agent.changed_members() {
            write!(
                version,
                ":{}-{}-{}",
                member.id, member.status, member.assets_changed
            )
            .unwrap();
        }
    }
    version
}

async fn report(
    state: &BotState,
    rev: &Revision,
    model: &str,
    progress: Option<watch::Sender<PromptProgress>>,
) -> Result<String> {
    let version = report_version(rev);
    let _guard = state.beta.lock_revision(&rev.previous, &rev.current).await;
    if let Some(text) = cached_report(state, rev, model, &version).await? {
        return Ok(text);
    }
    let source = if progress.is_some() {
        "telegram"
    } else {
        "scheduler"
    };
    let _slot = state.beta.slots.acquire().await?;
    let key = rustverse_ai::api_key()?;
    let span = rustverse_ai::telemetry::diff_span(
        source,
        model,
        &rev.previous,
        &rev.current,
        rev.agents.len(),
    );
    let text = rustverse_ai::telemetry::observe(
        span,
        rustverse_ai::beta::revision_report(&state.beta.client, rev, &key, model, progress),
    )
    .await?;
    state
        .db
        .save_beta_report(
            &rev.previous,
            &rev.current,
            WHOLE_REVISION,
            model,
            &version,
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

fn diff_status(previous: Option<&str>, current: Option<&str>, stage: DiffStage) -> Status<'static> {
    Status::Activity {
        template: "diff_status",
        context: json!({ "previous":previous, "current":current, "stage":stage }),
    }
}

#[tracing::instrument(
    target = "rustverse::bot",
    name = "revision.deliver_part",
    skip_all,
    err
)]
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

async fn deliver_parts(
    sender: &BotTemplateSender<'_>,
    destination: (ChatId, Option<teloxide::types::ThreadId>),
    settings: &SchedulerSettings,
    parts: &[String],
    checkpoint: Option<(&crate::db::Db, &str, u64, &[u64])>,
) -> Result<()> {
    let (chat, thread) = destination;
    for (i, part) in parts.iter().enumerate() {
        send_report_part(sender, chat, thread, part, settings).await?;
        if let Some((db, revision, key, ids)) = checkpoint {
            let remaining = &parts[i + 1..];
            if remaining.is_empty() {
                db.mark_beta_delivered(chat.0, revision, ids).await?;
            } else {
                db.save_pending_beta_text(chat.0, revision, key, remaining)
                    .await?;
            }
        }
    }
    Ok(())
}

// A chat can be partway through the previous per-character delivery format at upgrade.
fn outstanding_revision(row: &Revision, confirmed: &HashSet<u64>) -> Revision {
    let mut remaining = row.clone();
    for agent in &mut remaining.agents {
        if confirmed.contains(&agent.id) {
            agent.status = "unchanged".into();
        }
        agent.forms.retain(|form| !confirmed.contains(&form.id));
    }
    remaining
        .agents
        .retain(|agent| agent.changed_members().next().is_some());
    remaining
}

#[tracing::instrument(target = "rustverse::bot", name = "revision.deliver", skip_all, err)]
async fn send_reports(
    bot: &Bot,
    state: &BotState,
    row: &Revision,
    model: &str,
    chat: ChatId,
    settings: &SchedulerSettings,
    mut status: Option<&mut AgentRun<'_>>,
) -> Result<()> {
    if row.agents.is_empty() {
        return Ok(());
    }
    let automatic = status.is_none();
    let destination = (chat, status.as_ref().and_then(|s| s.address().thread));
    let sender = BotTemplateSender::new(bot, &state.templates);
    let outstanding;
    let mut pending = None;
    let row = if automatic {
        let mut confirmed = state.db.delivered_beta_agents(chat.0, &row.current).await?;
        if confirmed.contains(&WHOLE_REVISION) {
            return Ok(());
        }
        let mut tails = state.db.pending_beta_parts(chat.0, &row.current).await?;
        pending = tails.remove(&WHOLE_REVISION);
        for agent in &row.agents {
            let ids: Vec<_> = agent
                .changed_members()
                .map(|m| m.id)
                .filter(|id| !confirmed.contains(id))
                .collect();
            if let Some(&key) = ids.first()
                && let Some(parts) = tails.remove(&key)
            {
                deliver_parts(
                    &sender,
                    destination,
                    settings,
                    &parts,
                    Some((&state.db, &row.current, key, &ids)),
                )
                .await?;
                confirmed.extend(ids);
            }
        }
        outstanding = outstanding_revision(row, &confirmed);
        &outstanding
    } else {
        row
    };
    if row.agents.is_empty() {
        return Ok(());
    }

    let parts = if let Some(parts) = pending {
        parts
    } else {
        let (tx, progress) = watch::channel(PromptProgress::default());
        let generate = report(state, row, model, (!automatic).then_some(tx));
        let text = match status.as_deref_mut() {
            Some(run) => {
                run.track_progress(
                    diff_status(Some(&row.previous), Some(&row.current), DiffStage::Report),
                    progress,
                    generate,
                )
                .await?
            }
            None => generate.await?,
        };
        let sources = row
            .agents
            .iter()
            .flat_map(AgentChange::changed_members)
            .map(|member| {
                let url = rustverse_ai::beta::diff_url(row, member)?;
                let label = member
                    .name
                    .replace('\\', "\\\\")
                    .replace('[', "\\[")
                    .replace(']', "\\]");
                Ok(json!({"name":label, "url":url.as_str()}))
            })
            .collect::<Result<Vec<_>>>()?;
        let source = state.templates.render("beta_source", &json!({
            "comparison": !automatic,
            "hotfix": row.previous.rsplit_once('_').map(|p| p.0) == row.current.rsplit_once('_').map(|p| p.0),
            "release": row.current.starts_with("release_"), "sources": sources,
        }))?;
        telegram_html::chunks("", &format!("{text}\n\n{}", source.text), MESSAGE_LIMIT)
    };
    if let Some(run) = status {
        run.show(&diff_status(
            Some(&row.previous),
            Some(&row.current),
            DiffStage::Send,
        ))
        .await?;
    }
    let ids: Vec<_> = std::iter::once(WHOLE_REVISION)
        .chain(
            row.agents
                .iter()
                .flat_map(AgentChange::changed_members)
                .map(|m| m.id),
        )
        .collect();
    deliver_parts(
        &sender,
        destination,
        settings,
        &parts,
        automatic.then_some((&state.db, &row.current, WHOLE_REVISION, &ids)),
    )
    .await
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
        )
    }

    async fn execute(&self, status: &mut AgentRun<'_>, state: &BotState) -> Result<Outcome> {
        let model = status
            .model()
            .parse::<rustverse_ai::ModelSelection>()?
            .cache_key();
        let chat = status.address().chat;
        let bot = status.bot().clone();
        let mut rows = if let (true, Some(prev), Some(cur)) = (
            self.compare,
            self.previous.as_deref(),
            self.current.as_deref(),
        ) {
            vec![rustverse_ai::beta::compare_revisions(&state.beta.client, prev, cur).await?]
        } else {
            revisions(
                &state.beta.client,
                self.previous.as_deref(),
                self.current.as_deref(),
            )
            .await?
        };
        let prev = rows.first().context("empty changelog")?.previous.clone();
        let cur = rows.last().unwrap().current.clone();
        status
            .show(&diff_status(Some(&prev), Some(&cur), DiffStage::Prepare))
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
            && rows.iter().filter(|r| !r.agents.is_empty()).count() > state.beta.manual_limit
        {
            let mut missing = 0;
            for row in rows.iter().filter(|r| !r.agents.is_empty()) {
                if cached_report(state, row, &model, &report_version(row))
                    .await?
                    .is_none()
                {
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
        for row in &rows {
            send_reports(&bot, state, row, &model, chat, &settings, Some(status)).await?;
        }
        Ok(Outcome::Delivered)
    }
}

#[tracing::instrument(target = "rustverse::bot", name = "scheduler.revisions", skip_all, err)]
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

#[tracing::instrument(target = "rustverse::bot", name = "gachabase.poll_branch", skip_all, err, fields(branch = branch.name()))]
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
    async fn matching_revision_pairs_share_a_generation_lock() {
        let pages = BetaPages::new().unwrap();
        let first = pages.lock_revision("a", "c").await;
        let mut duplicate = std::pin::pin!(pages.lock_revision("a", "c"));
        assert!(futures_util::poll!(&mut duplicate).is_pending());
        assert!(futures_util::poll!(std::pin::pin!(pages.lock_revision("b", "c"))).is_ready());
        drop(first);
        assert!(futures_util::poll!(&mut duplicate).is_ready());
    }

    #[test]
    fn resumed_digest_skips_confirmed_members_and_keeps_form_ownership() {
        let agent = |id| AgentChange {
            id,
            name: format!("Agent {id}"),
            slug: id.to_string(),
            status: "updated".into(),
            assets_changed: false,
            forms: Vec::new(),
        };
        let mut owner = agent(1601);
        owner.forms.push(agent(1603));
        let row = Revision {
            previous: "before".into(),
            current: "after".into(),
            agents: vec![agent(1331), owner, agent(1651)],
        };
        let remaining = outstanding_revision(&row, &HashSet::from([1331, 1601]));
        assert_eq!(remaining.agents[0].id, 1601);
        assert_eq!(
            remaining
                .agents
                .iter()
                .flat_map(AgentChange::changed_members)
                .map(|m| m.id)
                .collect::<Vec<_>>(),
            [1603, 1651]
        );
        assert_ne!(report_version(&row), report_version(&remaining));
    }

    #[test]
    fn text_report_keeps_the_release_source_link() {
        let template = crate::templates::TemplateEngine::new().unwrap();
        let url = "https://zzz.gachabase.net/diff/agents/1/a?prev=x&cur=y";
        let source = template
            .render(
                "beta_source",
                &json!({"release":true, "sources":[{"name":"A & B","url":url}]}),
            )
            .unwrap();
        let parts =
            telegram_html::chunks("", &format!("# A & B\n\n{}", source.text), MESSAGE_LIMIT);
        let html = scraper::Html::parse_fragment(&parts[0]);
        let link = html
            .select(&scraper::Selector::parse("a").unwrap())
            .next()
            .unwrap();
        assert_eq!(link.value().attr("href"), Some(url));
        assert_eq!(link.text().collect::<String>(), "A & B");
    }
}
