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
}

impl Output {
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
        self.style(&[]);
        if self.units > 0 {
            self.chunks.push(std::mem::take(&mut self.html));
        }
        self.units = 0;
    }
    fn write(&mut self, text: &str, style: &[Entity]) {
        if text.is_empty() {
            return;
        }
        let breaks = if self.units > 0 { self.break_lines } else { 0 };
        self.break_lines = 0;
        if breaks > 0 {
            self.style(&[]);
            if self.units + breaks >= self.limit {
                self.finish_chunk();
            } else {
                for _ in 0..breaks {
                    self.html.push('\n');
                }
                self.units += breaks;
            }
        }
        self.style(style);
        for ch in text.chars() {
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

pub fn chunks(markdown: &str, limit: usize) -> Vec<String> {
    let mut out = Output {
        limit,
        ..Output::default()
    };
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
                    TagEnd::Item => out.break_lines = 1,
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
    out.finish_chunk();
    out.chunks
}

/// Complete an open inline span for this preview only. CommonMark decides
/// whether a suffix actually closes a span, so underscores inside IDs survive.
pub fn preview(markdown: &str, limit: usize) -> String {
    let mut source = markdown.to_owned();
    if let Some(pos) = source.rfind("](")
        && !source[pos + 2..].contains([')', '\n'])
        && let Some(start) = source[..pos].rfind('[')
    {
        source.truncate(pos);
        source.remove(start);
    }
    if !source.is_empty() && !source.ends_with(char::is_whitespace) {
        for suffix in ["***", "**", "__", "~~", "*", "_", "`"] {
            if !source.contains(suffix) {
                continue;
            }
            let candidate = format!("{source}{suffix}");
            let closes = Parser::new_ext(&candidate, Options::ENABLE_STRIKETHROUGH)
                .into_offset_iter()
                .any(|(event, range)| {
                    range.end == candidate.len()
                        && range.start < source.len()
                        && matches!(
                            event,
                            Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough)
                                | Event::Code(_)
                        )
                });
            if closes {
                source = candidate;
                break;
            }
        }
    }
    chunks(&source, limit)
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
        let pages = chunks(&format!("**{}**", "🌙".repeat(2050)), MESSAGE_LIMIT);
        assert_eq!(pages.len(), 2);
        assert!(
            pages
                .iter()
                .all(|s| s.starts_with("<b>") && s.ends_with("</b>"))
        );
    }
    #[test]
    fn partial_markup_is_balanced() {
        assert_eq!(preview("Урон **вырос", MESSAGE_LIMIT), "Урон <b>вырос</b>");
        assert_eq!(
            preview("см. [Гайд](https://exam", MESSAGE_LIMIT),
            "см. Гайд"
        );
        assert_eq!(
            preview("beta_3.4.5_20699967", MESSAGE_LIMIT),
            "beta_3.4.5_20699967"
        );
    }
}
