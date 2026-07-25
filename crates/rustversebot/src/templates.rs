use anyhow::Context;
use minijinja::{AutoEscape, Environment, Value};
use teloxide::types::ParseMode;

/// Telegram output format declared by a template's filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateFormat {
    PlainText,
    Html,
}

impl TemplateFormat {
    pub fn parse_mode(self) -> Option<ParseMode> {
        match self {
            Self::PlainText => None,
            Self::Html => Some(ParseMode::Html),
        }
    }

    pub fn from_filename(filename: &str) -> anyhow::Result<Self> {
        if filename.ends_with(".txt.j2") {
            Ok(Self::PlainText)
        } else if filename.ends_with(".html.j2") {
            Ok(Self::Html)
        } else {
            anyhow::bail!("template filename {filename:?} must end in .txt.j2 or .html.j2")
        }
    }
}

/// Rendered Telegram content together with the parse mode inferred from its
/// source filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTemplate {
    pub text: String,
    pub format: TemplateFormat,
}

impl RenderedTemplate {
    pub fn parse_mode(&self) -> Option<ParseMode> {
        self.format.parse_mode()
    }

    /// Plain-text fallback suitable for reporting a rendering failure without
    /// accidentally interpreting error details as Telegram markup.
    pub fn error(error: anyhow::Error) -> Self {
        Self {
            text: format!("Error: {error}"),
            format: TemplateFormat::PlainText,
        }
    }
}

/// A minijinja environment containing templates embedded into the binary.
pub struct TemplateEngine {
    env: Environment<'static>,
}

struct EmbeddedTemplate {
    name: &'static str,
    filename: &'static str,
    source: &'static str,
}

macro_rules! template {
    ($name:literal, $format:literal) => {
        EmbeddedTemplate {
            name: $name,
            filename: concat!($name, ".", $format, ".j2"),
            source: include_str!(concat!("../templates/", $name, ".", $format, ".j2")),
        }
    };
}

const TEMPLATES: &[EmbeddedTemplate] = &[
    template!("welcome", "txt"),
    template!("register_success", "txt"),
    template!("register_already", "txt"),
    template!("register_invalid", "txt"),
    template!("register_not_public", "html"),
    template!("register_error", "txt"),
    template!("unregister_success", "txt"),
    template!("unregister_not_found", "txt"),
    template!("status", "html"),
    template!("top_header", "html"),
    template!("top_entry", "html"),
    template!("top_empty", "html"),
    template!("top_footer", "html"),
    template!("da_detail", "txt"),
    template!("shiyu_detail", "txt"),
    template!("cookie_updated", "txt"),
    template!("cookie_invalid", "txt"),
    template!("no_cookie", "txt"),
    template!("refetch_all_start", "txt"),
    template!("refetch_all_empty", "txt"),
    template!("refetch_all_result", "txt"),
    template!("refetch_all_error", "txt"),
    template!("refetch_uid_start", "txt"),
    template!("refetch_uid_success", "txt"),
    template!("refetch_uid_not_public", "txt"),
    template!("refetch_uid_error", "txt"),
    template!("invalid_uid", "txt"),
    template!("data_not_public", "txt"),
    template!("error_generic", "txt"),
    template!("not_admin", "txt"),
    template!("cache_miss", "txt"),
    template!("detail_caption", "html"),
    template!("choose_uid", "html"),
    template!("uids_list", "html"),
    template!("uids_empty", "html"),
    template!("deadly_info_announcement", "html"),
    template!("shiyu_info_announcement", "html"),
];

impl TemplateEngine {
    pub fn new() -> anyhow::Result<Self> {
        let mut env = Environment::new();
        env.set_auto_escape_callback(|filename| {
            if filename.ends_with(".html.j2") {
                AutoEscape::Html
            } else {
                AutoEscape::None
            }
        });

        for template in TEMPLATES {
            TemplateFormat::from_filename(template.filename)?;

            // Most of the former Rust string literals did not end in a newline.
            // Keep that behavior while allowing text files to remain POSIX-style.
            let source = if matches!(template.name, "status" | "top_header" | "top_entry") {
                template.source
            } else {
                template
                    .source
                    .strip_suffix('\n')
                    .unwrap_or(template.source)
            };

            env.add_template(template.filename, source)
                .with_context(|| format!("failed to compile template {}", template.filename))?;
        }

        Ok(Self { env })
    }

