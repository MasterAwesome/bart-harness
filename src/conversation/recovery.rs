use std::collections::HashSet;

use crate::{
    conversation::Repairs,
    history::{Content, Message, Notification, NotificationKind, ToolResult},
};

pub(crate) struct Recovered {
    pub(crate) repairs: Repairs,
    pub(crate) window_repairs: Repairs,
    pub(crate) notices: Vec<Notification>,
}

pub(super) const INTERRUPTED_BY_EXIT: &str =
    "interrupted when Bart exited before a result was recorded; any effects are unknown.";

pub(super) fn notices(history: &[Message]) -> Vec<Notification> {
    let finished: HashSet<u32> = history
        .iter()
        .filter_map(|message| match message {
            Message::Notification(notification) => notification.terminal_job(),
            _ => None,
        })
        .collect();
    let lost: Vec<u32> = history
        .iter()
        .flat_map(|message| match message {
            Message::ToolResult(result) => result.jobs.as_slice(),
            _ => &[],
        })
        .copied()
        .filter(|job| !finished.contains(job))
        .collect();
    let mut notices = Vec::new();
    if history
        .iter()
        .any(|message| matches!(message, Message::ToolResult(_)))
    {
        notices.push(Notification { text: bart!(const Session, "resumed in a new process; Python state is gone, files and earlier effects remain").to_owned().into(), kind: NotificationKind::Recovery });
    }
    for job in lost {
        notices.push(Notification {
            text: bart!(
                Job,
                "job[{job}] lost when Bart restarted; it may still be running"
            )
            .into(),
            kind: NotificationKind::JobLost { id: job },
        });
    }
    notices
}

pub(super) fn orphaned_calls(history: &[Message], output: &str) -> Repairs {
    let mut added = Vec::new();
    let mut index = 0;
    while index < history.len() {
        let orphans: Vec<String> = match &history[index] {
            Message::Assistant(assistant) => assistant
                .parts
                .iter()
                .filter_map(|part| match &part.content {
                    Content::ToolCall(call) => Some(call.id.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        index += 1;
        let mut answered = HashSet::new();
        while let Some(Message::ToolResult(result)) = history.get(index) {
            answered.insert(result.call_id.clone());
            index += 1;
        }
        for call_id in orphans.into_iter().filter(|id| !answered.contains(id)) {
            let answer = Message::ToolResult(ToolResult {
                activities: Default::default(),
                diffs: Vec::new(),
                notes: String::new(),
                call_id,
                output: output.to_owned().into(),
                is_error: true,
                jobs: Vec::new(),
                saved: None,
                cell: None,
            });
            added.push((index + added.len(), answer));
        }
    }
    added
}
