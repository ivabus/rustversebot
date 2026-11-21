use crate::scheduler::SchedulerSettings;
use crate::{BotState, bot_templates::BotTemplateSender};
use crate::{
    prompt::{self, LiveMessage, MessageAddress},
    templates::RenderedTemplate,
};
use anyhow::{Context, Result};
use rustverse_ai::PromptProgress;
use rustverse_ai::beta::{AgentChange, Branch, Revision, branch_revisions, revisions};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use teloxide::{
    prelude::*,
    types::{InputFile, InputMedia, InputMediaDocument, InputMediaPhoto, ParseMode, ThreadId},
};
use tokio::{sync::watch, time::Instant};

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
    status: Option<&mut DiffStatus<'_>>,
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
        let last = status.update("report", Some(&agent.name)).await?;
        let (tx, rx) = watch::channel(PromptProgress::default());
        let (text, ()) = tokio::join!(
            generate(Some(tx)),
            status.updates(&agent.name, rx.clone(), last)
        );
        add_usage(&mut status.totals, &rx.borrow());
        text
    } else {
        generate(None).await
    }
}

struct DiffStatus<'a> {
    output: prompt::LiveOutput<'a>,
    target: LiveMessage,
    model: &'a str,
    totals: PromptProgress,
    retry_at: Option<Instant>,
}

fn add_usage(progress: &mut PromptProgress, other: &PromptProgress) {
    progress.tools += other.tools;
    progress.tokens = progress
        .tokens
        .zip(other.tokens)
        .and_then(|(a, b)| a.checked_add(b));
    progress.cost_usd = match (progress.turns, other.turns) {
        (_, 0) => progress.cost_usd,
        (0, _) => other.cost_usd,
        _ => progress.cost_usd.zip(other.cost_usd).map(|(a, b)| a + b),
    };
    progress.turns += other.turns;
}

impl DiffStatus<'_> {
    fn footer(&self, stage: &str, agent: Option<&str>) -> Result<RenderedTemplate> {
        prompt::status_footer(
            self.output.templates,
            self.model,
            &self.totals,
            Some(stage),
            agent,
        )
    }

    async fn update(&mut self, stage: &str, agent: Option<&str>) -> Result<String> {
        let rendered = self.footer(stage, agent)?;
        self.output
            .show_stage(self.target, &rendered.text, &mut self.retry_at)
            .await;
        Ok(rendered.text)
    }

    async fn updates(&mut self, agent: &str, rx: watch::Receiver<PromptProgress>, last: String) {
        self.output
            .updates(self.target, rx, last, &mut self.retry_at, |p| {
                let mut progress = p.clone();
                add_usage(&mut progress, &self.totals);
                Ok(prompt::status_footer(
                    self.output.templates,
                    self.model,
                    &progress,
                    Some("report"),
                    Some(agent),
                )?
                .text)
            })
            .await;
    }

    async fn finish(&self, rendered: RenderedTemplate) -> Result<()> {
        if let Some(at) = self.retry_at {
            tokio::time::sleep_until(at).await;
        }
        let edit = match self.target {
            LiveMessage::Group(id) => Some(id),
            LiveMessage::Draft => None,
        };
        self.output.send(rendered, edit).await?;
        Ok(())
    }
}

const ALBUM_LIMIT: usize = 10;

fn album_groups<T>(mut items: &[T]) -> Vec<&[T]> {
    let mut groups = Vec::new();
    while !items.is_empty() {
        // Keep the last two pages together when ten would leave a singleton.
        let count = if items.len() == ALBUM_LIMIT + 1 {
            ALBUM_LIMIT - 1
        } else {
            items.len().min(ALBUM_LIMIT)
        };
        let (group, rest) = items.split_at(count);
        groups.push(group);
        items = rest;
    }
    groups
}

#[derive(Clone)]
struct Page {
    file: InputFile,
    photo: bool,
}

impl Page {
    fn new(id: u64, png: Vec<u8>) -> Result<Self> {
        anyhow::ensure!(png.len() >= 24, "incomplete PNG");
        let width = u32::from_be_bytes(png[16..20].try_into()?);
        let height = u32::from_be_bytes(png[20..24].try_into()?);
        let photo = width.saturating_add(height) <= 10000
            && width.max(height) <= width.min(height).saturating_mul(20)
            && png.len() <= 10_000_000;
        Ok(Self {
            photo,
            file: InputFile::memory(png).file_name(format!("beta-{id}.png")),
        })
    }
}

