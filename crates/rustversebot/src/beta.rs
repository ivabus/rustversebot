use crate::{
    BotState,
    agent::{self, AgentRun, ChatAgent, MessageAddress, Outcome, Status},
    bot_templates::BotTemplateSender,
    scheduler::SchedulerSettings,
};
use anyhow::{Context, Result};
use rustverse_ai::beta::{AgentChange, Branch, Revision, branch_revisions, revisions};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use teloxide::prelude::*;

mod album;
pub(crate) use album::send_infographic;
use album::{Page, album_groups, send_album};

pub(crate) struct BetaPages {
    pub(crate) client: reqwest::Client,
    generation: tokio::sync::Mutex<()>,
    manual_limit: usize,
}

impl BetaPages {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(180))
                .build()?,
            generation: Default::default(),
            manual_limit: match std::env::var("BETA_MAX_NEW_REPORTS") {
                Ok(value) => value.parse().context("invalid BETA_MAX_NEW_REPORTS")?,
                Err(std::env::VarError::NotPresent) => 10,
                Err(error) => return Err(error.into()),
            },
        })
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
    status: Option<&mut AgentRun<'_>>,
) -> Result<String> {
    if let Some(text) = cached_report(state, rev, agent, model).await? {
        return Ok(text);
    }
    let generate = |progress| async move {
        let _guard = state.beta.generation.lock().await;
        if let Some(text) = cached_report(state, rev, agent, model).await? {
            return Ok(text);
        }
        let key = rustverse_ai::api_key()?;
        let text = rustverse_ai::beta::report_with_progress(
            &state.beta.client,
            rev,
            agent,
            &key,
            model,
            progress,
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
    };
    if let Some(status) = status {
        status
            .track(
                diff_status(
                    Some(&rev.previous),
                    Some(&rev.current),
                    DiffStage::Report,
                    Some(&agent.name),
                ),
                |tx| generate(Some(tx)),
            )
            .await
    } else {
        generate(None).await
    }
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum DiffStage {
    Prepare,
    Report,
    Render,
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

/// Render each requested character once, sharing its PNG between chat deliveries.
struct RevisionAlbums<'a> {
    revision: &'a Revision,
    state: &'a BotState,
    model: &'a str,
    rendered: BTreeMap<u64, Page>,
}

impl<'a> RevisionAlbums<'a> {
    fn new(state: &'a BotState, revision: &'a Revision, model: &'a str) -> Self {
        Self {
            state,
            revision,
            model,
            rendered: BTreeMap::new(),
        }
    }

    async fn send(
        &mut self,
        bot: &Bot,
        chat: ChatId,
        settings: &SchedulerSettings,
        mut status: Option<&mut AgentRun<'_>>,
    ) -> Result<()> {
        let automatic = status.is_none();
        let row = self.revision;
        let confirmed = if automatic {
            self.state
                .db
                .delivered_beta_agents(chat.0, &row.current)
                .await?
        } else {
            HashSet::new()
        };
        let groups = album_groups(&row.agents);
        for (index, agents) in groups.iter().enumerate() {
            let mut pages = Vec::new();
            let mut links = Vec::new();
            let mut ids = Vec::new();
            for agent in *agents {
                if agent
                    .changed_members()
                    .all(|member| confirmed.contains(&member.id))
                {
                    continue;
                }
                ids.extend(agent.changed_members().map(|member| member.id));
                let linked_member = agent
                    .changed_members()
                    .next()
                    .context("empty character report")?;
                links.push(
                    json!({"url":rustverse_ai::beta::diff_url(row,linked_member)?.as_str(),
                    "name":agent.name.chars().take(36).collect::<String>()}),
                );
                let page = match self.rendered.entry(agent.id) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let text =
                            report(self.state, row, agent, self.model, status.as_deref_mut())
                                .await?;
                        if let Some(status) = status.as_deref_mut() {
                            status
                                .show(&diff_status(
                                    Some(&row.previous),
                                    Some(&row.current),
                                    DiffStage::Render,
                                    Some(&agent.name),
                                ))
                                .await?;
                        }
                        entry.insert(Page::new(
                            agent.id,
                            rustverse_images::beta_page(&text).await?,
                        )?)
                    }
                };
                pages.push(page.clone());
            }
            if pages.is_empty() {
                continue;
            }
            let caption = self.state.templates.render("beta_album", &json!({
                "previous":row.previous.chars().take(48).collect::<String>(), "current":row.current.chars().take(48).collect::<String>(),
                "hotfix":row.previous.rsplit_once('_').map(|p|p.0)==row.current.rsplit_once('_').map(|p|p.0),
                "release":row.current.starts_with("release_"), "comparison":!automatic,
                "part":index+1, "parts":groups.len(), "links":links
            }))?;
            if let Some(status) = status.as_deref_mut() {
                status
                    .show(&diff_status(
                        Some(&row.previous),
                        Some(&row.current),
                        DiffStage::Send,
                        None,
                    ))
                    .await?;
            }
            let thread = status.as_ref().and_then(|s| s.address().thread);
            send_album(bot, chat, thread, &pages, &caption.text, settings).await?;
            if automatic {
                self.state
                    .db
                    .mark_beta_delivered(chat.0, &row.current, &ids)
                    .await?;
            }
        }
        Ok(())
    }
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
        let model = status.model().cache_key();
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
        // Preflight the whole range so an over-budget command sends no partial albums.
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
            RevisionAlbums::new(state, row, &model)
                .send(&bot, chat, &settings, Some(status))
                .await?;
        }
        Ok(Outcome::MediaDelivered)
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
    let mut albums = RevisionAlbums::new(state, row, model);
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
            albums.send(bot, ChatId(chat), settings, None).await?;
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

    #[test]
    fn release_caption_escapes_names_and_links_only_sent_entries() {
        let template = crate::templates::TemplateEngine::new().unwrap();
        let caption=template.render("beta_album",&json!({"release":true,"hotfix":false,"comparison":false,
            "previous":"release_3.2.0_1","current":"release_3.3.0_2","part":1,"parts":1,
            "links":[{"name":"A & B","url":"https://zzz.gachabase.net/diff/agents/1/a?prev=x&cur=y"}]})).unwrap();
        assert_eq!(caption.parse_mode(), Some(teloxide::types::ParseMode::Html));
        assert!(caption.text.contains("Релиз патча") && caption.text.contains(">A &amp; B</a>"));
        assert_eq!(
            scraper::Html::parse_fragment(&caption.text)
                .select(&scraper::Selector::parse("a").unwrap())
                .next()
                .unwrap()
                .value()
                .attr("href"),
            Some("https://zzz.gachabase.net/diff/agents/1/a?prev=x&cur=y")
        );
    }
}