    pub fn render<S: serde::Serialize>(
        &self,
        name: &str,
        data: &S,
    ) -> anyhow::Result<RenderedTemplate> {
        let embedded = TEMPLATES
            .iter()
            .find(|template| template.name == name)
            .with_context(|| format!("unknown template {name:?}"))?;
        let template = self.env.get_template(embedded.filename)?;

        Ok(RenderedTemplate {
            text: template.render(Value::from_serialize(data))?,
            format: TemplateFormat::from_filename(embedded.filename)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{TemplateEngine, TemplateFormat, TEMPLATES};
    use minijinja::{AutoEscape, Environment};
    use serde_json::json;
    use std::{collections::BTreeSet, fs, path::PathBuf};
    use teloxide::types::ParseMode;

    #[test]
    fn validates_every_template_file_and_registry_entry() {
        let templates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("templates");
        let mut files = BTreeSet::new();
        let mut environment = Environment::new();
        environment.set_auto_escape_callback(|filename| {
            if filename.ends_with(".html.j2") {
                AutoEscape::Html
            } else {
                AutoEscape::None
            }
        });

        for entry in fs::read_dir(&templates_dir).expect("templates directory should be readable") {
            let path = entry
                .expect("template directory entry should be readable")
                .path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("j2") {
                continue;
            }

            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("template filename should be valid UTF-8")
                .to_owned();
            TemplateFormat::from_filename(&filename)
                .unwrap_or_else(|error| panic!("invalid template filename {filename}: {error}"));
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));

            environment
                .add_template_owned(filename.clone(), source)
                .unwrap_or_else(|error| panic!("invalid template {}: {error}", path.display()));
            assert!(files.insert(filename), "duplicate template file");
        }

        let registered = TEMPLATES
            .iter()
            .map(|template| template.filename.to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered.len(),
            TEMPLATES.len(),
            "every template must be registered exactly once"
        );
        assert_eq!(
            files, registered,
            "every templates/*.j2 file must be embedded exactly once"
        );
    }

    #[test]
    fn all_embedded_templates_compile_and_render() {
        let engine = TemplateEngine::new().expect("templates should compile");

        for template in TEMPLATES {
            engine
                .render(template.name, &json!({}))
                .unwrap_or_else(|error| {
                    panic!("template {} should render: {error}", template.filename)
                });
        }
    }

    #[test]
    fn suffix_determines_telegram_format() {
        let engine = TemplateEngine::new().expect("templates should compile");

        let html = engine.render("status", &json!({ "seasons": [] })).unwrap();
        assert_eq!(html.format, TemplateFormat::Html);
        assert_eq!(html.parse_mode(), Some(ParseMode::Html));

        let text = engine
            .render("register_invalid", &json!({ "uid": 1 }))
            .unwrap();
        assert_eq!(text.format, TemplateFormat::PlainText);
        assert_eq!(text.parse_mode(), None);
    }

    #[test]
    fn html_values_are_escaped_and_plain_text_values_are_not() {
        let engine = TemplateEngine::new().expect("templates should compile");
        let hostile = r#"<Admin & "User">"#;

        let html = engine
            .render(
                "detail_caption",
                &json!({ "event": hostile, "nickname": hostile, "uid": hostile }),
            )
            .unwrap();
        assert!(!html.text.contains(hostile));
        assert!(html.text.contains("&lt;Admin &amp;"));
        assert!(
            html.text.contains("&quot;User&quot;&gt;"),
            "unexpected HTML escaping: {}",
            html.text
        );

        let text = engine
            .render("register_invalid", &json!({ "uid": hostile }))
            .unwrap();
        assert!(text.text.contains(hostile));
    }

    #[test]
    fn renders_template_variables_and_control_flow() {
        let engine = TemplateEngine::new().expect("templates should compile");
        let rendered = engine
            .render(
                "top_entry",
                &json!({
                    "position": 1,
                    "display_name": "Прокси",
                    "score_str": "42 000",
                    "extra": "6★"
                }),
            )
            .expect("top entry should render");

        assert_eq!(rendered.text, "1. <b>Прокси</b> — 42 000 | 6★");
    }
}
