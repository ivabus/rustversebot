use crate::telegram_trace::TraceRequest;
pub use crate::templates::RenderedTemplate;
use crate::templates::TemplateEngine;
use anyhow::{Context, bail};
use serde::Serialize;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardMarkup, InputFile, Message, ParseMode},
};

/// Telegram's documented text and media-caption limits.
pub(crate) const MESSAGE_LIMIT: usize = 4096;
const CAPTION_LIMIT: usize = 1024;

/// Renders templates and sends them with the parse mode implied by their
/// filename. Keeping this decision here prevents callers from accidentally
/// sending an HTML template as plain text (or vice versa).
pub struct BotTemplateSender<'a> {
    bot: &'a Bot,
    templates: &'a TemplateEngine,
}

impl<'a> BotTemplateSender<'a> {
    pub fn new(bot: &'a Bot, templates: &'a TemplateEngine) -> Self {
        Self { bot, templates }
    }

    pub async fn send_message<S: Serialize>(
        &self,
        chat_id: ChatId,
        template: &str,
        data: &S,
    ) -> anyhow::Result<Message> {
        self.send_to_thread(chat_id, None, template, data).await
    }

    pub async fn send_to_thread<S: Serialize>(
        &self,
        chat_id: ChatId,
        thread: Option<teloxide::types::ThreadId>,
        template: &str,
        data: &S,
    ) -> anyhow::Result<Message> {
        let rendered = self
            .templates
            .render(template, data)
            .with_context(|| format!("failed to render Telegram template {template}"))?;
        send_rendered_to_thread(self.bot, chat_id, thread, rendered)
            .await
            .with_context(|| format!("failed to send Telegram template {template}"))
    }

    pub async fn send_message_with_keyboard<S: Serialize>(
        &self,
        chat_id: ChatId,
        thread: Option<teloxide::types::ThreadId>,
        template: &str,
        data: &S,
        keyboard: InlineKeyboardMarkup,
    ) -> anyhow::Result<Message> {
        let rendered = self.templates.render(template, data)?;
        validate_length(template, &rendered.text, MESSAGE_LIMIT, "message")?;
        let mode = rendered.parse_mode();
        let mut request = self
            .bot
            .send_message(chat_id, rendered.text)
            .reply_markup(keyboard);
        if let Some(thread) = thread {
            request = request.message_thread_id(thread);
        }
        match mode {
            Some(mode) => Ok(request.parse_mode(mode).traced().await?),
            None => Ok(request.traced().await?),
        }
    }

    pub async fn send_photo<S: Serialize>(
        &self,
        chat_id: ChatId,
        photo: InputFile,
        caption_template: &str,
        data: &S,
    ) -> anyhow::Result<Message> {
        let rendered = self
            .templates
            .render(caption_template, data)
            .with_context(|| {
                format!("failed to render Telegram caption template {caption_template}")
            })?;
        send_photo_with_rendered(self.bot, chat_id, photo, rendered)
            .await
            .with_context(|| format!("failed to send Telegram caption template {caption_template}"))
    }

    pub async fn send_photo_with_keyboard<S: Serialize>(
        &self,
        chat_id: ChatId,
        photo: InputFile,
        caption_template: &str,
        data: &S,
        keyboard: InlineKeyboardMarkup,
    ) -> anyhow::Result<Message> {
        let rendered = self.templates.render(caption_template, data)?;
        send_card(self.bot, chat_id, photo, rendered, Some(keyboard)).await
    }

    pub async fn send_rendered_message(
        &self,
        chat_id: ChatId,
        rendered: RenderedTemplate,
    ) -> anyhow::Result<Message> {
        send_rendered_message(self.bot, chat_id, rendered).await
    }

    pub async fn send_rendered_to_thread(
        &self,
        chat: ChatId,
        thread: Option<teloxide::types::ThreadId>,
        rendered: RenderedTemplate,
    ) -> anyhow::Result<Message> {
        send_rendered_to_thread(self.bot, chat, thread, rendered).await
    }

    pub async fn send_photo_with_rendered(
        &self,
        chat_id: ChatId,
        photo: InputFile,
        rendered: RenderedTemplate,
    ) -> anyhow::Result<Message> {
        send_photo_with_rendered(self.bot, chat_id, photo, rendered).await
    }
}

pub async fn send_rendered_message(
    bot: &Bot,
    chat_id: ChatId,
    rendered: RenderedTemplate,
) -> anyhow::Result<Message> {
    send_rendered_to_thread(bot, chat_id, None, rendered).await
}

