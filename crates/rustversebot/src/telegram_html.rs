//! Convert model Markdown to the HTML entities accepted by Telegram. Split on
//! visible UTF-16 units, closing and reopening active entities at each boundary.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

#[derive(Clone, PartialEq)]
struct Entity {
    open: String,
    close: &'static str,
}
impl Entity {
    fn new(tag: &'static str, close: &'static str) -> Self {
        Self {
            open: tag.into(),
            close,
        }
    }
}

#[derive(Default)]
struct Output {
    limit: usize,
    chunks: Vec<String>,
    html: String,
    units: usize,
    active: Vec<Entity>,
    break_lines: usize,
    item_boundary: bool,
    // The most recent paragraph starts with a clean entity stack. Keep its HTML
    // offsets and visible position so an overflow can move the whole paragraph.
    paragraph: Option<(usize, usize, usize)>,
}

impl Output {
    fn with_question(question: &str, limit: usize) -> Self {
        let mut out = Self {
            limit,
            ..Self::default()
        };
        out.write_paragraphs(question, &[Entity::new("<blockquote>", "</blockquote>")]);
        out.break_lines = 2;
        out
    }

    fn finish(mut self) -> Vec<String> {
        self.finish_chunk();
        self.chunks
    }

    fn style(&mut self, next: &[Entity]) {
        let shared = self
            .active
            .iter()
            .zip(next)
            .take_while(|(a, b)| a == b)
            .count();
        for e in self.active.drain(shared..).rev() {
            self.html.push_str(e.close);
        }
        for e in &next[shared..] {
            self.html.push_str(&e.open);
            self.active.push(e.clone());
        }
    }
    fn finish_chunk(&mut self) {
        self.paragraph = None;
        self.style(&[]);
        if self.units > 0 {
            self.chunks.push(std::mem::take(&mut self.html));
        }
        self.units = 0;
    }
    fn move_paragraph(&mut self) {
        if let Some((end, start, units)) = self.paragraph.take() {
            let tail = self.html.split_off(start);
            self.html.truncate(end);
            self.chunks.push(std::mem::replace(&mut self.html, tail));
            self.units -= units;
        }
    }
    fn write_paragraphs(&mut self, text: &str, style: &[Entity]) {
        for (i, paragraph) in text.split("\n\n").enumerate() {
            if i > 0 {
                self.break_lines += 2;
            }
            self.write(paragraph, style);
        }
    }
    fn write(&mut self, text: &str, style: &[Entity]) {
        if text.is_empty() {
            return;
        }
        let breaks = if self.units > 0 { self.break_lines } else { 0 };
        self.break_lines = 0;
        let item_boundary = std::mem::take(&mut self.item_boundary);
        if breaks > 0 {
            self.style(&[]);
            if breaks == 1 && !item_boundary && self.units + breaks >= self.limit {
                self.move_paragraph();
            }
            if self.units + breaks >= self.limit {
                self.finish_chunk();
            } else {
                if breaks >= 2 || item_boundary {
                    self.paragraph = Some((
                        self.html.len(),
                        self.html.len() + breaks,
                        self.units + breaks,
                    ));
                }
                for _ in 0..breaks {
                    self.html.push('\n');
                }
                self.units += breaks;
            }
        }
        for word in text.split_inclusive(char::is_whitespace) {
            let width = word.encode_utf16().count();
            if self.units + width > self.limit {
                self.move_paragraph();
            }
            if width <= self.limit && self.units + width > self.limit {
                self.finish_chunk();
            }
            self.style(style);
            for ch in word.chars() {
                if self.units + ch.len_utf16() > self.limit {
                    self.finish_chunk();
                    self.style(style);
                }
                match ch {
                    '<' => self.html.push_str("&lt;"),
                    '>' => self.html.push_str("&gt;"),
                    '&' => self.html.push_str("&amp;"),
                    _ => self.html.push(ch),
                }
                self.units += ch.len_utf16();
            }
        }
    }
}

fn link(url: &str) -> Option<Entity> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
        return None;
    }
    let href = parsed
        .as_str()
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Some(Entity {
        open: format!("<a href=\"{href}\">"),
        close: "</a>",
    })
}

pub fn plain_chunks(question: &str, text: &str, limit: usize) -> Vec<String> {
    let mut out = Output::with_question(question, limit);
    out.write_paragraphs(text, &[]);
    out.finish()
}

