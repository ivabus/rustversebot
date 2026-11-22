use super::*;
use anyhow::{Context, ensure};
use futures_util::StreamExt;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use teloxide::{
    net::Download,
    types::{Me, ThreadId},
    utils::command::BotCommands,
};
use tokio::{
    sync::Mutex,
    time::{Instant, sleep_until},
};

// Includes the current turn and any earlier attachments reloaded for the same branch.
pub(super) const IMAGE_BUDGET: usize = 24 * 1024 * 1024;
const ALBUM_QUIET: Duration = Duration::from_millis(1200);

pub(super) fn image_id(msg: &Message) -> Option<&str> {
    if let Some(photo) = msg
        .photo()
        .into_iter()
        .flatten()
        .max_by_key(|p| u64::from(p.width) * u64::from(p.height))
    {
        return Some(&photo.file.id);
    }
    let doc = msg.document()?;
    let supported = doc
        .mime_type
        .as_ref()
        .is_some_and(|m| m.type_().as_str() == "image")
        || doc
            .file_name
            .as_deref()
            .and_then(|s| s.rsplit_once('.'))
            .is_some_and(|(_, ext)| {
                ["jpg", "jpeg", "png", "webp"]
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(ext))
            });
    supported.then_some(&doc.file.id)
}

pub(crate) fn is_media(msg: Message) -> bool {
    msg.media_group_id().is_some() || image_id(&msg).is_some()
}

type AlbumKey = (ChatId, Option<ThreadId>, Option<UserId>, String);

struct Album {
    messages: BTreeMap<i32, Message>,
    deadline: Instant,
}

#[derive(Clone, Default)]
pub(crate) struct Albums(Arc<Mutex<HashMap<AlbumKey, Album>>>);

pub(crate) async fn receive(
    bot: Bot,
    msg: Message,
    me: Me,
    state: Arc<BotState>,
    albums: Albums,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let Some(group) = msg.media_group_id() else {
        return dispatch(&bot, &[msg], me.username(), &state).await;
    };
    // Telegram delivers each item separately and provides no final-item marker.
    // Return to the dispatcher immediately so that it can receive the rest of this chat's album.
    let key = (
        msg.chat.id,
        topic(&msg),
        msg.from.as_ref().map(|u| u.id),
        group.to_owned(),
    );
    if let Some(batch) = albums.collect(key, msg).await {
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => {}
                result = async { dispatch(&bot, &batch.await, me.username(), &state).await } => {
                    if let Err(error) = result {
                        log::warn!("Could not handle prompt album: {error}");
                    }
                }
            }
        });
    }
    Ok(())
}

impl Albums {
    async fn collect(
        self,
        key: AlbumKey,
        msg: Message,
    ) -> Option<impl Future<Output = Vec<Message>>> {
        use std::collections::hash_map::Entry;
        let deadline = Instant::now() + ALBUM_QUIET;
        {
            let mut pending = self.0.lock().await;
            match pending.entry(key.clone()) {
                Entry::Occupied(mut entry) => {
                    let album = entry.get_mut();
                    album.messages.insert(msg.id.0, msg);
                    album.deadline = deadline;
                    return None;
                }
                Entry::Vacant(entry) => {
                    entry.insert(Album {
                        messages: BTreeMap::from([(msg.id.0, msg)]),
                        deadline,
                    });
                }
            }
        }
        Some(async move {
            let mut next = deadline;
            loop {
                sleep_until(next).await;
                let mut pending = self.0.lock().await;
                next = pending[&key].deadline;
                if next <= Instant::now() {
                    return pending
                        .remove(&key)
                        .unwrap()
                        .messages
                        .into_values()
                        .collect();
                }
            }
        })
    }
}

struct MediaQuestion<'a> {
    anchor: &'a Message,
    command: bool,
    text: String,
    images: Vec<String>,
}

fn media_question<'a>(messages: &'a [Message], username: &str) -> Option<MediaQuestion<'a>> {
    let mut images = Vec::new();
    let mut captions = Vec::new();
    let mut command = None;
    let mut reply = None;
    for msg in messages {
        if let Some(id) = image_id(msg) {
            images.push(id.to_owned());
        }
        if msg.reply_to_message().is_some() {
            reply.get_or_insert(msg);
        }
        let caption = msg.caption().unwrap_or_default().trim();
        let text = if caption.starts_with('/') {
            let (name, args) = caption
                .split_once(char::is_whitespace)
                .unwrap_or((caption, ""));
            if !matches!(
                crate::Command::parse(name, username),
                Ok(crate::Command::Prompt(_))
            ) {
                return None;
            }
            command = Some(msg);
            args.trim()
        } else {
            caption
        };
        if !text.is_empty() {
            captions.push(text);
        }
    }
    if images.is_empty() {
        return None;
    }
    Some(MediaQuestion {
        anchor: reply.or(command)?,
        command: command.is_some(),
        text: captions.join("\n"),
        images,
    })
}

