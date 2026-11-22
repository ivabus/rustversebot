use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use futures_util::future::{AbortHandle, AbortRegistration};
use teloxide::{
    prelude::*,
    types::{MessageId, UserId},
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{LiveMessage, LiveOutput, MessageAddress};
use crate::{BotState, agent_access, bot_templates::BotTemplateSender};

#[derive(Clone, Default)]
pub(crate) struct Prompts(Arc<Mutex<HashMap<Uuid, Entry>>>);

struct Entry {
    address: MessageAddress,
    target: LiveMessage,
    author: UserId,
    abort: AbortHandle,
    stopped: oneshot::Receiver<()>,
}

pub(super) struct Registration {
    prompts: Prompts,
    pub id: Uuid,
    _lifetime: oneshot::Sender<()>,
}

impl Registration {
    // Both the delivery path and /stop must claim the same entry before acting.
    pub fn claim(&self) -> bool {
        self.prompts.0.lock().unwrap().remove(&self.id).is_some()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.claim();
    }
}

impl Prompts {
    pub(super) fn register(
        &self,
        address: MessageAddress,
        target: LiveMessage,
        author: UserId,
    ) -> (Registration, AbortHandle, AbortRegistration) {
        let id = Uuid::new_v4();
        let (abort, signal) = AbortHandle::new_pair();
        let (lifetime, stopped) = oneshot::channel();
        self.0.lock().unwrap().insert(
            id,
            Entry {
                address,
                target,
                author,
                abort: abort.clone(),
                stopped,
            },
        );
        (
            Registration {
                prompts: self.clone(),
                id,
                _lifetime: lifetime,
            },
            abort,
            signal,
        )
    }

    fn select(
        &self,
        here: MessageAddress,
        reply: Option<MessageId>,
        actor: Option<UserId>,
    ) -> Result<(Uuid, UserId), &'static str> {
        let entries = self.0.lock().unwrap();
        let mut found = entries.iter().filter(|(_, e)| {
            e.address.chat == here.chat
                && e.address.thread == here.thread
                && match reply {
                    Some(id) => {
                        e.address.message_id == id
                            || matches!(e.target, LiveMessage::Message(status) if status == id)
                    }
                    None => Some(e.author) == actor,
                }
        });
        let (id, entry) = found.next().ok_or("missing")?;
        if found.next().is_some() {
            return Err("multiple");
        }
        Ok((*id, entry.author))
    }

    fn stop(&self, id: Uuid) -> Option<Entry> {
        let entry = self.0.lock().unwrap().remove(&id)?;
        entry.abort.abort();
        Some(entry)
    }
}

async fn may_stop<F: Future<Output = Result<bool>>>(
    msg: &Message,
    author: UserId,
    bot_admin: bool,
    chat_admin: impl FnOnce(UserId) -> F,
) -> Result<bool> {
    let sender = agent_access::sender(msg);
    if sender.is_some_and(|u| u.id == author) || bot_admin {
        return Ok(true);
    }
    if !(msg.chat.is_group() || msg.chat.is_supergroup()) {
        return Ok(false);
    }
    if let Some(chat) = &msg.sender_chat {
        return Ok(chat.id == msg.chat.id);
    }
    let Some(user) = sender else { return Ok(false) };
    chat_admin(user.id).await
}

