use crate::scheduler::SchedulerSettings;
use crate::{BotState, bot_templates::BotTemplateSender};
use anyhow::{Context, Result};
use rustverse_ai::beta::{AgentChange, Branch, Revision, branch_revisions, revisions};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use teloxide::{
    prelude::*,
    types::{InputFile, InputMedia, InputMediaDocument, InputMediaPhoto, ParseMode},
};

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
            rustverse_ai::prompt_version(),
        )
        .await
}

async fn report(
    state: &BotState,
    rev: &Revision,
    agent: &AgentChange,
    model: &str,
) -> Result<String> {
    if let Some(text) = cached_report(state, rev, agent, model).await? {
        return Ok(text);
    }
    let _guard = state.beta.generation.lock().await;
    if let Some(text) = cached_report(state, rev, agent, model).await? {
        return Ok(text);
    }
    let key = rustverse_ai::api_key()?;
    let text =
        rustverse_ai::beta::agent_report(&state.beta.client, rev, agent, &key, model).await?;
    state
        .db
        .save_beta_report(
            &rev.previous,
            &rev.current,
            agent.id,
            model,
            rustverse_ai::prompt_version(),
            &text,
        )
        .await?;
    Ok(text)
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
    id: u64,
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
            id,
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
            [page] if page.photo => bot
                .send_photo(chat, page.file.clone())
                .parse_mode(ParseMode::Html)
                .caption(caption)
                .await
                .map(|_| ()),
            [page] => bot
                .send_document(chat, page.file.clone())
                .parse_mode(ParseMode::Html)
                .caption(caption)
                .await
                .map(|_| ()),
            _ => bot
                .send_media_group(chat, album_media(pages, caption))
                .await
                .map(|_| ()),
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
        automatic: bool,
    ) -> Result<()> {
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
            for agent in agents.iter().filter(|a| !confirmed.contains(&a.id)) {
                links.push(
                    json!({"url":rustverse_ai::beta::diff_url(row,agent)?.as_str(),
                    "name":agent.name.chars().take(36).collect::<String>()}),
                );
                let page = match self.rendered.entry(agent.id) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let text = report(self.state, row, agent, self.model).await?;
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
            send_album(bot, chat, &pages, &caption.text, settings).await?;
            if automatic {
                let ids: Vec<_> = pages.iter().map(|page| page.id).collect();
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
    chat: ChatId,
    user: Option<&teloxide::types::User>,
    args: &str,
    state: &BotState,
) -> Result<()> {
    if args.trim().is_empty() {
        return picker(bot, chat, user, "menu", state).await;
    }
    command(bot, chat, user, args, state, true).await
}

pub async fn picker(
    bot: &Bot,
    chat: ChatId,
    user: Option<&teloxide::types::User>,
    action: &str,
    state: &BotState,
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
                return command(bot, chat, user, &format!("{before} {after}"), state, true).await;
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
                .send_message(chat, "diff_picker", &json!({"step":"invalid"}))
                .await?;
        } else {
            let keyboard = InlineKeyboardMarkup::new(buttons.chunks(2).map(|pair| pair.to_vec()));
            sender
                .send_message_with_keyboard(
                    chat,
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
            .send_message(chat, "error_generic", &json!({"error":error.to_string()}))
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
    chat: ChatId,
    user: Option<&teloxide::types::User>,
    args: &str,
    state: &BotState,
    compare: bool,
) -> Result<()> {
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
        if !crate::agent_access::admit(
            bot,
            chat,
            None,
            user,
            if compare { "diff" } else { "betadiff" },
            state,
        )
        .await?
        {
            return Ok(());
        }
        let model = state.ai_model().await?.cache_key();
        let mut rows = revisions(&state.beta.client, prev, cur).await?;
        if compare && rows.len() > 1 {
            rows = vec![rustverse_ai::beta::comparison_range(&rows)?];
        }
        let prev = &rows.first().context("empty changelog")?.previous;
        let cur = &rows.last().unwrap().current;
        let settings = SchedulerSettings::from_env();
        if rows.iter().all(|row| row.agents.is_empty()) {
            sender
                .send_message(chat, "beta_empty", &json!({"previous":prev,"current":cur}))
                .await?;
            return Ok(());
        }
        // Preflight the whole range so an over-budget command sends no partial albums.
        if !crate::agent_access::is_admin(user, state)
            && rows.iter().map(|r| r.agents.len()).sum::<usize>() > state.beta.manual_limit
        {
            let mut missing = 0;
            for row in &rows {
                for agent in &row.agents {
                    if cached_report(state, row, agent, &model).await?.is_none() {
                        missing += 1;
                        if missing > state.beta.manual_limit {
                            sender
                                .send_message(
                                    chat,
                                    "beta_limit",
                                    &json!({"limit":state.beta.manual_limit}),
                                )
                                .await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
        for row in &rows {
            RevisionAlbums::new(state, row, &model)
                .send(bot, chat, &settings, false)
                .await?;
        }
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
        for row in rows {
            if active.is_empty() {
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
            albums.send(bot, ChatId(chat), settings, true).await?;
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
        let page = |id| Page {
            id,
            file: InputFile::memory(vec![1]),
            photo: true,
        };
        let mut pages = [page(1601), page(1651)];
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
