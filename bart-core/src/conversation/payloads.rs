use std::sync::Arc;

use crate::history::Message;

pub(super) fn share_payloads(history: &mut [Message], window: &mut [Message]) {
    use crate::history::Content;
    let mut texts = std::collections::HashSet::new();
    let mut values = std::collections::HashSet::new();
    let mut text = |payload: &mut Arc<str>| {
        if let Some(original) = texts.get(payload) {
            *payload = Arc::clone(original);
        } else {
            texts.insert(Arc::clone(payload));
        }
    };
    let mut value = |payload: &mut Arc<serde_json::Value>| {
        if let Some(original) = values.get(payload) {
            *payload = Arc::clone(original);
        } else {
            values.insert(Arc::clone(payload));
        }
    };
    for message in history.iter_mut().chain(window.iter_mut()) {
        match message {
            Message::User(payload) | Message::Instruction(payload) => text(payload),
            Message::UserSource(source) => text(&mut source.text),
            Message::Notification(note) => text(&mut note.text),
            Message::Skill(skill) => text(&mut skill.text),
            Message::ToolResult(result) => text(&mut result.output),
            Message::Assistant(assistant) => {
                for part in &mut assistant.parts {
                    if let Some(replay) = &mut part.replay {
                        value(replay);
                    }
                    match &mut part.content {
                        Content::Text(payload) | Content::Reasoning(payload) => text(payload),
                        Content::ToolCall(call) => match &mut call.arguments {
                            crate::history::Arguments::Json { value: args, wire } => {
                                value(args);
                                text(wire);
                            }
                            crate::history::Arguments::Invalid(raw) => text(raw),
                        },
                        Content::Other => {}
                    }
                }
            }
            Message::Image(_) => {}
        }
    }
}