pub fn chunks(question: &str, markdown: &str, limit: usize) -> Vec<String> {
    let mut out = Output::with_question(question, limit);
    let mut styles = Vec::<Entity>::new();
    let mut depths = Vec::new();
    let mut lists = Vec::<Option<u64>>::new();
    for event in Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Start(tag) => {
                depths.push(styles.len());
                let entity = match tag {
                    Tag::Strong | Tag::Heading { .. } => Some(Entity::new("<b>", "</b>")),
                    Tag::Emphasis => Some(Entity::new("<i>", "</i>")),
                    Tag::Strikethrough => Some(Entity::new("<s>", "</s>")),
                    Tag::BlockQuote(_) => Some(Entity::new("<blockquote>", "</blockquote>")),
                    Tag::CodeBlock(_) => Some(Entity::new("<pre>", "</pre>")),
                    Tag::Link { dest_url, .. } => link(&dest_url),
                    Tag::List(start) => {
                        lists.push(start);
                        None
                    }
                    Tag::Item => {
                        let prefix = match lists.last_mut() {
                            Some(Some(i)) => {
                                let prefix = format!("{i}. ");
                                *i = i.saturating_add(1);
                                prefix
                            }
                            _ => "• ".into(),
                        };
                        out.write(&prefix, &styles);
                        None
                    }
                    _ => None,
                };
                if let Some(e) = entity
                    && !styles.contains(&e)
                {
                    styles.push(e);
                }
            }
            Event::End(tag) => {
                styles.truncate(depths.pop().unwrap_or_default());
                match tag {
                    TagEnd::List(_) => {
                        lists.pop();
                        out.break_lines = 2;
                    }
                    // Loose Markdown list items end in a paragraph with a blank line.
                    TagEnd::Item => {
                        out.item_boundary = true;
                        out.break_lines = out.break_lines.max(1);
                    }
                    TagEnd::Paragraph
                    | TagEnd::Heading(_)
                    | TagEnd::BlockQuote(_)
                    | TagEnd::CodeBlock => out.break_lines = 2,
                    _ => {}
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                // Telegram forbids nesting other entities inside a code block.
                if let Some(pre) = styles.last().filter(|e| e.close == "</pre>") {
                    out.write(&text, std::slice::from_ref(pre));
                } else {
                    out.write(&text, &styles);
                }
            }
            Event::Code(text) => out.write(&text, &[Entity::new("<code>", "</code>")]),
            Event::SoftBreak | Event::HardBreak => out.break_lines = 1,
            Event::Rule => {
                out.break_lines = 1;
                out.write("──", &[]);
                out.break_lines = 1;
            }
            _ => {}
        }
    }
    out.finish()
}

/// Keep the live status compact. The final reply quotes the full question.
pub fn question_preview(question: &str, limit: usize) -> String {
    let mut units = 0;
    let preview_question = question
        .char_indices()
        .find_map(|(i, ch)| {
            units += ch.len_utf16();
            (units > limit / 2).then_some(i)
        })
        .map(|i| format!("{}…", &question[..i]));
    Output::with_question(preview_question.as_deref().unwrap_or(question), limit)
        .finish()
        .into_iter()
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_templates::MESSAGE_LIMIT;
    #[test]
    fn renders_only_telegram_entities_and_reopens_long_spans() {
        let html = chunks(
            "",
            "## Гайд\n\n**Урон** и *шанс* · [источник](https://example.com/?a=1&b=2) · `<x>` · [ссылка](javascript:foo) <bad>",
            MESSAGE_LIMIT,
        );
        let s = &html[0];
        for part in [
            "<b>Гайд</b>",
            "<b>Урон</b>",
            "<i>шанс</i>",
            "<a href=\"https://example.com/?a=1&amp;b=2\">источник</a>",
            "<code>&lt;x&gt;</code>",
            "&lt;bad&gt;",
        ] {
            assert!(s.contains(part), "{s}");
        }
        assert!(!s.contains("javascript:"));
        let pages = chunks("", &format!("**{}**", "🌙".repeat(2050)), MESSAGE_LIMIT);
        assert_eq!(pages.len(), 2);
        assert!(
            pages
                .iter()
                .all(|s| s.starts_with("<b>") && s.ends_with("</b>"))
        );
    }
    #[test]
    fn packs_whole_paragraphs_and_preserves_inline_entities() {
        let parts = chunks(
            "",
            "Начало\n\n**Раз** и [два](https://example.org)\n\nКонец",
            12,
        );
        assert_eq!(
            parts,
            [
                "Начало",
                "<b>Раз</b> и <a href=\"https://example.org/\">два</a>",
                "Конец"
            ]
        );
        assert_eq!(
            plain_chunks("", "Один два\n\nТри 🌙", 12),
            ["Один два", "Три 🌙"]
        );
        assert_eq!(chunks("", "Один\n\nДва\n\nТри", 9), ["Один\n\nДва", "Три"]);
        assert_eq!(
            chunks("", "Слово **раз два три**", 9),
            ["Слово ", "<b>раз два </b>", "<b>три</b>"]
        );
    }

    #[test]
    fn preserves_paragraph_spacing_in_loose_lists_and_keeps_tight_lists_compact() {
        let markdown = "**Сейчас**\n\n- Рокси\n\n- Рина\n\n**После патча**\n\n1. Диски\n2. Команда";
        assert_eq!(
            chunks("", markdown, MESSAGE_LIMIT),
            ["<b>Сейчас</b>\n\n• Рокси\n\n• Рина\n\n<b>После патча</b>\n\n1. Диски\n2. Команда"]
        );
    }
}
