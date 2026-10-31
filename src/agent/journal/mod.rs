use std::{ops::Range, sync::Arc};

use crate::{
    approval::Review,
    conversation::{Conversation, Estimate, ModelContext},
    history::{Message, Notification},
    record::{Meta, PendingWrite, Recorder},
};

pub(super) const MAX_HISTORY_BYTES: usize = 128 * 1024 * 1024;

pub(crate) struct Journal {
    conversation: Conversation,
    bytes: usize,
    name: Option<String>,
    resume_notices: Range<usize>,
    unanswered: bool,
    pending: Option<PendingWrite>,
    recording: Recording,
}

enum Recording {
    Healthy(Recorder),
    Faulted {
        recorder: Option<Recorder>,
        reason: String,
        unreported: bool,
    },
    Off,
}

impl Journal {
    pub(super) fn new(recorder: Option<Recorder>) -> Self {
        Self {
            conversation: Conversation::default(),
            bytes: 0,
            name: None,
            resume_notices: 0..0,
            unanswered: false,
            pending: None,
            recording: recorder.map_or(Recording::Off, Recording::Healthy),
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = crate::history::Entry<'_>> {
        self.conversation.entries()
    }

    pub(crate) fn history(&self) -> &[Message] {
        self.conversation.history()
    }

    pub(super) fn usage(&self) -> crate::conversation::UsageSummary {
        self.conversation.usage()
    }

    pub(crate) fn model_context(&self) -> ModelContext<'_> {
        self.conversation.context()
    }

    pub(crate) fn compactions(&self) -> u64 {
        self.conversation.compactions()
    }