async fn dispatch(
    bot: &Bot,
    messages: &[Message],
    username: &str,
    state: &Arc<BotState>,
) -> Result<()> {
    let Some(q) = media_question(messages, username) else {
        return Ok(());
    };
    if q.command {
        if let Err(error) = state
            .db
            .observe_chat(q.anchor.chat.id.0, q.anchor.chat.title())
            .await
        {
            log::warn!("Could not record prompt chat: {error}");
        }
        super::start(bot, q.anchor, &q.text, state, q.images).await
    } else {
        super::continue_reply(bot, q.anchor, &q.text, state, q.images).await
    }
}

pub(super) async fn download(
    bot: &Bot,
    ids: &[String],
    remaining: &mut usize,
) -> Result<Vec<rustverse_ai::PromptImage>> {
    let mut images = Vec::with_capacity(ids.len());
    let mut budget = *remaining;
    for id in ids {
        const SIZE_ERROR: &str = "Изображения не помещаются в 24 МиБ. Пришлите меньше изображений или отправьте их как фото.";
        ensure!(budget > 0, SIZE_ERROR);
        let file = bot
            .get_file(id)
            .await
            .map_err(|_| anyhow::anyhow!("Не удалось получить изображение из Telegram."))?;
        ensure!(
            file.size == u32::MAX || file.size as usize <= budget,
            SIZE_ERROR
        );
        let mut stream = bot.download_file_stream(&file.path);
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .ok()
                .context("Не удалось скачать изображение из Telegram.")?;
            ensure!(chunk.len() <= budget - bytes.len(), SIZE_ERROR);
            bytes.extend_from_slice(&chunk);
        }
        images.push(rustverse_ai::PromptImage::from_bytes(&bytes)?);
        budget -= bytes.len();
    }
    *remaining = budget;
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn photo(id: i32, caption: Option<&str>, reply: bool) -> Message {
        let chat = json!({"id":-100123,"type":"supergroup","title":"ZZZ"});
        let mut msg = json!({"message_id":id,"date":1,"chat":chat, "media_group_id":"album",
            "photo":[{"width":1280,"height":720,"file_id":id.to_string(),"file_unique_id":"large"},
                {"width":90,"height":50,"file_id":"thumb","file_unique_id":"small"}]});
        if let Some(caption) = caption {
            msg["caption"] = json!(caption);
        }
        if reply {
            msg["reply_to_message"] = json!({"message_id":8,"date":1,"chat":chat,"text":"Ответ"});
        }
        serde_json::from_value(msg).unwrap()
    }

    #[tokio::test]
    async fn one_album_reader_collects_out_of_order_updates_and_deduplicates_retries() {
        let albums = Albums::default();
        let key = (ChatId(-100123), None, None, "album".to_owned());
        let batch = albums
            .clone()
            .collect(
                key.clone(),
                photo(13, Some("/prompt@gamebot\nСравни сборки"), false),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            assert!(
                albums
                    .clone()
                    .collect(key.clone(), photo(12, None, false))
                    .await
                    .is_none()
            );
        }
        let messages = batch.await;
        let q = media_question(&messages, "gamebot").unwrap();
        assert!(q.command);
        assert_eq!(q.text, "Сравни сборки");
        assert_eq!(q.images, ["12", "13"]);
        assert!(albums.0.lock().await.is_empty());
        assert!(media_question(&messages, "otherbot").is_none());
    }

    #[test]
    fn reply_anchor_and_caption_can_belong_to_different_album_items() {
        let batch = [
            photo(12, None, true),
            photo(13, Some("Второй вариант"), false),
        ];
        let q = media_question(&batch, "gamebot").unwrap();
        assert!(!q.command);
        assert_eq!(q.anchor.reply_to_message().unwrap().id.0, 8);
        assert_eq!(q.text, "Второй вариант");
        assert_eq!(q.images, ["12", "13"]);
        assert!(
            media_question(&batch[..1], "gamebot")
                .unwrap()
                .text
                .is_empty()
        );
        assert!(media_question(&batch[1..], "gamebot").is_none());
        assert!(media_question(&[photo(12, Some("/diff"), true)], "gamebot").is_none());
    }

    #[test]
    fn file_upload_uses_the_original_document() {
        let mut msg = serde_json::to_value(photo(12, Some("/prompt"), false)).unwrap();
        msg.as_object_mut().unwrap().remove("photo");
        msg.as_object_mut().unwrap().remove("media_group_id");
        msg["document"] =
            json!({"file_id":"original","file_unique_id":"png","file_name":"screen.PNG"});
        let parse = |v: &Value| serde_json::from_value::<Message>(v.clone()).unwrap();
        let q = [parse(&msg)];
        assert_eq!(media_question(&q, "gamebot").unwrap().images, ["original"]);
        msg["document"]["file_name"] = json!("note.txt");
        assert!(!is_media(parse(&msg)));
    }
}