pub(crate) async fn stop(bot: &Bot, msg: &Message, state: &BotState) -> Result<()> {
    let here = MessageAddress::from(msg);
    let selected = state.prompts.select(
        here,
        msg.reply_to_message().map(|m| m.id),
        agent_access::sender(msg).map(|u| u.id),
    );
    let notice = match selected {
        Err(reason) => reason,
        Ok((id, author)) => match may_stop(
            msg,
            author,
            agent_access::is_admin(agent_access::sender(msg), state),
            |user| async move {
                Ok(bot
                    .get_chat_administrators(here.chat)
                    .await?
                    .iter()
                    .any(|m| m.user.id == user))
            },
        )
        .await
        {
            Err(_) => "unverified",
            Ok(false) => "denied",
            Ok(true) => {
                if let Some(entry) = state.prompts.stop(id) {
                    state.ai_jobs.remove(id);
                    // Wait until any local status updates and generator futures are dropped.
                    let _ = entry.stopped.await;
                    let output = LiveOutput {
                        bot,
                        templates: &state.templates,
                        address: entry.address,
                    };
                    let edit = match entry.target {
                        LiveMessage::Draft => None,
                        LiveMessage::Message(id) => Some(id),
                    };
                    output
                        .send(
                            state
                                .templates
                                .render("prompt_stop", &serde_json::json!({"result":"stopped"}))?,
                            edit,
                        )
                        .await?;
                    return Ok(());
                }
                "missing"
            }
        },
    };
    BotTemplateSender::new(bot, &state.templates)
        .send_to_thread(
            msg.chat.id,
            here.thread,
            "prompt_stop",
            &serde_json::json!({"result":notice}),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::ThreadId;

    fn address(chat: i64, thread: i32, message: i32) -> MessageAddress {
        MessageAddress {
            chat: ChatId(chat),
            thread: Some(ThreadId(MessageId(thread))),
            message_id: MessageId(message),
        }
    }

    #[test]
    fn stop_targets_only_the_selected_request_in_this_chat_and_topic() {
        let prompts = Prompts::default();
        let here = address(-100, 7, 10);
        let user = UserId(1);
        let (first, _, _) = prompts.register(here, LiveMessage::Message(MessageId(20)), user);
        let (other, _, _) = prompts.register(address(-100, 7, 11), LiveMessage::Draft, UserId(2));
        for id in [10, 20] {
            assert_eq!(
                prompts.select(here, Some(MessageId(id)), Some(UserId(2))),
                Ok((first.id, user))
            );
            for elsewhere in [address(-101, 7, 10), address(-100, 8, 10)] {
                assert!(
                    prompts
                        .select(elsewhere, Some(MessageId(id)), Some(user))
                        .is_err()
                );
            }
        }
        assert_eq!(prompts.select(here, None, Some(user)), Ok((first.id, user)));
        let (_second, _, _) = prompts.register(address(-100, 7, 12), LiveMessage::Draft, user);
        assert_eq!(prompts.select(here, None, Some(user)), Err("multiple"));
        assert_eq!(
            prompts.select(here, Some(MessageId(42)), Some(user)),
            Err("missing")
        );
        assert!(other.claim());
        assert!(prompts.stop(other.id).is_none()); // delivery already owns it
    }

    #[tokio::test]
    async fn cancelling_an_active_prompt_drops_the_generator_and_prevents_delivery() {
        let prompts = Prompts::default();
        let (guard, _, signal) = prompts.register(address(1, 1, 1), LiveMessage::Draft, UserId(1));
        let id = guard.id;
        let (started, ready) = oneshot::channel();
        let run = tokio::spawn(async move {
            let generate = async {
                started.send(()).unwrap();
                std::future::pending::<()>().await
            };
            assert!(
                futures_util::future::Abortable::new(generate, signal)
                    .await
                    .is_err()
            );
            assert!(!guard.claim());
        });
        ready.await.unwrap();
        let entry = prompts.stop(id).unwrap();
        assert!(prompts.stop(id).is_none());
        run.await.unwrap();
        assert!(entry.stopped.await.is_err());
    }

    #[tokio::test]
    async fn author_bot_admin_and_chat_admin_can_stop_but_other_members_cannot() {
        let chat = serde_json::json!({"id":-100,"type":"supergroup","title":"Test"});
        let mut msg: Message = serde_json::from_value(serde_json::json!({
            "chat":chat,"message_id":30,"date":1,"text":"/stop",
            "from":{"id":2,"first_name":"Member","is_bot":false}
        }))
        .unwrap();
        let no_lookup = |_| async { panic!("unexpected admin lookup") };
        assert!(may_stop(&msg, UserId(2), false, no_lookup).await.unwrap());
        assert!(may_stop(&msg, UserId(1), true, no_lookup).await.unwrap());
        for admin in [true, false] {
            assert_eq!(
                may_stop(&msg, UserId(1), false, |id| async move {
                    assert_eq!(id, UserId(2));
                    Ok(admin)
                })
                .await
                .unwrap(),
                admin
            );
        }
        assert!(
            may_stop(&msg, UserId(1), false, |_| async {
                anyhow::bail!("permission lookup failed")
            })
            .await
            .is_err()
        );
        msg.sender_chat = Some(msg.chat.clone());
        assert!(may_stop(&msg, UserId(1), false, no_lookup).await.unwrap());
        msg.sender_chat.as_mut().unwrap().id = ChatId(-200);
        assert!(!may_stop(&msg, UserId(2), false, no_lookup).await.unwrap());
    }
}