    pub(crate) fn reviews(&self) -> &std::collections::VecDeque<Review> {
        self.conversation.reviews()
    }

    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn recording_warning(&self) -> Option<&str> {
        match &self.recording {
            Recording::Faulted { reason, .. } => Some(reason),
            Recording::Healthy(_) | Recording::Off => None,
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn user_messages(&self) -> Vec<(usize, Arc<str>)> {
        self.history()
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                Message::User(text) => Some((index, Arc::clone(text))),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn resume_notifications(&self) -> impl Iterator<Item = &Notification> {
        let current = !self.history()[self.resume_notices.end..]
            .iter()
            .any(|message| matches!(message, Message::User(_)));
        self.history()[self.resume_notices.clone()]
            .iter()
            .take(if current { usize::MAX } else { 0 })
            .filter_map(|message| match message {
                Message::Notification(note) => Some(note),
                _ => None,
            })
    }

    fn recorder(&self) -> Option<&Recorder> {
        match &self.recording {
            Recording::Healthy(recorder) => Some(recorder),
            Recording::Faulted { .. } | Recording::Off => None,
        }
    }

    pub(super) async fn automatic_name(&self, base: &str) -> Result<String, String> {
        match self.recorder() {
            Some(recorder) => recorder
                .automatic_name(base)
                .await
                .map_err(|error| error.to_string()),
            None => Ok(base.to_owned()),
        }
    }

    pub(crate) fn take_recording_notice(&mut self) -> Option<&str> {
        match &mut self.recording {
            Recording::Faulted {
                reason, unreported, ..
            } if *unreported => {
                *unreported = false;
                Some(reason)
            }
            _ => None,
        }
    }

    pub(crate) async fn settle_recording(&mut self) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        let result = pending.as_mut().await;
        self.pending = None;
        if let Err(error) = result {
            let Recording::Healthy(recorder) =
                std::mem::replace(&mut self.recording, Recording::Off)
            else {
                unreachable!("only a healthy record has writes");
            };
            self.recording = Recording::Faulted {
                recorder: Some(recorder),
                reason: format!(
                    "recording stopped ({error}); this session will not be resumable past this point"
                ),
                unreported: true,
            };
        }
    }

    pub(super) async fn append(&mut self, messages: Vec<Message>) -> &[Message] {
        self.settle_recording().await;
        self.stage_append(messages)
    }

    fn stage_append(&mut self, messages: Vec<Message>) -> &[Message] {
        let start = self.history().len();
        let pending = if messages.is_empty() {
            None
        } else {
            self.recorder()
                .map(|recorder| recorder.append_batch(messages.clone()))
        };
        self.apply(messages);
        self.pending = pending;
        &self.history()[start..]
    }

    fn apply(&mut self, messages: Vec<Message>) {
        for message in messages {
            self.unanswered |= matches!(&message, Message::Assistant(assistant)
                if assistant.parts.iter().any(|part| matches!(part.content, crate::history::Content::ToolCall(_))));
            self.bytes = self
                .bytes
                .saturating_add(crate::history::message_len(&message));
            self.conversation.push(message);
        }
    }

    pub(super) fn answered(&mut self) {
        self.unanswered = false;
    }

    pub(super) async fn rename(&mut self, name: String) {
        self.settle_recording().await;
        let pending = self.recorder().map(|recorder| recorder.rename(&name));
        self.name = Some(name);
        self.pending = pending;
    }

    pub(super) async fn describe(&mut self, meta: &Meta) {
        self.settle_recording().await;
        self.pending = self.recorder().map(|recorder| recorder.describe(meta));
    }

    pub(crate) async fn review(&mut self, review: &Review) {
        self.settle_recording().await;
        let pending = self.recorder().map(|recorder| recorder.review(review));
        self.conversation.review(review.clone());
        self.pending = pending;
    }

    pub(super) async fn rewind(&mut self, at: usize) -> Result<Arc<str>, String> {
        self.settle_recording().await;
        let Some(Message::User(text)) = self.history().get(at) else {
            return Err("that message is no longer in the conversation".into());
        };
        let text = Arc::clone(text);
        let pending = self.recorder().map(|recorder| recorder.rewind(at));
        self.conversation.rewind(at);
        self.resume_notices = 0..0;
        self.bytes = total_bytes(self.history());
        self.pending = pending;
        Ok(text)
    }

    pub(super) async fn checkpoint(&mut self, window: Vec<Message>, estimate: Estimate) {
        self.settle_recording().await;
        let pending = self
            .recorder()
            .map(|recorder| recorder.compact(&window, estimate));
        self.conversation.compact(window, estimate);
        self.pending = pending;
    }

    pub(super) async fn recover(&mut self, output: &str) -> bool {
        self.settle_recording().await;
        if !self.unanswered {
            return false;
        }
        let (history, window) = self.conversation.recover(output);
        self.bytes = total_bytes(self.history());
        self.unanswered = false;
        if !history.is_empty() || !window.is_empty() {
            self.pending = self
                .recorder()
                .map(|recorder| recorder.repair(history, window));
            return true;
        }
        false
    }

    pub(super) async fn resume(
        &mut self,
        meta: Option<&Meta>,
        name: Option<String>,
        mut conversation: Conversation,
    ) {
        self.settle_recording().await;
        let recovered = conversation.resumed();
        self.bytes = total_bytes(conversation.history());
        self.conversation = conversation;
        self.name = name;
        let start = self.history().len();
        let pending = self.recorder().map(|recorder| {
            recorder.opening(
                meta,
                recovered.repairs,
                recovered.window_repairs,
                &recovered.notices,
            )
        });
        self.apply(
            recovered
                .notices
                .into_iter()
                .map(Message::Notification)
                .collect(),
        );
        self.resume_notices = start..self.history().len();
        self.pending = pending;
    }

    fn take_recorder(&mut self) -> Option<Recorder> {
        match &mut self.recording {
            Recording::Healthy(_) => {
                let Recording::Healthy(recorder) =
                    std::mem::replace(&mut self.recording, Recording::Off)
                else {
                    unreachable!()
                };
                Some(recorder)
            }
            Recording::Faulted { recorder, .. } => recorder.take(),
            Recording::Off => None,
        }
    }

    pub(super) async fn finish(&mut self, notifications: impl FnOnce() -> Vec<Notification>) {
        self.settle_recording().await;
        self.stage_append(
            notifications()
                .into_iter()
                .map(Message::Notification)
                .collect(),
        );
        self.settle_recording().await;
        drop(self.take_recorder());
    }

    pub(super) fn close_on_drop(&mut self, notifications: Vec<Notification>) {
        let healthy = self.recorder().is_some();
        if let Some(recorder) = self.take_recorder() {
            recorder.close(if healthy {
                notifications.clone()
            } else {
                Vec::new()
            });
        }
        self.apply(
            notifications
                .into_iter()
                .map(Message::Notification)
                .collect(),
        );
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl Journal {
    pub(crate) fn of(history: Vec<Message>) -> Self {
        let mut journal = Self::new(None);
        journal.apply(history);
        journal
    }
}

pub(super) fn total_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(crate::history::message_len)
        .fold(0, usize::saturating_add)
}