fn album_media(pages: &[Page], caption: &str) -> Vec<InputMedia> {
    let photos = pages.iter().all(|page| page.photo);
    pages
        .iter()
        .enumerate()
        .map(|(index, page)| {
            // Telegram requires a document album to contain documents exclusively.
            let caption = (index == 0).then(|| caption.to_owned());
            if photos {
                let mut media = InputMediaPhoto::new(page.file.clone());
                media.parse_mode = caption.as_ref().map(|_| ParseMode::Html);
                media.caption = caption;
                InputMedia::Photo(media)
            } else {
                let mut media = InputMediaDocument::new(page.file.clone());
                media.parse_mode = caption.as_ref().map(|_| ParseMode::Html);
                media.caption = caption;
                InputMedia::Document(media)
            }
        })
        .collect()
}

async fn send_album(
    bot: &Bot,
    chat: ChatId,
    thread: Option<ThreadId>,
    pages: &[Page],
    caption: &str,
    settings: &SchedulerSettings,
) -> Result<()> {
    anyhow::ensure!(
        (1..=ALBUM_LIMIT).contains(&pages.len()),
        "invalid album size"
    );
    let mut attempt = 1;
    loop {
        tokio::time::sleep(settings.request_spacing).await;
        let result = match pages {
            [page] if page.photo => {
                let mut request = bot
                    .send_photo(chat, page.file.clone())
                    .parse_mode(ParseMode::Html)
                    .caption(caption);
                if let Some(thread) = thread {
                    request = request.message_thread_id(thread);
                }
                request.await.map(|_| ())
            }
            [page] => {
                let mut request = bot
                    .send_document(chat, page.file.clone())
                    .parse_mode(ParseMode::Html)
                    .caption(caption);
                if let Some(thread) = thread {
                    request = request.message_thread_id(thread);
                }
                request.await.map(|_| ())
            }
            _ => {
                let mut request = bot.send_media_group(chat, album_media(pages, caption));
                if let Some(thread) = thread {
                    request = request.message_thread_id(thread);
                }
                request.await.map(|_| ())
            }
        };
        match result {
            Ok(()) => return Ok(()),
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
        mut status: Option<&mut DiffStatus<'_>>,
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
                            status.update("render", Some(&agent.name)).await?;
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
                status.update("send", None).await?;
            }
            let thread = status.as_ref().and_then(|s| s.output.address.thread);
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
    let Some(slot) = crate::agent_access::admit(
        bot,
        chat,
        address.thread,
        user,
        if compare { "diff" } else { "betadiff" },
        state,
    )
    .await?
    else {
        return Ok(());
    };
    let selection = state.ai_model().await?;
    let model = selection.cache_key();
    let display_model = selection.to_string();
    let initial = prompt::status_footer(
        &state.templates,
        &display_model,
        &PromptProgress::default(),
        Some("prepare"),
        None,
    )?;
    let output = prompt::LiveOutput {
        bot,
        templates: &state.templates,
        address,
    };
    let target = output.start(initial).await?;
    let admin = crate::agent_access::is_admin(user, state);
    let (prev, cur) = (prev.map(str::to_owned), cur.map(str::to_owned));
    let state = state.clone();
    let bot = bot.clone();
    slot.send(Box::pin(async move {
        let mut status = DiffStatus {
            output: prompt::LiveOutput {
                bot: &bot,
                templates: &state.templates,
                address,
            },
            target,
            model: &display_model,
            totals: PromptProgress::default(),
            retry_at: None,
        };
        let run = async {
            let mut rows = revisions(&state.beta.client, prev.as_deref(), cur.as_deref()).await?;
            if compare && rows.len() > 1 {
                rows = vec![rustverse_ai::beta::comparison_range(&rows)?];
            }
            for row in &mut rows {
                row.group_forms(&state.beta.client).await?;
            }
            let prev = &rows.first().context("empty changelog")?.previous;
            let cur = &rows.last().unwrap().current;
            let settings = SchedulerSettings::from_env();
            if rows.iter().all(|row| row.agents.is_empty()) {
                return state
                    .templates
                    .render("beta_empty", &json!({"previous":prev,"current":cur}));
            }
            // Preflight the whole range so an over-budget command sends no partial albums.
            if !admin
                && rows.iter().map(|r| r.agents.len()).sum::<usize>() > state.beta.manual_limit
            {
                let mut missing = 0;
                for row in &rows {
                    for agent in &row.agents {
                        if cached_report(&state, row, agent, &model).await?.is_none() {
                            missing += 1;
                            if missing > state.beta.manual_limit {
                                return state.templates.render(
                                    "beta_limit",
                                    &json!({"limit":state.beta.manual_limit}),
                                );
                            }
                        }
                    }
                }
            }
            for row in &rows {
                RevisionAlbums::new(&state, row, &model)
                    .send(&bot, chat, &settings, Some(&mut status))
                    .await?;
            }
            status.footer("complete", None)
        }
        .await;
        let final_message = match run {
            Ok(rendered) => rendered,
            Err(error) => state
                .templates
                .render("error_generic", &json!({"error":error.to_string()}))?,
        };
        status.finish(final_message).await
    }));
    Ok(())
}

pub async fn poll(bot: &Bot, state: &BotState, settings: &SchedulerSettings) -> Result<()> {
    if state.db.get_distinct_chats().await?.is_empty() || rustverse_ai::api_key().is_err() {
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
            albums.send(bot, ChatId(chat), settings, None).await?;
            state
                .db
                .advance_revision(chat, branch.name(), &row.previous, &row.current)
                .await
        }
        .await;
        match delivery {
            Ok(()) => successful.push(chat),
            Err(error) => log::warn!("Beta revision {} for chat {chat}: {error:#}", row.current),
        }
    }
    *active = successful;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_status_keeps_total_usage_across_agents_and_cache_hits() {
        let templates = crate::templates::TemplateEngine::new().unwrap();
        let mut totals = PromptProgress::default();
        let usage = PromptProgress {
            turns: 2,
            tools: 3,
            tokens: Some(150),
            cost_usd: Some(0.025),
            ..Default::default()
        };
        add_usage(&mut totals, &usage);
        add_usage(&mut totals, &usage);
        add_usage(&mut totals, &PromptProgress::default());
        let rendered =
            prompt::status_footer(&templates, "model", &totals, Some("render"), Some("A&B"))
                .unwrap();
        assert!(
            rendered
                .text
                .starts_with("<b>A&amp;B</b>\n<i>Рендеринг карточки…</i>")
        );
        assert!(
            rendered
                .text
                .contains("Вызовы инструментов: 6 · Ходы: 4 · Токены: 300 · Стоимость: ≈ $0,05")
        );
        add_usage(
            &mut totals,
            &PromptProgress {
                turns: 1,
                tokens: None,
                cost_usd: None,
                ..Default::default()
            },
        );
        let done =
            prompt::status_footer(&templates, "model", &totals, Some("complete"), None).unwrap();
        assert!(done.text.contains("Сравнение готово"));
        assert!(done.text.contains("Токены: — · Стоимость: —"));
    }

    #[test]
    fn release_caption_escapes_names_and_links_only_sent_entries() {
        let template = crate::templates::TemplateEngine::new().unwrap();
        let caption=template.render("beta_album",&json!({"release":true,"hotfix":false,"comparison":false,
            "previous":"release_3.2.0_1","current":"release_3.3.0_2","part":1,"parts":1,
            "links":[{"name":"A & B","url":"https://zzz.gachabase.net/diff/agents/1/a?prev=x&cur=y"}]})).unwrap();
        assert_eq!(caption.parse_mode(), Some(ParseMode::Html));
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

    #[test]
    fn album_sizes_preserve_every_page_in_order() {
        for (count, sizes) in [
            (0, vec![]),
            (1, vec![1]),
            (2, vec![2]),
            (10, vec![10]),
            (11, vec![9, 2]),
            (12, vec![10, 2]),
            (20, vec![10, 10]),
            (21, vec![10, 9, 2]),
        ] {
            let agents: Vec<_> = (0..count).collect();
            let albums = album_groups(&agents);
            assert_eq!(albums.iter().map(|g| g.len()).collect::<Vec<_>>(), sizes);
            assert_eq!(albums.concat(), agents);
        }
    }

    #[test]
    fn album_uses_one_media_type_and_only_one_caption() {
        let page = || Page {
            file: InputFile::memory(vec![1]),
            photo: true,
        };
        let mut pages = [page(), page()];
        for photos in [true, false] {
            pages[1].photo = photos;
            let media = album_media(&pages, "r1 → r2");
            assert_eq!(media.len(), 2);
            for (i, item) in media.iter().enumerate() {
                let caption = match (photos, item) {
                    (true, InputMedia::Photo(m)) => &m.caption,
                    (false, InputMedia::Document(m)) => &m.caption,
                    _ => panic!("unexpected media type"),
                };
                assert_eq!(caption.as_deref(), (i == 0).then_some("r1 → r2"));
            }
        }
    }
}
