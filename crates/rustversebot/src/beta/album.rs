use crate::scheduler::SchedulerSettings;
use anyhow::Result;
use teloxide::{
    prelude::*,
    types::{InputFile, InputMedia, InputMediaDocument, InputMediaPhoto, ParseMode, ThreadId},
};

const ALBUM_LIMIT: usize = 10;

pub(super) fn album_groups<T>(mut items: &[T]) -> Vec<&[T]> {
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
pub(super) struct Page {
    filename: String,
    png: bytes::Bytes,
    photo: bool,
}

impl Page {
    pub(super) fn new(id: u64, png: Vec<u8>) -> Result<Self> {
        anyhow::ensure!(png.len() >= 24, "incomplete PNG");
        let width = u32::from_be_bytes(png[16..20].try_into()?);
        let height = u32::from_be_bytes(png[20..24].try_into()?);
        let photo = width.saturating_add(height) <= 10000
            && width.max(height) <= width.min(height).saturating_mul(20)
            && png.len() <= 10_000_000;
        Ok(Self {
            photo,
            filename: format!("beta-{id}.png"),
            png: png.into(),
        })
    }
}

fn album_media(count: usize, caption: &str, photos: bool) -> Vec<InputMedia> {
    (0..count)
        .map(|index| {
            let file = InputFile::file_id(format!("attach://page{index}"));
            // Telegram requires a document album to contain documents exclusively.
            let caption = (index == 0).then(|| caption.to_owned());
            if photos {
                let mut media = InputMediaPhoto::new(file);
                media.parse_mode = caption.as_ref().map(|_| ParseMode::Html);
                media.caption = caption;
                InputMedia::Photo(media)
            } else {
                let mut media = InputMediaDocument::new(file);
                media.parse_mode = caption.as_ref().map(|_| ParseMode::Html);
                media.caption = caption;
                InputMedia::Document(media)
            }
        })
        .collect()
}

// teloxide 0.13 panics when its multipart serializer encounters ThreadId.
// Build the upload form with a numeric topic field, using the bot's configured client.
async fn upload_pages(
    bot: &Bot,
    chat: ChatId,
    thread: Option<ThreadId>,
    pages: &[Page],
    caption: &str,
    photos: bool,
) -> Result<(), teloxide::RequestError> {
    use telegram_http::multipart::{Form, Part};
    use teloxide::types::{False, ResponseParameters, True};
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Reply {
        Sent {
            #[serde(rename = "ok")]
            _ok: True,
            #[serde(rename = "result")]
            _result: serde::de::IgnoredAny,
        },
        Rejected {
            #[serde(rename = "ok")]
            _ok: False,
            description: teloxide::ApiError,
            parameters: Option<ResponseParameters>,
        },
    }

    let mut form = Form::new().text("chat_id", chat.0.to_string());
    if let Some(thread) = thread {
        form = form.text("message_thread_id", thread.0.0.to_string());
    }
    let single = pages.len() == 1;
    let kind = if photos { "photo" } else { "document" };
    let method = if single {
        form = form
            .text("caption", caption.to_owned())
            .text("parse_mode", "HTML");
        if photos { "sendPhoto" } else { "sendDocument" }
    } else {
        let media = album_media(pages.len(), caption, photos);
        form = form.text(
            "media",
            serde_json::to_string(&media).expect("media metadata is serializable"),
        );
        "sendMediaGroup"
    };
    for (index, page) in pages.iter().enumerate() {
        let field = if single {
            kind.to_owned()
        } else {
            format!("page{index}")
        };
        let file = Part::stream(page.png.clone())
            .file_name(page.filename.clone())
            .mime_str("image/png")?;
        form = form.part(field, file);
    }
    let url = bot
        .api_url()
        .join(&format!("/bot{}/{method}", bot.token()))
        .expect("Telegram API base URL");
    let body = bot
        .client()
        .post(url)
        .multipart(form)
        .send()
        .await?
        .text()
        .await?;
    let reply =
        serde_json::from_str(&body).map_err(|source| teloxide::RequestError::InvalidJson {
            source,
            raw: body.into(),
        })?;
    match reply {
        Reply::Sent { .. } => Ok(()),
        Reply::Rejected {
            description,
            parameters,
            ..
        } => Err(match parameters {
            Some(ResponseParameters::RetryAfter(wait)) => teloxide::RequestError::RetryAfter(wait),
            Some(ResponseParameters::MigrateToChatId(id)) => {
                teloxide::RequestError::MigrateToChatId(id)
            }
            None => description.into(),
        }),
    }
}

pub(super) async fn send_album(
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
    let mut photos = pages.iter().all(|page| page.photo);
    loop {
        tokio::time::sleep(settings.request_spacing).await;
        let result = upload_pages(bot, chat, thread, pages, caption, photos).await;
        match result {
            Ok(()) => return Ok(()),
            Err(teloxide::RequestError::Api(error))
                if photos
                    && (error == teloxide::ApiError::ImageProcessFailed
                        || matches!(error, teloxide::ApiError::Unknown(ref message) if message.contains("IMAGE_PROCESS_FAILED"))) =>
            {
                // Telegram rejected the photo upload. Keep the original PNGs together.
                photos = false;
                attempt = 1;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BotState,
        agent::{self, AgentRun, ChatAgent, MessageAddress, Outcome, Status},
        beta::{BetaPages, DiffStage, diff_status},
    };
    use serde_json::json;
    use std::time::Duration;
    #[tokio::test]
    async fn private_diff_retries_rejected_photos_as_files_before_removing_status() {
        use axum::{Json, Router, body::Bytes, http::Uri, routing::post};
        use std::sync::Arc;
        use teloxide::types::{MessageId, User};

        struct Album;
        impl ChatAgent for Album {
            fn initial_status(&self) -> Status<'_> {
                diff_status(Some("r1"), Some("r2"), DiffStage::Send, None)
            }

            async fn execute(&self, run: &mut AgentRun<'_>, _: &BotState) -> Result<Outcome> {
                let pages = ["a.png", "b.png"].map(|name| Page {
                    filename: name.to_owned(),
                    png: bytes::Bytes::from_static(&[1, 2, 3]),
                    photo: true,
                });
                let mut settings = SchedulerSettings::default();
                settings.request_spacing = Duration::ZERO;
                send_album(
                    run.bot(),
                    run.address().chat,
                    run.address().thread,
                    &pages,
                    "r1 → r2",
                    &settings,
                )
                .await?;
                Ok(Outcome::MediaDelivered)
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = teloxide::net::default_reqwest_settings()
            .no_proxy()
            .build()
            .unwrap();
        let bot = Bot::with_client("test", client).set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let (tx, mut calls) = tokio::sync::mpsc::unbounded_channel();
        let mock = Router::new().fallback(post(move |uri: Uri, body: Bytes| {
            let tx = tx.clone();
            async move {
                let method = uri.path().rsplit('/').next().unwrap().to_lowercase();
                let body = String::from_utf8_lossy(&body);
                let reply = match method.as_str() {
                    "sendmessage" => json!({"ok":true,"result":{"message_id":91,"date":1,"chat":{"id":1,"type":"private"},"text":"status"}}),
                    "sendmediagroup" if body.contains("\"type\":\"photo\"") =>
                        json!({"ok":false,"error_code":400,"description":"Bad Request: IMAGE_PROCESS_FAILED"}),
                    "sendmediagroup" => json!({"ok":true,"result":[{"message_id":92},{"message_id":93}]}),
                    "deletemessage" => json!({"ok":true,"result":true}),
                    _ => json!({"ok":false,"error_code":400,"description":"unexpected method"}),
                };
                tx.send((method, body.into_owned())).unwrap();
                Json(reply)
            }
        }));
        let server = tokio::spawn(axum::serve(listener, mock).into_future());
        let (ai_jobs, queue) = crate::ai_jobs::channel();
        let state = Arc::new(BotState {
            ai_jobs,
            prompts: Default::default(),
            beta: BetaPages::new().unwrap(),
            db: crate::db::Db::new_test().await.unwrap(),
            templates: crate::templates::TemplateEngine::new().unwrap(),
            admin_id: 1,
            public_web_url: None,
            nanoka: nanoka::NanokaClient::new(),
            http: reqwest::Client::new(),
        });
        let user: User =
            serde_json::from_value(json!({"id":1,"is_bot":false,"first_name":"Test"})).unwrap();
        let address = MessageAddress {
            chat: ChatId(1),
            message_id: MessageId(42),
            thread: Some(ThreadId(MessageId(5))),
        };
        agent::launch(&bot, address, Some(&user), "diff", &state, Album)
            .await
            .unwrap();
        queue.try_recv().unwrap().await.unwrap();
        server.abort();

        let mut requests = Vec::new();
        while let Ok(request) = calls.try_recv() {
            requests.push(request);
        }
        assert_eq!(
            requests
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            [
                "sendmessage",
                "sendmediagroup",
                "sendmediagroup",
                "deletemessage"
            ]
        );
        for (i, kind) in [(1, "photo"), (2, "document")] {
            let body = &requests[i].1;
            assert_eq!(body.matches(&format!("\"type\":\"{kind}\"")).count(), 2);
            assert!(
                body.contains("a.png")
                    && body.contains("b.png")
                    && body.contains("message_thread_id")
            );
        }
        let deletion: serde_json::Value = serde_json::from_str(&requests[3].1).unwrap();
        assert_eq!(deletion["message_id"], 91);
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
            filename: "test.png".into(),
            png: bytes::Bytes::from_static(&[1]),
            photo: true,
        };
        let mut pages = [page(), page()];
        for photos in [true, false] {
            pages[1].photo = photos;
            let media = album_media(pages.len(), "r1 → r2", pages.iter().all(|p| p.photo));
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