async fn send_rendered_to_thread(
    bot: &Bot,
    chat_id: ChatId,
    thread: Option<teloxide::types::ThreadId>,
    rendered: RenderedTemplate,
) -> anyhow::Result<Message> {
    // Telegram applies its limit after parsing entities. Escaped code and URLs
    // can make the HTML source substantially longer than its visible text.
    if rendered.parse_mode() == Some(ParseMode::Html) && rendered.text.len() > MESSAGE_LIMIT {
        let visible = scraper::Html::parse_fragment(&rendered.text)
            .root_element()
            .text()
            .collect::<String>();
        validate_length("composed", &visible, MESSAGE_LIMIT, "message")?;
    } else {
        validate_length("composed", &rendered.text, MESSAGE_LIMIT, "message")?;
    }

    let mode = selected_parse_mode(&rendered);
    let mut request = bot.send_message(chat_id, rendered.text);
    if let Some(thread) = thread {
        request = request.message_thread_id(thread);
    }
    match mode {
        Some(mode) => Ok(request.parse_mode(mode).traced().await?),
        None => Ok(request.traced().await?),
    }
}

pub async fn send_photo_with_rendered(
    bot: &Bot,
    chat_id: ChatId,
    photo: InputFile,
    rendered: RenderedTemplate,
) -> anyhow::Result<Message> {
    send_card(bot, chat_id, photo, rendered, None).await
}

async fn send_card(
    bot: &Bot,
    chat_id: ChatId,
    photo: InputFile,
    rendered: RenderedTemplate,
    keyboard: Option<InlineKeyboardMarkup>,
) -> anyhow::Result<Message> {
    validate_length("composed", &rendered.text, CAPTION_LIMIT, "photo caption")?;
    let mode = selected_parse_mode(&rendered);
    let mut request = bot
        .send_photo(chat_id, photo.clone())
        .caption(rendered.text.clone());
    if let Some(mode) = mode {
        request = request.parse_mode(mode);
    }
    if let Some(keyboard) = keyboard.clone() {
        request = request.reply_markup(keyboard);
    }
    match request.traced().await {
        Ok(message) => return Ok(message),
        Err(teloxide::RequestError::Api(error))
            if error == teloxide::ApiError::ImageProcessFailed
                || matches!(error,teloxide::ApiError::Unknown(ref s) if s.contains("PHOTO_INVALID_DIMENSIONS") || s.contains("PHOTO_TOO_BIG")) =>
            {}
        Err(error) => return Err(error.into()),
    }
    let mut document = bot
        .send_document(chat_id, photo.file_name("card.png"))
        .caption(rendered.text);
    if let Some(mode) = mode {
        document = document.parse_mode(mode);
    }
    if let Some(keyboard) = keyboard {
        document = document.reply_markup(keyboard);
    }
    Ok(document.traced().await?)
}

fn selected_parse_mode(rendered: &RenderedTemplate) -> Option<ParseMode> {
    rendered.parse_mode()
}

fn validate_length(
    template: &str,
    text: &str,
    limit: usize,
    destination: &str,
) -> anyhow::Result<()> {
    let length = text.chars().count();
    if length > limit {
        bail!(
            "rendered template {template} is too long for a Telegram {destination}: \
             {length} characters (limit {limit})"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RenderedTemplate, selected_parse_mode, validate_length};
    use crate::templates::TemplateFormat;
    use teloxide::types::ParseMode;

    #[test]
    fn applies_html_mode_only_to_html_templates() {
        let html = RenderedTemplate {
            text: "<b>test</b>".to_owned(),
            format: TemplateFormat::Html,
        };
        let plain = RenderedTemplate {
            text: "<b>test</b>".to_owned(),
            format: TemplateFormat::PlainText,
        };

        assert_eq!(selected_parse_mode(&html), Some(ParseMode::Html));
        assert_eq!(selected_parse_mode(&plain), None);
    }

    #[test]
    fn accepts_text_at_telegram_limit() {
        let text = "я".repeat(16);
        validate_length("test", &text, 16, "message").expect("text at limit should be accepted");
    }

    #[test]
    fn rejects_text_over_telegram_limit() {
        let error = validate_length("oversized", "12345", 4, "caption")
            .expect_err("oversized text should be rejected");

        assert!(error.to_string().contains("oversized"));
        assert!(error.to_string().contains("5 characters (limit 4)"));
    }
}
