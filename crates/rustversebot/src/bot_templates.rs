pub use crate::templates::RenderedTemplate;
use crate::templates::TemplateEngine;
use anyhow::{Context, bail};
use serde::Serialize;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardMarkup, InputFile, Message, ParseMode},
};

/// Telegram's documented text and media-caption limits.
const MESSAGE_LIMIT: usize = 4096;
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
        let rendered = self
            .templates
            .render(template, data)
            .with_context(|| format!("failed to render Telegram template {template}"))?;
        send_rendered_message(self.bot, chat_id, rendered)
            .await
            .with_context(|| format!("failed to send Telegram template {template}"))
    }

    pub async fn send_message_with_keyboard<S: Serialize>(
        &self,
        chat_id: ChatId,
        template: &str,
        data: &S,
        keyboard: InlineKeyboardMarkup,
    ) -> anyhow::Result<Message> {
        let rendered = self.templates.render(template, data)?;
        validate_length(template, &rendered.text, MESSAGE_LIMIT, "message")?;
        let mode = rendered.parse_mode();
        let request = self
            .bot
            .send_message(chat_id, rendered.text)
            .reply_markup(keyboard);
        match mode {
            Some(mode) => Ok(request.parse_mode(mode).await?),
            None => Ok(request.await?),
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
        validate_length(
            caption_template,
            &rendered.text,
            CAPTION_LIMIT,
            "photo caption",
        )?;
        let mode = rendered.parse_mode();
        let request = self
            .bot
            .send_photo(chat_id, photo)
            .caption(rendered.text)
            .reply_markup(keyboard);
        match mode {
            Some(mode) => Ok(request.parse_mode(mode).await?),
            None => Ok(request.await?),
        }
    }

    pub async fn send_rendered_message(
        &self,
        chat_id: ChatId,
        rendered: RenderedTemplate,
    ) -> anyhow::Result<Message> {
        send_rendered_message(self.bot, chat_id, rendered).await
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
    validate_length("composed", &rendered.text, MESSAGE_LIMIT, "message")?;

    let mode = selected_parse_mode(&rendered);
    let request = bot.send_message(chat_id, rendered.text);
    match mode {
        Some(mode) => Ok(request.parse_mode(mode).await?),
        None => Ok(request.await?),
    }
}

pub async fn send_photo_with_rendered(
    bot: &Bot,
    chat_id: ChatId,
    photo: InputFile,
    rendered: RenderedTemplate,
) -> anyhow::Result<Message> {
    validate_length("composed", &rendered.text, CAPTION_LIMIT, "photo caption")?;

    let mode = selected_parse_mode(&rendered);
    let request = bot.send_photo(chat_id, photo).caption(rendered.text);
    match mode {
        Some(mode) => Ok(request.parse_mode(mode).await?),
        None => Ok(request.await?),
    }
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
